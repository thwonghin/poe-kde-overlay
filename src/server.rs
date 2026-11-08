use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::config_store::ConfigStore;
use crate::game::Game;
use crate::host_config::ParsedHostConfig;
use crate::ipc::{ClientEvent, HostState, MainEvent, TrackArea, UpdateInfo, UserAction};
use crate::logger::Logger;

/// `base` joined with a request-path remainder, or `None` if it would
/// escape `base` (`..`, absolute, or otherwise non-plain components).
pub(crate) fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    rel.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
        .then(|| base.join(rel))
}

/// Upstream's own upload cap (file-uploads.ts); nothing else posts more.
const MAX_BODY_BYTES: usize = 16_000_000;

type ClientId = u64;

// The socket itself is owned by its connection thread (handle_websocket),
// never shared: senders just queue text here and `notify` the thread's
// poller, which wakes it to write the queue out. Sharing the socket behind
// a Mutex let the reader starve senders — confirmed live, the GTK main
// thread blocked 4.4s in send_last_active, freezing every hotkey and
// delaying the price-check popup.
struct Client {
    tx: std::sync::mpsc::Sender<String>,
    poller: Arc<polling::Poller>,
}

/// Renderer requests the GTK main loop has to act on (it owns the
/// `Application`, window, click-through and input-injection state).
pub enum UiRequest {
    /// OVERLAY->MAIN::focus-game (the title-bar "x"): assertGameActive.
    FocusGame,
    /// CLIENT->MAIN::user-action {action:"stash-search"}.
    StashSearch(String),
    /// OVERLAY->MAIN::render-state: whether any widget wants showing.
    RenderState(bool),
    /// CLIENT->MAIN::user-action {action:"quit"} (AppTray.ts listens for
    /// the same event).
    Quit,
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
    // WS connection threads → GTK main loop, which owns the state these
    // act on. `host_config_tx` goes to shortcuts.rs's task holding the
    // GlobalShortcuts session; everything else is a `UiRequest`.
    ui_request_tx: async_channel::Sender<UiRequest>,
    host_config_tx: async_channel::Sender<ParsedHostConfig>,
    // Latest OVERLAY->MAIN::track-area (price-check popup bounds + holdKey),
    // read by shortcuts.rs's popup tracker on every poll.
    track_area: Mutex<Option<TrackArea>>,
    /// This game's Client.txt tail thread (game_log.rs); unset on the idle
    /// bus and in tests.
    game_log: OnceLock<std::sync::mpsc::Sender<crate::game_log::Command>>,
}

impl EventBus {
    pub(crate) fn new() -> (
        Self,
        async_channel::Receiver<UiRequest>,
        async_channel::Receiver<ParsedHostConfig>,
    ) {
        let (ui_request_tx, ui_request_rx) = async_channel::unbounded();
        let (host_config_tx, host_config_rx) = async_channel::unbounded();
        (
            Self {
                clients: Mutex::new(HashMap::new()),
                last_active: Mutex::new(None),
                overlay_active: std::sync::atomic::AtomicBool::new(true),
                next_id: AtomicU64::new(1),
                ui_request_tx,
                host_config_tx,
                track_area: Mutex::new(None),
                game_log: OnceLock::new(),
            },
            ui_request_rx,
            host_config_rx,
        )
    }

    fn add_client(&self, client: Client) -> ClientId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().unwrap().insert(id, Arc::new(client));
        *self.last_active.lock().unwrap() = Some(id);
        id
    }

    fn remove_client(&self, id: ClientId) {
        let mut last_active = self.last_active.lock().unwrap();
        let mut clients = self.clients.lock().unwrap();
        clients.remove(&id);
        if *last_active == Some(id) {
            *last_active = clients.keys().next().copied();
            // Mirrors real APT's server.ts: closing a client while exactly
            // one remains re-trusts it as the overlay, rather than leaving
            // overlay_active stuck on whatever the closed client reported.
            if clients.len() == 1 {
                self.set_overlay_active(true);
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
        if client.tx.send(msg.to_string()).is_ok() {
            let _ = client.poller.notify();
        }
    }

    pub fn broadcast(&self, event: &MainEvent) {
        let msg = serde_json::to_string(event).unwrap();
        let clients: Vec<_> = self.clients.lock().unwrap().values().cloned().collect();
        for client in clients {
            self.send_to(&client, &msg);
        }
    }

    pub fn send_last_active(&self, event: &MainEvent) {
        let msg = serde_json::to_string(event).unwrap();
        let target = self
            .last_active
            .lock()
            .unwrap()
            .and_then(|id| self.clients.lock().unwrap().get(&id).cloned());
        if let Some(client) = target {
            self.send_to(&client, &msg);
        }
    }

    pub(crate) fn game_log(&self, command: crate::game_log::Command) {
        if let Some(tx) = self.game_log.get() {
            let _ = tx.send(command);
        }
    }

    fn request_ui(&self, request: UiRequest) {
        let _ = self.ui_request_tx.try_send(request);
    }
}

/// One game's renderer backend: its own port, event bus, dist/ and data
/// dir, so the two games' clients and configs never mix. `logger` is shared
/// across games (main.rs), so it gets this bus attached here.
pub struct Server {
    pub game: Game,
    pub port: u16,
    pub events: Arc<EventBus>,
    pub ui_request_rx: async_channel::Receiver<UiRequest>,
    pub host_config_rx: async_channel::Receiver<ParsedHostConfig>,
}

pub fn spawn(game: Game, logger: Arc<Logger>) -> std::io::Result<Server> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    let (event_bus, ui_request_rx, host_config_rx) = EventBus::new();
    let events = Arc::new(event_bus);
    started_at_ms();
    logger.attach(events.clone());
    let _ = events.game_log.set(crate::game_log::spawn(game, events.clone(), logger.clone()));
    let server = Server {
        game,
        port,
        events: events.clone(),
        ui_request_rx,
        host_config_rx,
    };
    let config = Arc::new(ConfigStore::new(crate::config_store::config_path(game)));
    let dist: &'static Path = Box::leak(game.dist_dir().into_boxed_path());

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let events = events.clone();
                    let config = config.clone();
                    let logger = logger.clone();
                    std::thread::spawn(move || {
                        if let Err(err) = handle_connection(game, dist, stream, &events, &config, &logger) {
                            eprintln!("[server] connection error: {err}");
                        }
                    });
                }
                Err(err) => eprintln!("[server] accept error: {err}"),
            }
        }
    });

    Ok(server)
}

fn handle_connection(
    game: Game,
    dist: &Path,
    stream: TcpStream,
    events: &Arc<EventBus>,
    config: &Arc<ConfigStore>,
    logger: &Arc<Logger>,
) -> std::io::Result<()> {
    // A client that connects and stalls would otherwise hold this thread
    // forever. The WebSocket path replaces this with its own short timeout.
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut peek_buf = [0u8; 1024];
    let n = stream.peek(&mut peek_buf)?;
    let first_line_end = peek_buf[..n].iter().position(|&b| b == b'\r').unwrap_or(n);
    let first_line = &peek_buf[..first_line_end];

    if first_line.starts_with(b"GET /events") {
        return handle_websocket(game, stream, events, config, logger);
    }

    handle_http(game, dist, stream, config, logger)
}

fn handle_websocket(
    game: Game,
    stream: TcpStream,
    events: &Arc<EventBus>,
    config: &Arc<ConfigStore>,
    logger: &Arc<Logger>,
) -> std::io::Result<()> {
    // WebSockets aren't covered by CORS, so without this any web page that
    // finds our port could drive the overlay (save-config, quit, typing
    // into PoE via stash-search). The renderer always connects same-origin
    // (IPC.ts: `${location.host}/events`); a missing Origin is a non-browser
    // client, which the page-level threat doesn't cover anyway.
    #[allow(clippy::result_large_err)] // tungstenite's callback signature
    let same_origin = |req: &tungstenite::handshake::server::Request,
                       resp: tungstenite::handshake::server::Response| {
        let header = |name| req.headers().get(name).and_then(|v| v.to_str().ok());
        match header("origin") {
            None => Ok(resp),
            Some(origin) if header("host").is_some_and(|host| origin == format!("http://{host}")) => {
                Ok(resp)
            }
            Some(origin) => {
                eprintln!("[server] rejected websocket from origin {origin}");
                let mut deny = tungstenite::handshake::server::ErrorResponse::new(None);
                *deny.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
                Err(deny)
            }
        }
    };
    let mut ws = match tungstenite::accept_hdr(stream, same_origin) {
        Ok(ws) => ws,
        Err(err) => {
            eprintln!("[server] websocket handshake failed: {err}");
            return Ok(());
        }
    };

    // Sleeps until the socket is readable (or, with output still buffered,
    // writable) or a sender `notify`s — no idle wakeups, and queued messages
    // go out immediately. Non-blocking so tungstenite returns WouldBlock
    // instead of stalling; it keeps partial frames buffered and resumes.
    ws.get_ref().set_nonblocking(true)?;
    let poller = Arc::new(polling::Poller::new()?);
    // SAFETY: deleted from the poller below, before `ws` (the socket) drops.
    unsafe { poller.add(ws.get_ref(), polling::Event::readable(0))? };

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let id = events.add_client(Client {
        tx,
        poller: poller.clone(),
    });
    println!("[server] websocket client {id} connected");
    events.send_last_active(&MainEvent::LogEntry { message: logger.history() });

    let would_block = |err: &tungstenite::Error| {
        matches!(err, tungstenite::Error::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    };
    let mut events_buf = polling::Events::new();
    'conn: loop {
        loop {
            match ws.read() {
                Ok(tungstenite::Message::Text(text)) => {
                    handle_client_event(game, &text, id, events, config, logger);
                }
                Ok(tungstenite::Message::Close(_)) => break 'conn,
                Ok(_) => {}
                Err(err) if would_block(&err) => break,
                Err(_) => break 'conn,
            }
        }
        while let Ok(msg) = rx.try_recv() {
            match ws.write(tungstenite::Message::Text(msg.into())) {
                Ok(()) => {}
                // Kept in tungstenite's write buffer; the flush below retries.
                Err(err) if would_block(&err) => {}
                Err(_) => break 'conn,
            }
        }
        // Whether tungstenite still holds output because the socket is full.
        let unflushed = match ws.flush() {
            Ok(()) => false,
            Err(err) if would_block(&err) => true,
            Err(_) => break 'conn,
        };

        let interest = if unflushed {
            polling::Event::all(0)
        } else {
            polling::Event::readable(0)
        };
        // Oneshot mode: interest is re-armed before every wait.
        if poller.modify(ws.get_ref(), interest).is_err() {
            break;
        }
        events_buf.clear();
        if poller.wait(&mut events_buf, None).is_err() {
            break;
        }
    }

    let _ = poller.delete(ws.get_ref());
    events.remove_client(id);
    println!("[server] websocket client {id} disconnected");
    Ok(())
}

fn handle_client_event(
    game: Game,
    text: &str,
    from: ClientId,
    events: &Arc<EventBus>,
    config: &Arc<ConfigStore>,
    logger: &Arc<Logger>,
) {
    // Upstream's IpcEvent is checked to only contain events ClientEvent
    // parses (ipc.rs), so failing here means a malformed message.
    let event = match serde_json::from_str::<ClientEvent>(text) {
        Ok(event) => event,
        Err(err) => {
            eprintln!("[server] malformed client event ({err}): {text}");
            return;
        }
    };

    match event {
        ClientEvent::UsedRecently { is_overlay } => {
            events.mark_active(from);
            events.set_overlay_active(is_overlay);
        }
        ClientEvent::TrackArea(area) => {
            debug!("[server] track-area: {area:?}");
            *events.track_area.lock().unwrap() = Some(area);
        }
        ClientEvent::FocusGame => events.request_ui(UiRequest::FocusGame),
        ClientEvent::UpdateHostConfig(config) => {
            events.game_log(crate::game_log::Command::Restart {
                path: config.client_log.clone().unwrap_or_default(),
                read: config.read_client_log.unwrap_or(true),
            });
            let parsed = crate::host_config::parse_host_config(config, logger);
            println!(
                "[server] update-host-config received, {} shortcut(s) after filtering, restore_clipboard={}",
                parsed.shortcuts.len(),
                parsed.restore_clipboard
            );
            crate::kwin_tracker::set_window_title(game, &parsed.window_title);
            let _ = events.host_config_tx.try_send(parsed);
        }
        ClientEvent::UserAction(UserAction::Quit) => events.request_ui(UiRequest::Quit),
        ClientEvent::UserAction(UserAction::StashSearch { text }) => {
            events.request_ui(UiRequest::StashSearch(text));
        }
        ClientEvent::UserAction(
            UserAction::CheckForUpdate | UserAction::UpdateAndRestart | UserAction::Unknown,
        ) => {}
        // ponytail: EE2 also flushes its FileWriter client-log channel here;
        // write-data isn't implemented, so there's nothing to flush.
        ClientEvent::ReparseLog => events.game_log(crate::game_log::Command::Reparse),
        ClientEvent::RenderState { should_show } => events.request_ui(UiRequest::RenderState(should_show)),
        ClientEvent::WriteData(_) => {}
        ClientEvent::SaveConfig { contents, is_temporary } => {
            config.save(&contents, is_temporary, logger);
            events.broadcast(&MainEvent::ConfigChanged { contents });
        }
    }
}

fn handle_http(
    game: Game,
    dist: &Path,
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

    let mut stream = stream;
    if content_length > MAX_BODY_BYTES {
        return write_response(&mut stream, 413, "text/plain", b"body too large");
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    // The proxy forwards the query string; local files never have one.
    let file_path = path.split('?').next().unwrap_or("");
    if path == "/config" {
        let body = serde_json::to_string(&HostState {
            contents: config.load(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            updater: UpdateInfo::UpdateNotAvailable { checked_at: started_at_ms() },
        })
        .unwrap();
        write_response(&mut stream, 200, "application/json", body.as_bytes())?;
        return Ok(());
    }

    if path.starts_with("/uploads/") {
        return crate::uploads::handle(game, &mut stream, &method, file_path, &body);
    }

    if path.starts_with("/proxy/") {
        return crate::proxy::handle(&mut stream, &method, &path, &headers, &body, logger);
    }

    serve_static(dist, &mut stream, file_path)
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

fn serve_static(dist: &Path, stream: &mut TcpStream, path: &str) -> std::io::Result<()> {
    let path = if path == "/" { "/index.html" } else { path };
    let Some(file_path) = safe_join(dist, path.trim_start_matches('/')) else {
        return write_response(stream, 404, "text/plain", b"not found");
    };

    match std::fs::read(&file_path) {
        Ok(bytes) => {
            let content_type = match file_path.extension().and_then(|e| e.to_str()) {
                Some("html") => Some("text/html"),
                Some("js") => Some("text/javascript"),
                Some("json") => Some("application/json"),
                Some("css") => Some("text/css"),
                Some("svg") => Some("image/svg+xml"),
                Some("png") => Some("image/png"),
                Some("woff2") => Some("font/woff2"),
                Some("wasm") => Some("application/wasm"),
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
        413 => "Payload Too Large",
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
    use serde_json::{Value, json};

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

    fn raw_request(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    #[test]
    fn server_rejects_traversal_oversized_bodies_and_cross_origin_websockets() {
        let port = spawn(Game::Poe1, Arc::new(Logger::new())).unwrap().port;
        let get = |path: &str| raw_request(port, &format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert!(get("/../../../../etc/passwd").starts_with("HTTP/1.1 404"));
        assert!(get("/uploads/../../../../etc/passwd").starts_with("HTTP/1.1 404"));
        let big = raw_request(port, "POST /uploads/a.png HTTP/1.1\r\nContent-Length: 999999999999\r\n\r\n");
        assert!(big.starts_with("HTTP/1.1 413"), "{big}");
        // Proxy allowlist is exact-host: no lookalikes, no userinfo tricks
        // (all rejected before any network request).
        for path in ["/proxy/evil.example/x", "/proxy/poe.ninja.evil.example/x", "/proxy/poe.ninja@evil.example/x"] {
            assert!(get(path).starts_with("HTTP/1.1 403"), "{path}");
        }

        let ws = |origin: &str| {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let host = format!("127.0.0.1:{port}");
            write!(
                stream,
                "GET /events HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            line
        };
        assert!(ws("https://evil.example").starts_with("HTTP/1.1 403"));
        assert!(ws(&format!("http://127.0.0.1:{port}")).starts_with("HTTP/1.1 101"));
    }

    #[test]
    fn websocket_delivers_both_ways_without_polling_delay() {
        use tungstenite::client::IntoClientRequest;

        let server = spawn(Game::Poe1, Arc::new(Logger::new())).unwrap();
        let port = server.port;
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let request = format!("ws://127.0.0.1:{port}/events").into_client_request().unwrap();
        let (mut ws, _) = tungstenite::client(request, stream).unwrap();
        let mut next_text = || loop {
            if let tungstenite::Message::Text(text) = ws.read().unwrap() {
                break serde_json::from_str::<Value>(&text).unwrap();
            }
        };

        // Replayed log history is the first thing a new client gets.
        assert_eq!(next_text()["name"], "MAIN->CLIENT::log-entry");

        // Server → client, immediately (was up to one 15ms read timeout).
        let started = std::time::Instant::now();
        server.events.broadcast(&MainEvent::WidgetAction { target: "t".into() });
        assert_eq!(next_text()["payload"]["target"], "t");
        assert!(started.elapsed() < Duration::from_millis(500));

        // Bigger than any socket buffer: exercises the WouldBlock/flush path.
        let big = "x".repeat(8 * 1024 * 1024);
        server.events.broadcast(&MainEvent::LogEntry { message: big.clone() });
        server.events.broadcast(&MainEvent::OverlayAttached);
        assert_eq!(next_text()["payload"]["message"].as_str().unwrap().len(), big.len());
        assert_eq!(next_text()["name"], "MAIN->OVERLAY::overlay-attached");

        // Client → server.
        ws.send(tungstenite::Message::Text(
            json!({ "name": "CLIENT->MAIN::user-action", "payload": { "action": "quit" } })
                .to_string()
                .into(),
        ))
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let request = loop {
            match server.ui_request_rx.try_recv() {
                Ok(request) => break request,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(err) => panic!("no UiRequest: {err}"),
            }
        };
        assert!(matches!(request, UiRequest::Quit));
    }

    #[test]
    fn last_active_routing_and_retrust_on_close() {
        let (bus, _ui, _cfg) = EventBus::new();
        let mut inboxes = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..3 {
            let (tx, rx) = std::sync::mpsc::channel();
            ids.push(bus.add_client(Client { tx, poller: Arc::new(polling::Poller::new().unwrap()) }));
            inboxes.push(rx);
        }
        let received = |inboxes: &[std::sync::mpsc::Receiver<String>]| -> Vec<usize> {
            (0..inboxes.len()).filter(|&i| inboxes[i].try_recv().is_ok()).collect()
        };

        // A newly connected client is last-active; used-recently moves it.
        bus.send_last_active(&MainEvent::OverlayAttached);
        assert_eq!(received(&inboxes), [2]);
        bus.mark_active(ids[0]);
        bus.send_last_active(&MainEvent::OverlayAttached);
        assert_eq!(received(&inboxes), [0]);
        bus.broadcast(&MainEvent::OverlayAttached);
        assert_eq!(received(&inboxes), [0, 1, 2]);

        // A browser tab reported isOverlay:false.
        bus.set_overlay_active(false);
        // Closing a client that isn't last-active changes nothing.
        bus.remove_client(ids[2]);
        assert!(!bus.overlay_was_used_recently());
        bus.send_last_active(&MainEvent::OverlayAttached);
        assert_eq!(received(&inboxes), [0]);

        // Closing the last-active one leaves exactly one: it inherits
        // last-active and is re-trusted as the overlay (server.ts).
        bus.remove_client(ids[0]);
        assert!(bus.overlay_was_used_recently());
        bus.send_last_active(&MainEvent::OverlayAttached);
        assert_eq!(received(&inboxes), [1]);
    }

    // update-host-config is left out: it sets kwin_tracker's global window
    // title, which would race that module's own test.
    #[test]
    fn client_events_reach_their_handlers() {
        let (bus, ui_rx, _cfg) = EventBus::new();
        let events = Arc::new(bus);
        let logger = Arc::new(Logger::new());
        logger.attach(events.clone());
        let dir = std::env::temp_dir().join(format!("poe-client-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Arc::new(ConfigStore::new(dir.join("config.json")));
        let (tx, inbox) = std::sync::mpsc::channel();
        let id = events.add_client(Client { tx, poller: Arc::new(polling::Poller::new().unwrap()) });
        let send = |event: Value| handle_client_event(Game::Poe1, &event.to_string(), id, &events, &config, &logger);

        // save-config persists and echoes the contents to every client.
        send(json!({ "name": "CLIENT->MAIN::save-config", "payload": { "contents": "{\"a\":1}", "isTemporary": false } }));
        assert_eq!(config.load().as_deref(), Some("{\"a\":1}"));
        let echoed: Value = serde_json::from_str(&inbox.try_recv().unwrap()).unwrap();
        assert_eq!(echoed, json!({ "name": "MAIN->CLIENT::config-changed", "payload": { "contents": "{\"a\":1}" } }));
        send(json!({ "name": "CLIENT->MAIN::save-config", "payload": { "contents": "t", "isTemporary": true } }));
        assert!(dir.join("config.json.tmp").exists());

        // used-recently carries whether the sender is the overlay.
        send(json!({ "name": "CLIENT->MAIN::used-recently", "payload": { "isOverlay": false } }));
        assert!(!events.overlay_was_used_recently());
        send(json!({ "name": "CLIENT->MAIN::used-recently", "payload": { "isOverlay": true } }));
        assert!(events.overlay_was_used_recently());

        // track-area is stored; a malformed one is dropped, not stored.
        send(json!({ "name": "OVERLAY->MAIN::track-area", "payload": {
            "holdKey": "Alt", "closeThreshold": 40, "from": { "x": 0, "y": 0 },
            "area": { "x": 1, "y": 2, "width": 3, "height": 4 }, "dpr": 1 } }));
        assert_eq!(events.track_area().unwrap().hold_key, "Alt");
        events.clear_track_area();
        send(json!({ "name": "OVERLAY->MAIN::track-area", "payload": { "holdKey": "Alt" } }));
        assert!(events.track_area().is_none());

        // UI requests go to the GTK loop; unknown/garbage input is ignored.
        send(json!({ "name": "OVERLAY->MAIN::focus-game" }));
        send(json!({ "name": "CLIENT->MAIN::user-action", "payload": { "action": "stash-search", "text": "chaos" } }));
        send(json!({ "name": "CLIENT->MAIN::user-action", "payload": { "action": "check-for-update" } }));
        send(json!({ "name": "CLIENT->MAIN::nonsense" }));
        handle_client_event(Game::Poe1, "not json", id, &events, &config, &logger);
        assert!(matches!(ui_rx.try_recv(), Ok(UiRequest::FocusGame)));
        assert!(matches!(ui_rx.try_recv(), Ok(UiRequest::StashSearch(t)) if t == "chaos"));
        assert!(ui_rx.try_recv().is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn safe_join_rejects_escapes() {
        let base = Path::new("/srv/dist");
        assert_eq!(safe_join(base, "assets/a.js"), Some(PathBuf::from("/srv/dist/assets/a.js")));
        assert_eq!(safe_join(base, "../../etc/passwd"), None);
        assert_eq!(safe_join(base, "assets/../../x"), None);
        assert_eq!(safe_join(base, "/etc/passwd"), None);
        assert_eq!(safe_join(base, "./a"), None);
    }
}
