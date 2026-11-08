//! GameLogWatcher.ts, per game: tails the game's Client.txt and broadcasts
//! new lines as MAIN->CLIENT::game-log (both renderers' OverlayWindow.vue
//! parse them: chat/zone events, poe2's leveling widget). Driven by
//! update-host-config's clientLog and poe2's readClientLog, and poe2's
//! CLIENT->MAIN::re-parse-log. An empty clientLog is found from the
//! focused game's own open file descriptors first (any Wine prefix or
//! Steam library), then upstream's fixed guesses.
//!
//! Differences from upstream, all because we run as a long-lived daemon:
//! a file that isn't there yet (game never launched) is retried every
//! poll instead of only on the next config change; a deleted/recreated
//! Client.txt is reopened; only whole lines are sent (upstream splits a
//! line the game is mid-way through writing in two); and re-enabling
//! readClientLog with an unchanged path reopens the file (upstream keeps
//! its old `_wantedPath` and silently stays off).

use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::game::Game;
use crate::ipc::MainEvent;
use crate::logger::Logger;
use crate::server::EventBus;

/// watchFile's `interval` upstream.
const POLL: Duration = Duration::from_millis(450);
/// Upstream's read buffer: one game-log event per chunk.
const CHUNK: usize = 64 * 1024;

pub enum Command {
    /// `path` is clientLog ("" = guess); `read` is poe2's readClientLog
    /// (poe1 has no such option and always reads).
    Restart { path: String, read: bool },
    /// Resend the whole file from the start.
    Reparse,
    /// The focused game window's pid (kwin_tracker), to find its log.
    Pid(i64),
}

pub fn spawn(game: Game, events: Arc<EventBus>, logger: Arc<Logger>) -> mpsc::Sender<Command> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut watcher = Watcher::new(game, logger);
        loop {
            match rx.recv_timeout(POLL) {
                Ok(Command::Restart { path, read }) => watcher.restart(path, read),
                Ok(Command::Reparse) => watcher.reparse(),
                Ok(Command::Pid(pid)) => watcher.pid = Some(pid),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            watcher.poll(|lines| events.broadcast(&MainEvent::GameLog { lines }));
        }
    });
    tx
}

struct Open {
    path: PathBuf,
    file: File,
    offset: u64,
}

struct Watcher {
    game: Game,
    /// The configured clientLog; `None` while reading is off.
    wanted: Option<String>,
    open: Option<Open>,
    logger: Arc<Logger>,
    pid: Option<i64>,
}

impl Watcher {
    fn new(game: Game, logger: Arc<Logger>) -> Self {
        Self { game, wanted: None, open: None, logger, pid: None }
    }

    /// stdout + the in-app Log tab.
    fn info(&self, msg: &str) {
        let line = format!("info [GameLogWatcher {}] {msg}", self.game.name());
        println!("{line}");
        self.logger.write(&line);
    }

    fn restart(&mut self, path: String, read: bool) {
        if !read {
            if self.wanted.take().is_some() {
                self.info("disabled");
            }
            self.open = None;
            return;
        }
        if self.wanted.as_ref() == Some(&path) {
            return;
        }
        self.wanted = Some(path);
        self.open = None;
        self.try_open(false);
    }

    fn reparse(&mut self) {
        if let Some(open) = &mut self.open {
            open.offset = 0;
        }
    }

    /// Opens the wanted (or guessed) file at its end and says which one.
    /// Failures are `quiet` on the per-poll retry, logged on a config change.
    fn try_open(&mut self, quiet: bool) {
        let Some(wanted) = self.wanted.clone() else { return };
        let game = self.game;
        let log = |msg: String| {
            if !quiet {
                self.logger.error(&format!("error [GameLogWatcher {}] {msg}", game.name()));
            }
        };
        let path = if let Some(path) = wanted.is_empty().then(|| self.pid.and_then(open_log_of)).flatten() {
            path
        } else if wanted.is_empty() {
            let found: Vec<PathBuf> = candidates(game).into_iter().filter(|p| File::open(p).is_ok()).collect();
            match found.as_slice() {
                [one] => one.clone(),
                [] => return log("No log file found".into()),
                // Standalone + Steam: don't guess between them.
                many => {
                    let list: Vec<String> = many.iter().map(|p| p.display().to_string()).collect();
                    return log(format!("Found {} log files, please enter one into settings: {}", many.len(), list.join(", ")));
                }
            }
        } else {
            PathBuf::from(wanted)
        };
        match File::open(&path).and_then(|file| Ok((file.metadata()?.len(), file))) {
            Ok((offset, file)) => {
                self.info(&format!("watching {}", path.display()));
                self.open = Some(Open { path, file, offset });
            }
            Err(err) => log(format!("Failed to watch file {}: {err}", path.display())),
        }
    }

    /// Emits every whole line written since the last poll, one call per
    /// read chunk like upstream.
    fn poll(&mut self, mut emit: impl FnMut(Vec<String>)) {
        if self.open.is_none() {
            self.try_open(true);
        }
        let Some(open) = &mut self.open else { return };
        // Deleted and recreated (the game writes a fresh one): it's all new.
        let same_file = match (std::fs::metadata(&open.path), open.file.metadata()) {
            (Ok(on_disk), Ok(ours)) => on_disk.ino() == ours.ino() && on_disk.dev() == ours.dev(),
            _ => false,
        };
        if !same_file {
            match File::open(&open.path) {
                Ok(file) => *open = Open { path: open.path.clone(), file, offset: 0 },
                Err(_) => return,
            }
        }
        if open.file.metadata().is_ok_and(|m| m.len() < open.offset) {
            open.offset = 0; // truncated
        }
        let mut buf = vec![0; CHUNK];
        loop {
            let n = match open.file.read_at(&mut buf, open.offset) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let end = match buf[..n].iter().rposition(|&b| b == b'\n') {
                Some(i) => i + 1,
                None if n == CHUNK => n, // one >64KB line: don't stall on it
                None => return,          // the game is mid-line
            };
            open.offset += end as u64;
            let lines: Vec<String> = String::from_utf8_lossy(&buf[..end])
                .split('\n')
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect();
            if !lines.is_empty() {
                emit(lines);
            }
        }
    }
}

/// The `logs/Client.txt` process `pid` holds open (PoE keeps it open while
/// running).
/// ponytail: only the window's own pid; walk its children if KWin ever
/// reports a launcher's pid instead.
fn open_log_of(pid: i64) -> Option<PathBuf> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .ok()?
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .find(|target| target.ends_with("logs/Client.txt"))
}

/// Each vendor's Linux POSSIBLE_PATH, relative to $HOME.
fn relative_candidates(game: Game) -> [String; 2] {
    let dir = match game {
        Game::Poe1 => "Path of Exile",
        Game::Poe2 => "Path of Exile 2",
    };
    [
        format!(".wine/drive_c/Program Files (x86)/Grinding Gear Games/{dir}/logs/Client.txt"),
        format!(".local/share/Steam/steamapps/common/{dir}/logs/Client.txt"),
    ]
}

fn candidates(game: Game) -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    relative_candidates(game).iter().map(|rel| PathBuf::from(&home).join(rel)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn guessed_paths_match_each_vendor() {
        let sources = crate::game::vendor_sources("main/src/host-files/GameLogWatcher.ts");
        for (game, source) in Game::ALL.iter().zip(&sources) {
            for rel in relative_candidates(*game) {
                assert!(source.contains(&format!("'{rel}'")) || source.contains(&format!("\"{rel}\"")), "{rel}");
            }
        }
    }

    #[test]
    fn finds_the_log_a_process_holds_open() {
        let dir = std::env::temp_dir().join(format!("poe-game-log-fd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let path = dir.join("logs/Client.txt");
        let _held = File::create(&path).unwrap();
        assert_eq!(open_log_of(std::process::id().into()), Some(path));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tails_whole_lines_and_follows_reparse_disable_and_recreate() {
        let dir = std::env::temp_dir().join(format!("poe-game-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Client.txt");
        std::fs::write(&path, "old\n").unwrap();
        let append = |s: &str| std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(s.as_bytes()).unwrap();
        let logger = Arc::new(Logger::new());
        let mut w = Watcher::new(Game::Poe2, logger.clone());
        let poll = |w: &mut Watcher| {
            let mut got = Vec::new();
            w.poll(|lines| got.extend(lines));
            got
        };

        // Starts at the end; a half-written line waits for its newline.
        w.restart(path.display().to_string(), true);
        assert!(poll(&mut w).is_empty());
        assert!(logger.history().contains(&format!("watching {}", path.display())));
        append("a\r\nb");
        assert_eq!(poll(&mut w), ["a"]);
        append("\n\n");
        assert_eq!(poll(&mut w), ["b"]);

        w.reparse();
        assert_eq!(poll(&mut w), ["old", "a", "b"]);

        // Off, then back on with the same path: reopened at the end.
        w.restart(path.display().to_string(), false);
        append("skipped\n");
        assert!(poll(&mut w).is_empty());
        w.restart(path.display().to_string(), true);
        assert!(poll(&mut w).is_empty());

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "fresh\n").unwrap();
        assert_eq!(poll(&mut w), ["fresh"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
