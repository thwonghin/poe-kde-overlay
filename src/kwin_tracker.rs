use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use zbus::interface;

const PLUGIN_NAME: &str = "apt-kwin-overlay-tracker";
const CALLBACK_IFACE: &str = "io.github.thwonghin.AptKwinOverlay.Callback";
const CALLBACK_PATH: &str = "/";
const CURSOR_QUERY_IFACE: &str = "io.github.thwonghin.AptKwinOverlay.CursorPosCallback";

/// The renderer's `windowTitle` setting (CLIENT->MAIN::update-host-config),
/// lowercased + trimmed. Real APT attaches by this title alone; we match it
/// in addition to the built-in PoE 1/2 captions below. Empty until the
/// renderer sends its config.
static WINDOW_TITLE: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

pub fn set_window_title(title: &str) {
    *WINDOW_TITLE.write().unwrap() = title.trim().to_ascii_lowercase();
}

fn configured_title() -> String {
    WINDOW_TITLE.read().unwrap().clone()
}

#[derive(Debug, Clone, Deserialize)]
pub struct WindowEvent {
    // Not read yet — kept for later matching a specific tracked window
    // (e.g. the real PoE window) across repeated geometry events.
    #[allow(dead_code)]
    pub id: String,
    #[serde(rename = "className")]
    pub class_name: String,
    pub caption: String,
    pub pid: i64,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl WindowEvent {
    /// Steam surfaces a numeric resourceClass (e.g. "steam_app_3083953976")
    /// that tells us nothing on this setup, so the caption is the only
    /// usable signal — but it must be an exact match, not "contains": a
    /// browser tab titled "Empower Support - Trade - Path of Exile -
    /// Helium" contains the substring too, and matching loosely made the
    /// overlay follow the browser around instead of the game.
    pub fn is_path_of_exile(&self) -> bool {
        let class = self.class_name.to_ascii_lowercase();
        let caption = self.caption.trim().to_ascii_lowercase();
        let title = configured_title();
        class.contains("pathofexile")
            || caption == "path of exile"
            || caption == "path of exile 2"
            || (!title.is_empty() && caption == title)
    }

    /// True when this activation event is our own overlay surface, not some
    /// other real app. Confirmed live: KDE's KeyboardMode::OnDemand grants
    /// our layer-shell surface a genuine window-activation event the moment
    /// it becomes interactive (click-through off, e.g. opening a locked
    /// price-check popup) -- not only on an explicit click. Callers that
    /// track "what's the last externally-focused app" (main.rs's
    /// `active_window`, used by shortcuts.rs::price_check to avoid injecting
    /// Ctrl+C into whatever app happens to be focused) must ignore these
    /// self-activations, or a locked popup opening would immediately look
    /// indistinguishable from "the user alt-tabbed away from the game."
    pub fn is_own_process(&self) -> bool {
        self.pid as u32 == std::process::id()
    }
}

#[derive(Debug)]
pub enum TrackerEvent {
    Activated(WindowEvent),
    GeometryChanged(WindowEvent),
}

struct Callback {
    sender: async_channel::Sender<TrackerEvent>,
}

#[interface(name = "io.github.thwonghin.AptKwinOverlay.Callback")]
impl Callback {
    async fn activated(&self, json: String) {
        match serde_json::from_str::<WindowEvent>(&json) {
            Ok(event) => {
                let _ = self.sender.send(TrackerEvent::Activated(event)).await;
            }
            Err(err) => eprintln!("[kwin_tracker] bad activated payload: {err}"),
        }
    }

    #[allow(non_snake_case)]
    async fn geometryChanged(&self, json: String) {
        match serde_json::from_str::<WindowEvent>(&json) {
            Ok(event) => {
                let _ = self.sender.send(TrackerEvent::GeometryChanged(event)).await;
            }
            Err(err) => eprintln!("[kwin_tracker] bad geometryChanged payload: {err}"),
        }
    }
}

fn tracker_script(dbus_addr: &str) -> String {
    format!(
        r#"
function emit(kind, w) {{
    callDBus("{dbus_addr}", "{path}", "{iface}", kind, JSON.stringify({{
        id: w.internalId.toString(),
        className: w.resourceClass,
        caption: w.caption,
        pid: w.pid,
        x: w.frameGeometry.x,
        y: w.frameGeometry.y,
        width: w.frameGeometry.width,
        height: w.frameGeometry.height
    }}));
}}
function hook(w) {{
    w.frameGeometryChanged.connect(function () {{ emit("GeometryChanged", w); }});
}}
workspace.windowList().forEach(hook);
workspace.windowAdded.connect(hook);
workspace.windowActivated.connect(function (w) {{ if (w) emit("Activated", w); }});
if (workspace.activeWindow) emit("Activated", workspace.activeWindow);
"#,
        dbus_addr = dbus_addr,
        path = CALLBACK_PATH,
        iface = CALLBACK_IFACE,
    )
}

/// Spawns a dedicated thread running its own zbus connection: registers our
/// callback interface, loads a persistent KWin script that hooks window
/// activation/geometry signals, and forwards every event to the GTK main
/// loop via `sender`. The KWin script and this connection are meant to live
/// for the whole process lifetime (never `stop()`/`unloadScript`'d).
///
/// Also hands back the `zbus::Connection` itself (once established) via the
/// returned receiver, so other code (e.g. an on-demand `query_cursor_pos`
/// call from the GTK main thread) can reuse the same connection — it's a
/// cheap, `Clone`, thread-safe handle whose `async-io` reactor runs
/// independently of whoever awaits calls on it, so this doesn't require
/// touching this dedicated thread's setup.
pub fn spawn(sender: async_channel::Sender<TrackerEvent>) -> async_channel::Receiver<zbus::Connection> {
    let (conn_tx, conn_rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        if let Err(err) = async_io::block_on(run(sender, conn_tx)) {
            eprintln!("[kwin_tracker] error: {err}");
        }
    });
    conn_rx
}

async fn run(
    sender: async_channel::Sender<TrackerEvent>,
    conn_tx: async_channel::Sender<zbus::Connection>,
) -> zbus::Result<()> {
    let connection = zbus::connection::Builder::session()?
        .serve_at(CALLBACK_PATH, Callback { sender })?
        .build()
        .await?;
    let _ = conn_tx.send(connection.clone()).await;

    let dbus_addr = connection.unique_name().expect("connection has a unique name").to_string();

    let script_path = std::env::temp_dir().join(format!("{PLUGIN_NAME}.js"));
    std::fs::write(&script_path, tracker_script(&dbus_addr))?;

    let scripting = zbus::Proxy::new(
        &connection,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .await?;

    let already_loaded: bool = scripting.call("isScriptLoaded", &(PLUGIN_NAME,)).await?;
    if already_loaded {
        let _: bool = scripting.call("unloadScript", &(PLUGIN_NAME,)).await?;
    }

    let script_id: i32 = scripting
        .call(
            "loadScript",
            &(script_path.to_string_lossy().to_string(), PLUGIN_NAME),
        )
        .await?;

    let script = zbus::Proxy::new(
        &connection,
        "org.kde.KWin",
        format!("/Scripting/Script{script_id}"),
        "org.kde.kwin.Script",
    )
    .await?;
    script.call_method("run", &()).await?;

    println!("[kwin_tracker] persistent script loaded (id {script_id}), watching for events");

    // Keep this thread (and the connection/object-server it owns) alive for
    // the process lifetime.
    std::future::pending::<()>().await;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct CursorPos {
    x: f64,
    y: f64,
}

struct CursorPosCallback {
    sender: async_channel::Sender<(f64, f64)>,
}

#[interface(name = "io.github.thwonghin.AptKwinOverlay.CursorPosCallback")]
impl CursorPosCallback {
    async fn cursor_pos(&self, json: String) {
        match serde_json::from_str::<CursorPos>(&json) {
            Ok(pos) => {
                let _ = self.sender.send((pos.x, pos.y)).await;
            }
            Err(err) => eprintln!("[kwin_tracker] bad cursor pos payload: {err}"),
        }
    }
}

/// One-shot query for the current global cursor position, via the same
/// KWin-Scripting mechanism as the resident tracker, but load/run/unload a
/// tiny disposable script instead of a persistent one. Each call uses a
/// fresh path + plugin name so it can't collide with the resident tracker
/// script or with an overlapping call.
pub async fn query_cursor_pos(connection: &zbus::Connection) -> zbus::Result<(f64, f64)> {
    static QUERY_COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = QUERY_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = format!("/query{id}");
    let plugin_name = format!("apt-kwin-overlay-cursor-{id}");

    let (tx, rx) = async_channel::bounded(1);
    connection
        .object_server()
        .at(path.as_str(), CursorPosCallback { sender: tx })
        .await?;

    let dbus_addr = connection.unique_name().expect("connection has a unique name").to_string();
    let script = format!(
        r#"callDBus("{dbus_addr}", "{path}", "{iface}", "CursorPos", JSON.stringify({{x: workspace.cursorPos.x, y: workspace.cursorPos.y}}));"#,
        dbus_addr = dbus_addr,
        path = path,
        iface = CURSOR_QUERY_IFACE,
    );

    let script_path = std::env::temp_dir().join(format!("{plugin_name}.js"));
    std::fs::write(&script_path, &script)?;

    let scripting = zbus::Proxy::new(
        connection,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .await?;
    let script_id: i32 = scripting
        .call(
            "loadScript",
            &(script_path.to_string_lossy().to_string(), plugin_name.as_str()),
        )
        .await?;

    let script_obj = zbus::Proxy::new(
        connection,
        "org.kde.KWin",
        format!("/Scripting/Script{script_id}"),
        "org.kde.kwin.Script",
    )
    .await?;
    script_obj.call_method("run", &()).await?;

    let result = rx
        .recv()
        .await
        .map_err(|_| zbus::Error::Failure("cursor pos query channel closed".to_string()));

    let _: bool = scripting.call("unloadScript", &(plugin_name.as_str(),)).await?;
    let _ = connection
        .object_server()
        .remove::<CursorPosCallback, _>(path.as_str())
        .await;
    let _ = std::fs::remove_file(&script_path);

    result
}

/// Explicitly re-activates the PoE window via KWin scripting. Needed because
/// restoring click-through doesn't hand real OS focus back to the game on
/// its own: the click a user makes to dismiss an interactive popup is
/// consumed entirely by our own overlay surface (that's how the renderer
/// knows to ask us to restore click-through in the first place), so it never
/// reaches PoE the way a real click-through would. Confirmed live: without
/// this, PoE stays unfocused after closing a popup until the user happens to
/// click the game again separately -- our own injected Ctrl+C lands nowhere
/// useful in the meantime, and WebKitGTK's own webview can end up claiming
/// clipboard ownership instead.
///
/// Same disposable load/run/unload pattern as `query_cursor_pos`, but
/// fire-and-forget (no callback/object-server registration needed since
/// there's no return value to wait for).
pub async fn activate_path_of_exile(connection: &zbus::Connection) -> zbus::Result<()> {
    static ACTIVATE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = ACTIVATE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let plugin_name = format!("apt-kwin-overlay-activate-{id}");

    // Mirrors WindowEvent::is_path_of_exile()'s matching -- keep in sync.
    let script = format!(
        "var configuredTitle = {};\n{}",
        serde_json::Value::String(configured_title()),
        r#"
var candidates = workspace.windowList().filter(function (w) {
    var className = (w.resourceClass || "").toLowerCase();
    var caption = (w.caption || "").trim().toLowerCase();
    return className.indexOf("pathofexile") !== -1 || caption === "path of exile" || caption === "path of exile 2"
        || (configuredTitle !== "" && caption === configuredTitle);
});
if (candidates.length > 0) {
    workspace.activeWindow = candidates[0];
}
"#
    );

    let script_path = std::env::temp_dir().join(format!("{plugin_name}.js"));
    std::fs::write(&script_path, script)?;

    let scripting = zbus::Proxy::new(
        connection,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .await?;
    let script_id: i32 = scripting
        .call(
            "loadScript",
            &(script_path.to_string_lossy().to_string(), plugin_name.as_str()),
        )
        .await?;

    let script_obj = zbus::Proxy::new(
        connection,
        "org.kde.KWin",
        format!("/Scripting/Script{script_id}"),
        "org.kde.kwin.Script",
    )
    .await?;
    script_obj.call_method("run", &()).await?;

    let _: bool = scripting.call("unloadScript", &(plugin_name.as_str(),)).await?;
    let _ = std::fs::remove_file(&script_path);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(caption: &str) -> WindowEvent {
        WindowEvent {
            id: String::new(),
            class_name: "steam_app_1".to_string(),
            caption: caption.to_string(),
            pid: 0,
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        }
    }

    #[test]
    fn matches_configured_window_title_in_addition_to_builtins() {
        assert!(!window("PoE Custom").is_path_of_exile());
        set_window_title("  PoE Custom ");
        assert!(window("poe custom").is_path_of_exile());
        assert!(window("Path of Exile").is_path_of_exile());
        assert!(!window("Trade - Path of Exile - Browser").is_path_of_exile());
        set_window_title("");
    }
}
