//! poe2's FileWriter.ts, behind EE2's experimental "Item Data Collection"
//! (libraryAlpha): CLIENT->MAIN::write-data appends the library widget's
//! sessions to `<name>.csv` (header on creation, one row per log-item
//! copy) and parsed Client.txt events to `client-log.ndjson`, in
//! libraryOutputPath or the game's data dir. One per game bus; written
//! from the WebSocket threads, never the GTK one.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::game::Game;
use crate::ipc::WriteData;
use crate::logger::Logger;

#[derive(Default)]
pub struct Library(Mutex<State>);

#[derive(Default)]
struct State {
    enabled: bool,
    dir: PathBuf,
    item: Option<File>,
    client_log: Option<File>,
    last_ts: f64,
}

impl Library {
    /// update-host-config's libraryAlpha / libraryOutputPath.
    pub fn restart(&self, game: Game, enabled: bool, output_path: Option<String>) {
        let mut state = self.0.lock().unwrap();
        state.enabled = enabled;
        // upstream: userData/apt-data/csv-data
        state.dir = output_path
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| game.data_dir().join("csv-data"));
    }

    /// re-parse-log: the ndjson is rewritten from the start.
    pub fn flush_client_log(&self, logger: &Logger) {
        let mut state = self.0.lock().unwrap();
        if state.client_log.take().is_some() {
            logger.write("info [FileWriter] Channel client-log closed.");
        }
    }

    pub fn handle(&self, data: WriteData, logger: &Logger) {
        let mut state = self.0.lock().unwrap();
        if !state.enabled {
            return;
        }
        match data {
            WriteData::ClientLogEvent { data, close } => {
                if state.client_log.is_none() {
                    let path = state.dir.join("client-log.ndjson");
                    match create_dir_and(&state.dir, || File::create(&path)) {
                        Ok(file) => state.client_log = Some(file),
                        Err(err) => return logger.write(&format!("error [FileWriter] {}: {err}", path.display())),
                    }
                }
                let ts = data.get("ts").and_then(|ts| ts.as_f64()).unwrap_or(0.0);
                if ts < state.last_ts {
                    logger.write(&format!("warn [FileWriter] time just went backwards: {ts} < {}", state.last_ts));
                }
                state.last_ts = ts;
                write_line(&mut state.client_log, &data.to_string(), "client-log", logger);
                if close {
                    state.client_log = None;
                    logger.write("info [FileWriter] Channel client-log closed.");
                }
            }
            WriteData::LogItem { text } => write_line(&mut state.item, &text, "item", logger),
            WriteData::Session { start: true, name: Some(name), header: Some(header) } if !name.is_empty() => {
                // The name is typed into the widget: keep it a plain file
                // name inside the output dir.
                let Some(path) = crate::server::safe_join(&state.dir, &format!("{name}.csv")) else {
                    return logger.write("error [FileWriter] Invalid session name.");
                };
                let existed = path.exists();
                let opened = create_dir_and(&state.dir, || OpenOptions::new().append(true).create(true).open(&path));
                match opened {
                    Ok(file) => {
                        state.item = Some(file);
                        if !existed {
                            write_line(&mut state.item, &header, "item", logger);
                        }
                    }
                    Err(_) => logger.write("error [FileWriter] Failed to create session file."),
                }
            }
            WriteData::Session { start: true, .. } => logger.write("error [FileWriter] Invalid session start event."),
            WriteData::Session { start: false, .. } => {
                if state.item.take().is_some() {
                    logger.write("info [FileWriter] Channel item closed.");
                } else {
                    logger.write("error [FileWriter] Channel item already closed.");
                }
            }
            WriteData::Unknown => {}
        }
    }
}

fn create_dir_and(dir: &Path, open: impl FnOnce() -> std::io::Result<File>) -> std::io::Result<File> {
    std::fs::create_dir_all(dir)?;
    open()
}

fn write_line(file: &mut Option<File>, line: &str, channel: &str, logger: &Logger) {
    let Some(f) = file else {
        return logger.write(&format!("error [FileWriter] Unable to write to channel {channel}, channel closed."));
    };
    if let Err(err) = writeln!(f, "{line}") {
        logger.write(&format!("error [FileWriter] channel {channel}: {err}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn writes_sessions_and_client_log_like_file_writer() {
        let dir = std::env::temp_dir().join(format!("poe-library-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logger = Logger::new();
        let lib = Library::default();
        let send = |v: serde_json::Value| lib.handle(serde_json::from_value(v).unwrap(), &logger);
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();

        // Off: nothing written.
        lib.restart(Game::Poe2, false, Some(dir.display().to_string()));
        send(json!({ "action": "session", "start": true, "name": "s", "header": "h" }));
        assert!(!dir.exists());

        lib.restart(Game::Poe2, true, Some(dir.display().to_string()));
        send(json!({ "action": "session", "start": true, "name": "s", "header": "h" }));
        send(json!({ "action": "log-item", "text": "row1" }));
        send(json!({ "action": "session", "start": false }));
        send(json!({ "action": "log-item", "text": "dropped" }));
        // Reopened: appended, no second header.
        send(json!({ "action": "session", "start": true, "name": "s", "header": "h" }));
        send(json!({ "action": "log-item", "text": "row2" }));
        assert_eq!(read("s.csv"), "h\nrow1\nrow2\n");

        send(json!({ "action": "session", "start": true, "name": "../escape", "header": "h" }));
        assert!(!dir.parent().unwrap().join("escape.csv").exists());

        // ndjson: truncated when (re)opened, e.g. after re-parse-log.
        send(json!({ "action": "client-log-event", "data": { "ts": 1 }, "close": false }));
        lib.flush_client_log(&logger);
        send(json!({ "action": "client-log-event", "data": { "ts": 2 }, "close": false }));
        assert_eq!(read("client-log.ndjson"), "{\"ts\":2}\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
