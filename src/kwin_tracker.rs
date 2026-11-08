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

// The one definition of "is this PoE": used by `is_path_of_exile` here and
// generated into the activate script's JS by `poe_matcher_js`.
const POE_CLASS_SUBSTRING: &str = "pathofexile";
const POE_CAPTIONS: &[&str] = &["path of exile", "path of exile 2"];

fn is_path_of_exile(class_name: &str, caption: &str) -> bool {
    let class = class_name.to_ascii_lowercase();
    let caption = caption.trim().to_ascii_lowercase();
    let title = configured_title();
    class.contains(POE_CLASS_SUBSTRING)
        || POE_CAPTIONS.contains(&caption.as_str())
        || (!title.is_empty() && caption == title)
}

/// `is_path_of_exile` as a JS function `isPoe(w)` over a KWin window,
/// with the current configured title baked in.
fn poe_matcher_js() -> String {
    format!(
        r#"function isPoe(w) {{
    var className = (w.resourceClass || "").toLowerCase();
    var caption = (w.caption || "").trim().toLowerCase();
    var title = {title};
    return className.indexOf({class}) !== -1 || {captions}.indexOf(caption) !== -1
        || (title !== "" && caption === title);
}}
"#,
        title = serde_json::Value::String(configured_title()),
        class = serde_json::Value::String(POE_CLASS_SUBSTRING.to_string()),
        captions = serde_json::json!(POE_CAPTIONS),
    )
}

#[derive(Debug, Clone, Deserialize)]
pub struct WindowEvent {
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
        is_path_of_exile(&self.class_name, &self.caption)
    }

    /// Our own overlay surface: KWin activates it whenever it becomes
    /// keyboard-interactive (KeyboardMode::OnDemand), not only on a click.
    /// Anything tracking the last externally focused app must skip these,
    /// or opening a locked popup looks like alt-tabbing away from PoE.
    pub fn is_own_process(&self) -> bool {
        self.pid as u32 == std::process::id()
    }
}

#[derive(Debug)]
pub enum TrackerEvent {
    Activated(WindowEvent),
    /// The window moved to another output (monitor).
    OutputChanged(WindowEvent),
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

    async fn output_changed(&self, json: String) {
        match serde_json::from_str::<WindowEvent>(&json) {
            Ok(event) => {
                let _ = self.sender.send(TrackerEvent::OutputChanged(event)).await;
            }
            Err(err) => eprintln!("[kwin_tracker] bad outputChanged payload: {err}"),
        }
    }
}

fn tracker_script(dbus_addr: &str) -> String {
    format!(
        r#"
function emit(kind, w) {{
    callDBus("{dbus_addr}", "{path}", "{iface}", kind, JSON.stringify({{
        className: w.resourceClass,
        caption: w.caption,
        pid: w.pid,
        x: w.frameGeometry.x,
        y: w.frameGeometry.y,
        width: w.frameGeometry.width,
        height: w.frameGeometry.height
    }}));
}}
// outputChanged, not frameGeometryChanged: the latter fires on every frame
// of every window's move/resize, and only a monitor change matters to us.
// Guarded so a KWin without it still gets activation tracking.
function hook(w) {{
    if (w.outputChanged) w.outputChanged.connect(function () {{ emit("OutputChanged", w); }});
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

    let scripting = scripting(&connection).await?;
    let already_loaded: bool = scripting.call("isScriptLoaded", &(PLUGIN_NAME,)).await?;
    if already_loaded {
        let _: bool = scripting.call("unloadScript", &(PLUGIN_NAME,)).await?;
    }
    let script_id = run_script(&connection, PLUGIN_NAME, &tracker_script(&dbus_addr)).await?;

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
    #[serde(rename = "className")]
    class_name: String,
    caption: String,
}

/// `(x, y, poe_is_active)` — the active-window check rides along on the
/// same KWin round trip so price-check can verify focus right before
/// injecting Ctrl+C, instead of trusting the async tracker's cached
/// activation (which can lag a fast Alt+Tab and send Ctrl+C into e.g. a
/// terminal).
struct CursorPosCallback {
    sender: async_channel::Sender<(f64, f64, bool)>,
}

#[interface(name = "io.github.thwonghin.AptKwinOverlay.CursorPosCallback")]
impl CursorPosCallback {
    async fn cursor_pos(&self, json: String) {
        match serde_json::from_str::<CursorPos>(&json) {
            Ok(pos) => {
                let poe = is_path_of_exile(&pos.class_name, &pos.caption);
                let _ = self.sender.send((pos.x, pos.y, poe)).await;
            }
            Err(err) => eprintln!("[kwin_tracker] bad cursor pos payload: {err}"),
        }
    }
}

/// Bound on each disposable-script KWin round trip below: these are
/// awaited inline on the GTK main thread (hotkey loop, focus tracker), where
/// a hung KWin call would otherwise freeze every later hotkey.
const SCRIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

async fn scripting(connection: &zbus::Connection) -> zbus::Result<zbus::Proxy<'static>> {
    zbus::Proxy::new(connection, "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting").await
}

fn script_path(plugin_name: &str) -> std::path::PathBuf {
    crate::xdg::runtime_dir().join(format!("{plugin_name}.js"))
}

/// Writes `source` to a script file, loads it into KWin as `plugin_name`
/// and runs it. Returns KWin's script id.
async fn run_script(connection: &zbus::Connection, plugin_name: &str, source: &str) -> zbus::Result<i32> {
    let path = script_path(plugin_name);
    std::fs::write(&path, source)?;
    let script_id: i32 = scripting(connection)
        .await?
        .call("loadScript", &(path.to_string_lossy().to_string(), plugin_name))
        .await?;
    let script = zbus::Proxy::new(
        connection,
        "org.kde.KWin",
        format!("/Scripting/Script{script_id}"),
        "org.kde.kwin.Script",
    )
    .await?;
    script.call_method("run", &()).await?;
    Ok(script_id)
}

/// Unloads a disposable script and deletes its file. Runs even when
/// `run_script` failed or timed out partway, so nothing is left loaded in
/// KWin or on disk; unloading a never-loaded name is a harmless no-op.
async fn cleanup_script(connection: &zbus::Connection, plugin_name: &str) {
    let unload = async {
        let _: bool = scripting(connection).await?.call("unloadScript", &(plugin_name,)).await?;
        zbus::Result::Ok(())
    };
    if let Some(Err(err)) = crate::with_timeout(SCRIPT_TIMEOUT, unload).await {
        eprintln!("[kwin_tracker] failed to unload {plugin_name}: {err}");
    }
    let _ = std::fs::remove_file(script_path(plugin_name));
}

fn timed_out(what: &str) -> zbus::Error {
    zbus::Error::Failure(format!("{what} timed out (KWin never replied)"))
}

/// One-shot query for the current global cursor position, via the same
/// KWin-Scripting mechanism as the resident tracker, but load/run/unload a
/// tiny disposable script instead of a persistent one. Each call uses a
/// fresh path + plugin name so it can't collide with the resident tracker
/// script or with an overlapping call.
pub async fn query_cursor_pos(connection: &zbus::Connection) -> zbus::Result<(f64, f64, bool)> {
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
        r#"var w = workspace.activeWindow; callDBus("{dbus_addr}", "{path}", "{iface}", "CursorPos", JSON.stringify({{x: workspace.cursorPos.x, y: workspace.cursorPos.y, className: w ? (w.resourceClass || "") : "", caption: w ? (w.caption || "") : ""}}));"#,
        iface = CURSOR_QUERY_IFACE,
    );

    let work = async {
        run_script(connection, &plugin_name, &script).await?;
        rx.recv()
            .await
            .map_err(|_| zbus::Error::Failure("cursor pos query channel closed".to_string()))
    };
    let result = crate::with_timeout(SCRIPT_TIMEOUT, work)
        .await
        .unwrap_or_else(|| Err(timed_out("cursor pos query")));

    cleanup_script(connection, &plugin_name).await;
    let _ = connection
        .object_server()
        .remove::<CursorPosCallback, _>(path.as_str())
        .await;

    result
}

/// Explicitly re-activates the PoE window via KWin scripting. Needed because
/// restoring click-through doesn't hand real OS focus back to the game on
/// its own: the click a user makes to dismiss an interactive popup is
/// consumed entirely by our own overlay surface, so it never reaches PoE.
/// Without this, PoE stays unfocused after closing a popup until the user
/// clicks the game again -- our injected Ctrl+C lands nowhere useful in the
/// meantime.
pub async fn activate_path_of_exile(connection: &zbus::Connection) -> zbus::Result<()> {
    static ACTIVATE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = ACTIVATE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let plugin_name = format!("apt-kwin-overlay-activate-{id}");

    let script = format!(
        "{}var candidates = workspace.windowList().filter(isPoe);\n\
         if (candidates.length > 0) workspace.activeWindow = candidates[0];\n",
        poe_matcher_js()
    );

    let result = crate::with_timeout(SCRIPT_TIMEOUT, run_script(connection, &plugin_name, &script))
        .await
        .unwrap_or_else(|| Err(timed_out("activate script")));
    cleanup_script(connection, &plugin_name).await;
    result.map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(caption: &str) -> WindowEvent {
        WindowEvent {
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
    fn poe_matcher_js_embeds_the_shared_rule() {
        let js = poe_matcher_js();
        assert!(js.contains(r#""pathofexile""#));
        assert!(js.contains(r#"["path of exile","path of exile 2"]"#));
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
