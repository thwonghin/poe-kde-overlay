use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::config_store::ConfigStore;
use crate::host_config::ShortcutAction;
use crate::logger::Logger;

// `cargo run` gets APT_KWIN_OVERLAY_DIST for free from .cargo/config.toml's
// [env] table (set relative to the repo root); anything else needs it
// exported explicitly, otherwise this falls back to where the PKGBUILD
// installs it.
pub(crate) fn dist_dir() -> &'static Path {
    static DIST_DIR: OnceLock<PathBuf> = OnceLock::new();
    DIST_DIR.get_or_init(|| {
        std::env::var("APT_KWIN_OVERLAY_DIST")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/usr/share/apt-kwin-overlay/dist"))
    })
}

type ClientId = u64;

// The socket itself is owned by its connection thread (handle_websocket),
// never shared: senders just queue text here and that thread writes it out
// between reads. Sharing the socket behind a Mutex let the reader (which
// re-locks immediately after each 15ms read timeout) starve senders —
// confirmed live, the GTK main thread blocked 4.4s in send_last_active,
// freezing every hotkey and delaying the price-check popup.
struct Client {
    tx: std::sync::mpsc::Sender<String>,
}

/// `ipc/types.ts`'s `IpcTrackArea` payload, in the overlay WebView's CSS
/// pixels (`area` is relative to the overlay window; `from` is the position
/// we sent with item-text, i.e. global).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackArea {
    pub hold_key: String,
    pub area: Rect,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x > self.x && x < self.x + self.width && y > self.y && y < self.y + self.height
    }
}

pub struct EventBus {
    clients: Mutex<HashMap<ClientId, Arc<Client>>>,
    last_active: Mutex<Option<ClientId>>,
    // Mirrors real APT's OverlayWindow.ts `wasUsedRecently` (default true):
    // true when the most recent CLIENT->MAIN::used-recently event's
    // isOverlay flag said so. The renderer sends that flag itself
    // (isOverlay: Host.isElectron), and our overlay WebView is the only
    // client whose user agent is spoofed to include "Electron" (main.rs) —
    // so a plain browser tab hitting the HTTP server always reports
    // isOverlay:false. Lets price_check (shortcuts.rs) avoid stealing
    // click-through away from the game on behalf of a locked hotkey whose
    // item-text payload actually went to a browser tab, not the overlay.
    overlay_active: std::sync::atomic::AtomicBool,
    next_id: AtomicU64,
    // The renderer's own title-bar "x" button sends OVERLAY->MAIN::focus-game
    // (real APT's `assertGameActive` handler) instead of just hiding itself
    // locally — it expects the host to actually restore click-through/close
    // our widgets in response. This channel carries that request from the WS
    // connection's own thread over to the GTK main loop, which owns the
    // window/click-through state needed to act on it.
    focus_game_tx: async_channel::Sender<()>,
    // Latest OVERLAY->MAIN::track-area (price-check popup bounds + holdKey),
    // read by shortcuts.rs's popup tracker on every poll.
    track_area: Mutex<Option<TrackArea>>,
    // Carries a freshly parsed CLIENT->MAIN::update-host-config shortcut
    // list from the WS connection's own thread over to the GTK main loop's
    // shortcuts.rs task, which owns the ashpd GlobalShortcuts session that
    // needs to react to it. Same shape as focus_game_tx/rx above.
    shortcut_config_tx: async_channel::Sender<Vec<ShortcutAction>>,
    // Carries the same update-host-config event's restoreClipboard flag,
    // over to shortcuts.rs so price-check can pass it into
    // RemoteInput::copy_item_text. Same shape as the two channels
    // above, kept separate since it's consumed independently (doesn't touch
    // apply_shortcuts's GlobalShortcuts bookkeeping).
    restore_clipboard_tx: async_channel::Sender<bool>,
    // Carries a CLIENT->MAIN::user-action {action:"quit"} request (the
    // renderer's own quit button, real APT's AppTray.ts listens for the
    // same event) from the WS connection's own thread over to the GTK main
    // loop, which owns the `Application` needed to call `app.quit()`. Same
    // shape as the three channels above.
    quit_tx: async_channel::Sender<()>,
}

/// `(bus, focus_game_rx, shortcut_config_rx, restore_clipboard_rx, quit_rx)`
type EventBusChannels = (
    EventBus,
    async_channel::Receiver<()>,
    async_channel::Receiver<Vec<ShortcutAction>>,
    async_channel::Receiver<bool>,
    async_channel::Receiver<()>,
);

impl EventBus {
    pub(crate) fn new() -> EventBusChannels {
        let (focus_game_tx, focus_game_rx) = async_channel::unbounded();
        let (shortcut_config_tx, shortcut_config_rx) = async_channel::unbounded();
        let (restore_clipboard_tx, restore_clipboard_rx) = async_channel::unbounded();
        let (quit_tx, quit_rx) = async_channel::unbounded();
        (
            Self {
                clients: Mutex::new(HashMap::new()),
                last_active: Mutex::new(None),
                overlay_active: std::sync::atomic::AtomicBool::new(true),
                next_id: AtomicU64::new(1),
                focus_game_tx,
                track_area: Mutex::new(None),
                shortcut_config_tx,
                restore_clipboard_tx,
                quit_tx,
            },
            focus_game_rx,
            shortcut_config_rx,
            restore_clipboard_rx,
            quit_rx,
        )
    }

    fn add_client(&self, tx: std::sync::mpsc::Sender<String>) -> ClientId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.clients
            .lock()
            .unwrap()
            .insert(id, Arc::new(Client { tx }));
        *self.last_active.lock().unwrap() = Some(id);
        id
    }

    fn remove_client(&self, id: ClientId) {
        self.clients.lock().unwrap().remove(&id);
        let mut last_active = self.last_active.lock().unwrap();
        if *last_active == Some(id) {
            let remaining = self.clients.lock().unwrap();
            *last_active = remaining.keys().next().copied();
            if let Some(sole_id) = *last_active {
                if remaining.len() == 1 {
                    drop(remaining);
                    drop(last_active);
                    self.mark_active(sole_id);
                    // Mirrors real APT's server.ts: closing a client while
                    // exactly one remains re-trusts it as the overlay,
                    // rather than leaving overlay_active stuck on whatever
                    // the closed client last reported.
                    self.set_overlay_active(true);
                    return;
                }
            }
        }
    }

    pub fn track_area(&self) -> Option<TrackArea> {
        self.track_area.lock().unwrap().clone()
    }

    /// Dropped before each new item-text so the tracker never acts on the
    /// previous popup's bounds while the renderer's new track-area is in
    /// flight.
    pub fn clear_track_area(&self) {
        *self.track_area.lock().unwrap() = None;
    }

    fn mark_active(&self, id: ClientId) {
        *self.last_active.lock().unwrap() = Some(id);
    }

    fn set_overlay_active(&self, active: bool) {
        self.overlay_active.store(active, Ordering::Relaxed);
    }

    /// Whether the overlay WebView itself (as opposed to a plain browser
    /// tab hitting `Open in Browser`) is the last client to report
    /// `CLIENT->MAIN::used-recently`. See `overlay_active`'s field comment.
    pub fn overlay_was_used_recently(&self) -> bool {
        self.overlay_active.load(Ordering::Relaxed)
    }

    fn send_to(&self, client: &Arc<Client>, msg: &str) {
        // Never blocks; a send error just means the connection thread is gone.
        let _ = client.tx.send(msg.to_string());
    }

    pub fn broadcast(&self, name: &str, payload: Value) {
        let msg = json!({ "name": name, "payload": payload }).to_string();
        let clients: Vec<_> = self.clients.lock().unwrap().values().cloned().collect();
        for client in clients {
            self.send_to(&client, &msg);
        }
    }

    pub fn send_last_active(&self, name: &str, payload: Value) {
        let msg = json!({ "name": name, "payload": payload }).to_string();
        let target = self
            .last_active
            .lock()
            .unwrap()
            .and_then(|id| self.clients.lock().unwrap().get(&id).cloned());
        if let Some(client) = target {
            self.send_to(&client, &msg);
        }
    }

    fn request_focus_game(&self) {
        let _ = self.focus_game_tx.try_send(());
    }

    fn request_shortcut_update(&self, actions: Vec<ShortcutAction>) {
        let _ = self.shortcut_config_tx.try_send(actions);
    }

    fn request_restore_clipboard_update(&self, value: bool) {
        let _ = self.restore_clipboard_tx.try_send(value);
    }

    fn request_quit(&self) {
        let _ = self.quit_tx.try_send(());
    }
}

pub struct Server {
    pub events: Arc<EventBus>,
    pub config: Arc<ConfigStore>,
    pub logger: Arc<Logger>,
    pub focus_game_rx: async_channel::Receiver<()>,
    pub shortcut_config_rx: async_channel::Receiver<Vec<ShortcutAction>>,
    pub restore_clipboard_rx: async_channel::Receiver<bool>,
    pub quit_rx: async_channel::Receiver<()>,
}

pub fn spawn() -> std::io::Result<(u16, Server)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    let (event_bus, focus_game_rx, shortcut_config_rx, restore_clipboard_rx, quit_rx) =
        EventBus::new();
    let events = Arc::new(event_bus);
    started_at_ms();
    let server = Server {
        events: events.clone(),
        config: Arc::new(ConfigStore::new()),
        logger: Arc::new(Logger::new(events.clone())),
        focus_game_rx,
        shortcut_config_rx,
        restore_clipboard_rx,
        quit_rx,
    };
    let config = server.config.clone();
    let logger = server.logger.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let events = events.clone();
                    let config = config.clone();
                    let logger = logger.clone();
                    std::thread::spawn(move || {
                        if let Err(err) = handle_connection(stream, &events, &config, &logger) {
                            eprintln!("[server] connection error: {err}");
                        }
                    });
                }
                Err(err) => eprintln!("[server] accept error: {err}"),
            }
        }
    });

    Ok((port, server))
}

fn handle_connection(
    stream: TcpStream,
    events: &Arc<EventBus>,
    config: &Arc<ConfigStore>,
    logger: &Arc<Logger>,
) -> std::io::Result<()> {
    let mut peek_buf = [0u8; 1024];
    let n = stream.peek(&mut peek_buf)?;
    let first_line_end = peek_buf[..n].iter().position(|&b| b == b'\r').unwrap_or(n);
    let first_line = &peek_buf[..first_line_end];

    if first_line.starts_with(b"GET /events") {
        return handle_websocket(stream, events, config, logger);
    }

    handle_http(stream, config, logger)
}

fn handle_websocket(
    stream: TcpStream,
    events: &Arc<EventBus>,
    config: &Arc<ConfigStore>,
    logger: &Arc<Logger>,
) -> std::io::Result<()> {
    // Bounds how long a queued outgoing message waits for the current
    // blocking read to time out before this thread gets to write it.
    stream.set_read_timeout(Some(Duration::from_millis(15)))?;
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(err) => {
            eprintln!("[server] websocket handshake failed: {err}");
            return Ok(());
        }
    };

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let id = events.add_client(tx);
    println!("[server] websocket client {id} connected");
    events.send_last_active(
        "MAIN->CLIENT::log-entry",
        json!({ "message": logger.history() }),
    );

    'conn: loop {
        match ws.read() {
            Ok(tungstenite::Message::Text(text)) => {
                handle_client_event(&text, id, events, config, logger);
            }
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref io_err))
                if io_err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        while let Ok(msg) = rx.try_recv() {
            if ws.send(tungstenite::Message::Text(msg.into())).is_err() {
                break 'conn;
            }
        }
    }

    events.remove_client(id);
    println!("[server] websocket client {id} disconnected");
    Ok(())
}

fn handle_client_event(
    text: &str,
    from: ClientId,
    events: &Arc<EventBus>,
    config: &Arc<ConfigStore>,
    logger: &Arc<Logger>,
) {
    let Ok(event) = serde_json::from_str::<Value>(text) else {
        eprintln!("[server] malformed client event: {text}");
        return;
    };
    let Some(name) = event.get("name").and_then(Value::as_str) else {
        return;
    };
    let payload = event.get("payload").cloned().unwrap_or(Value::Null);

    match name {
        "CLIENT->MAIN::used-recently" => {
            events.mark_active(from);
            let is_overlay = payload
                .get("isOverlay")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            events.set_overlay_active(is_overlay);
        }
        "OVERLAY->MAIN::track-area" => match serde_json::from_value::<TrackArea>(payload) {
            Ok(area) => {
                println!("[server] track-area: {area:?}");
                *events.track_area.lock().unwrap() = Some(area);
            }
            Err(err) => eprintln!("[server] malformed track-area: {err}"),
        },
        "OVERLAY->MAIN::focus-game" => {
            events.request_focus_game();
        }
        "CLIENT->MAIN::update-host-config" => {
            let parsed = crate::host_config::parse_host_config(&payload, logger);
            println!(
                "[server] update-host-config received, {} shortcut(s) after filtering, restore_clipboard={}",
                parsed.shortcuts.len(),
                parsed.restore_clipboard
            );
            events.request_shortcut_update(parsed.shortcuts);
            events.request_restore_clipboard_update(parsed.restore_clipboard);
            crate::kwin_tracker::set_window_title(&parsed.window_title);
        }
        "CLIENT->MAIN::user-action" => {
            if payload.get("action").and_then(Value::as_str) == Some("quit") {
                events.request_quit();
            }
        }
        "CLIENT->MAIN::save-config" => {
            let contents = payload
                .get("contents")
                .and_then(Value::as_str)
                .unwrap_or("");
            let is_temporary = payload
                .get("isTemporary")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            config.save(contents, is_temporary, logger);
            events.broadcast(
                "MAIN->CLIENT::config-changed",
                json!({ "contents": contents }),
            );
        }
        _ => {}
    }
}

fn handle_http(
    stream: TcpStream,
    config: &Arc<ConfigStore>,
    logger: &Arc<Logger>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    let mut stream = stream;
    if path == "/config" {
        let body = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "updater": { "state": "update-not-available", "checkedAt": started_at_ms() },
            "contents": config.load(),
        })
        .to_string();
        write_response(&mut stream, 200, "application/json", body.as_bytes())?;
        return Ok(());
    }

    if path.starts_with("/uploads/") {
        return crate::uploads::handle(&mut stream, &method, &path, &body);
    }

    if path.starts_with("/proxy/") {
        return crate::proxy::handle(&mut stream, &method, &path, &headers, &body, logger);
    }

    serve_static(&mut stream, &path)
}

/// Process start time, reported as the updater's `checkedAt` — `0` made the
/// About page render "last checked" as 1970.
fn started_at_ms() -> u64 {
    static STARTED_AT: OnceLock<u64> = OnceLock::new();
    *STARTED_AT.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    })
}

fn serve_static(stream: &mut TcpStream, path: &str) -> std::io::Result<()> {
    let path = if path == "/" { "/index.html" } else { path };
    let file_path: PathBuf = dist_dir().join(path.trim_start_matches('/'));

    match std::fs::read(&file_path) {
        Ok(bytes) => {
            let content_type = match file_path.extension().and_then(|e| e.to_str()) {
                Some("html") => Some("text/html"),
                Some("js") => Some("text/javascript"),
                Some("json") => Some("application/json"),
                Some("svg") => Some("image/svg+xml"),
                _ => None,
            };
            write_response(stream, 200, content_type.unwrap_or(""), &bytes)
        }
        Err(_) => write_response(stream, 404, "text/plain", b"not found"),
    }
}

pub(crate) fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write_response_with_headers(stream, status, content_type, &[], body)
}

pub(crate) fn write_response_with_headers(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !content_type.is_empty() {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    for (key, value) in extra_headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_area_parses_renderer_payload() {
        let payload = json!({
            "holdKey": "Ctrl",
            "closeThreshold": 40,
            "from": { "x": 2400, "y": 1100 },
            "area": { "x": 1500, "y": 0, "width": 460, "height": 1440 },
            "dpr": 1
        });
        let t: TrackArea = serde_json::from_value(payload).unwrap();
        assert_eq!(t.hold_key, "Ctrl");
        assert!(t.area.contains(1600.0, 700.0));
        assert!(!t.area.contains(1400.0, 700.0));
        assert!(!t.area.contains(1600.0, 1440.0));
    }

    #[test]
    fn dist_dir_falls_back_to_system_path_without_override() {
        // .cargo/config.toml sets this for `cargo run`/`cargo test`; clear
        // it to exercise the no-override path a packaged install hits.
        // SAFETY: single-threaded test, no other thread reads env vars here.
        unsafe { std::env::remove_var("APT_KWIN_OVERLAY_DIST") };
        assert_eq!(dist_dir(), Path::new("/usr/share/apt-kwin-overlay/dist"));
    }
}
