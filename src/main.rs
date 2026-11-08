/// `println!` only when `POE_KDE_OVERLAY_DEBUG` is set: per-event chatter
/// (window activations, hotkey dispatch, popup tracking). Lifecycle
/// messages and errors stay on unconditionally.
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::debug_enabled() {
            println!($($arg)*);
        }
    };
}

pub(crate) fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("POE_KDE_OVERLAY_DEBUG").is_some())
}

mod config_store;
mod game;
mod game_config;
mod game_log;
mod host_config;
mod ipc;
mod library;
mod kwin_tracker;
mod logger;
mod overlay_visibility;
mod proxy;
mod remote_input;
mod server;
mod shortcuts;
mod tray;
mod uploads;
mod xdg;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk4::cairo::Region;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, glib};
use game::Game;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use ipc::MainEvent;
use kwin_tracker::TrackerEvent;
use webkit6::WebView;
use webkit6::prelude::*;

pub(crate) const APP_ID: &str = "io.github.thwonghin.PoeKdeOverlay";

// Hotkeys are configured exclusively through KDE's own Global Shortcuts
// (System Settings, or ashpd's ConfigureShortcuts), not the renderer's own
// Settings UI — that UI is real, unmodified upstream Vue code we don't
// patch, so instead of hiding the tabs that reach them, we keep the tabs
// reachable but replace their content with a single explanatory banner,
// worded as a plain statement of where hotkeys live now (not as "this used
// to work here" — this app's own hotkey editing UI never actually took
// effect for most users anyway once ids became trigger-independent).
// Detected by marker label text, in English or the active UI language:
// the script loads that language's renderer/public/data/<lang>/app_i18n.json
// (the same file the renderer's i18n.ts uses, chosen by the `lang` attribute
// it sets on <html>). English is always matched too, since untranslated keys
// fall back to it (e.g. cmn-Hant has none of these three). If a translation
// fails to load, a non-English tab just keeps its original content:
//   - "Hotkeys" tab (renderer/src/web/settings/hotkeys.vue) — detected via
//     its "You can clear hotkey by pressing Backspace" hint box
//     (`settings.clear_hotkey`), which is
//     always the first thing that tab renders. Only that hint box and the
//     rows container right after it get replaced/hidden — the stash-scroll
//     radio is a later, unrelated real setting and stays untouched.
//   - "Item info" tab (item-check/settings-item-check.vue) — no equivalent
//     static hint box, so detected via its first hotkey row's own label
//     ("Open item on wiki", `item.open_on_wiki`). This tab's entire content
//     is hotkey rows, so
//     everything in it gets hidden, leaving just the banner.
// Not annotated: stash-search/stopwatch/chat-command hotkey fields — a key
// set there becomes that action's preferred KDE trigger the first time it's
// registered (apply_shortcuts in shortcuts.rs); after that KDE owns it like
// every other hotkey. ocr-gems is Windows-only upstream and stays inert.
//
// Price Check's own tab keeps a real, functional pair of toggles ("smart
// initial search" / "locked initial search" — price-check/
// settings-price-check.vue) that happen to also render the relevant
// hotkey as a small text label next to each one, purely for the user's
// reference. The toggles themselves aren't hotkey config, so the whole
// row stays visible; only that now-possibly-stale label text (its source
// config value no longer reflects the real KDE-owned trigger) gets
// replaced with a pointer to where the actual key now lives, found the
// same way — by the label of the row it's in (`price_check.auto_search`).
const HOTKEY_UI_INFO_JS: &str = include_str!("hotkey_ui_info.js");

fn main() -> glib::ExitCode {
    // GTK4 auto-selects its Vulkan GSK renderer on this NVIDIA setup, which
    // turned out to burn 80-140% CPU compositing our fullscreen transparent
    // surface during any WebView scroll/animation (measured via /proc/*/stat
    // deltas) -- WebKit's own rendering stayed under 3% the whole time, so
    // this was never a WebKit/hardware-acceleration problem. Forcing the
    // older OpenGL renderer dropped that to ~4%. Only set as a default -- an
    // explicit GSK_RENDERER in the environment (e.g. someone testing a
    // driver fix) should still win.
    //
    // The renderer's own name is "gl", not "ngl" -- confirmed live, setting
    // "ngl" on this GTK4 version silently fails ("Gsk-WARNING: The new GL
    // renderer has been renamed to gl") and GTK falls back to its own
    // auto-selection, i.e. exactly the slow Vulkan path this exists to
    // avoid. "ngl" was presumably correct on an older GTK4 release.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: first line of main, no other threads exist yet.
        unsafe { std::env::set_var("GSK_RENDERER", "gl") };
    }

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

/// GTK-main-thread state shared by the hotkey loop, the focus tracker, the
/// renderer's UI requests and the local key handler. Lives for the whole
/// process behind one `Rc`.
pub(crate) struct Overlay {
    pub window: ApplicationWindow,
    webview: WebView,
    /// Each available game's (port, bus); `active` picks the one the
    /// WebView shows (`switch_game`).
    games: Vec<(Game, u16, Arc<server::EventBus>)>,
    /// The focused game's renderer, `None` until a game is identified.
    pub active: Cell<Option<Game>>,
    /// Bus with no clients, standing in while no game is active: sends go
    /// nowhere, the same as before any renderer connects.
    idle: Arc<server::EventBus>,
    /// A new PoE process was seen while its renderer was still loading: the
    /// renderer's first host config (its "ready" signal) gets the
    /// overlay-attached instead.
    attach_pending: Cell<bool>,
    tray_active: async_channel::Sender<Option<Game>>,
    pub logger: Arc<logger::Logger>,
    /// Starts true: read from the moment the app starts, before the
    /// deferred idle callback in `build_ui` applies it to the surface.
    pub click_through: Cell<bool>,
    /// Host-side price-check popup bookkeeping.
    pub price_check_open: Cell<bool>,
    pub price_check_locked: Cell<bool>,
    /// The renderer's `restoreClipboard` setting.
    pub restore_clipboard: Cell<bool>,
    /// The active game's gameConfig setting (game_config.rs).
    pub game_config: RefCell<String>,
    /// The active game's initialDelay (poe2; 48 otherwise).
    pub initial_delay_ms: Cell<u64>,
    /// poe2's hideOverlayOnBlur, and its renderer's last render-state.
    hide_on_blur: Cell<bool>,
    should_show: Cell<bool>,
    /// Bumped to cancel `shortcuts::spawn_auto_close`'s poll loop
    /// (WidgetAreaTracker's removeListeners).
    pub auto_close_gen: Cell<u64>,
    /// Last externally focused window (our own surface's activations are
    /// filtered out).
    pub active_window: RefCell<Option<kwin_tracker::WindowEvent>>,
    kwin_connection: RefCell<Option<zbus::Connection>>,
    remote_input: RefCell<Option<Rc<remote_input::RemoteInput>>>,
}

impl Overlay {
    /// The active game's bus (see `idle`).
    pub fn events(&self) -> Arc<server::EventBus> {
        let active = self.active.get();
        self.games
            .iter()
            .find(|(game, _, _)| Some(*game) == active)
            .map_or_else(|| self.idle.clone(), |(_, _, bus)| bus.clone())
    }

    fn port_of(&self, game: Game) -> Option<u16> {
        self.games.iter().find(|(g, _, _)| *g == game).map(|(_, port, _)| *port)
    }

    /// Shows `game`'s renderer: the focused game alone decides this. A game
    /// whose renderer isn't built shows nothing rather than the other's.
    /// ponytail: reloads the one WebView (~1-2s), so a hotkey pressed
    /// mid-reload is lost; a WebView per game in a GtkStack if that matters.
    pub fn switch_game(&self, game: Option<Game>) {
        let game = game.filter(|g| self.port_of(*g).is_some());
        if self.active.get() == game {
            return;
        }
        println!("[main] overlay now shows {}", game.map_or("nothing", Game::name));
        self.price_check_open.set(false);
        self.price_check_locked.set(false);
        self.auto_close_gen.set(self.auto_close_gen.get() + 1);
        self.set_click_through(true);
        self.active.set(game);
        // A toast still owed to the previous renderer isn't this one's.
        self.attach_pending.set(false);
        let _ = self.tray_active.try_send(game);
        // Navigating alone keeps the old page's memory: measured ~360 MB
        // after unloading to about:blank vs ~175 MB with the web process
        // killed, and 40-90 MB of the previous game lingering after a
        // switch. The next load_uri spawns a fresh process.
        match game.and_then(|g| self.port_of(g)) {
            Some(port) => {
                self.webview.terminate_web_process();
                self.webview.load_uri(&format!("http://127.0.0.1:{port}/"));
            }
            None => {
                self.webview.load_uri("about:blank");
                self.webview.terminate_web_process();
            }
        }
    }

    pub fn kwin(&self) -> Option<zbus::Connection> {
        self.kwin_connection.borrow().clone()
    }

    /// Cloned out so callers never hold a `RefCell` borrow across an await.
    pub fn remote(&self) -> Option<Rc<remote_input::RemoteInput>> {
        self.remote_input.borrow().clone()
    }

    pub fn poe_active(&self) -> bool {
        self.active_window
            .borrow()
            .as_ref()
            .is_some_and(kwin_tracker::WindowEvent::is_path_of_exile)
    }

    pub fn toggle_click_through(&self) {
        self.set_click_through(!self.click_through.get());
    }

    /// `can-target` only affects GTK's own internal hit-testing, not the
    /// input region the compositor is told about — that requires poking the
    /// surface directly, or clicks keep landing on our window no matter what.
    pub fn set_click_through(&self, now_click_through: bool) {
        if self.click_through.get() == now_click_through {
            return;
        }
        self.click_through.set(now_click_through);
        apply_click_through_to_surface(now_click_through, &self.window);
        self.sync_window_visibility();

        // The renderer only auto-hides "hide-on-blur" popups (e.g. the
        // price-check window) in reaction to this event
        // (renderer/src/web/overlay/OverlayWindow.vue). click-through ON
        // maps to "overlay not active", OFF to "overlay active".
        self.events().broadcast(&MainEvent::FocusChange {
            game: now_click_through,
            overlay: !now_click_through,
            using_hotkey: true,
        });
    }

    /// OverlayWindow.ts's syncWindowVisibility: with hideOverlayOnBlur, the
    /// surface is unmapped unless the overlay is interactive, or PoE is
    /// focused and some widget wants showing. Without it, always mapped.
    pub fn sync_window_visibility(&self) {
        let visible = !self.hide_on_blur.get()
            || !self.click_through.get()
            || (self.should_show.get() && self.poe_active());
        if visible == self.window.is_visible() {
            return;
        }
        debug!("[main] hideOverlayOnBlur: overlay {}", if visible { "shown" } else { "hidden" });
        self.window.set_visible(visible);
        if visible {
            // Remapping resets the input region (see move_overlay_to_window_monitor).
            let window = self.window.clone();
            let click_through = self.click_through.get();
            glib::idle_add_local_once(move || apply_click_through_to_surface(click_through, &window));
        }
    }

    pub fn hide_price_check(&self) {
        self.events().broadcast(&MainEvent::HideExclusiveWidget);
        self.price_check_open.set(false);
    }

    /// Force click-through back on and close any open overlay widgets, as an
    /// always-available safety hatch regardless of what's currently showing.
    /// Matches real APT's `assertGameActive` (main/src/windowing/OverlayWindow.ts).
    pub async fn close_all_ui(&self) {
        debug!(
            "[main] close_all_ui: price_check_open={}, price_check_locked={}, click_through was {}",
            self.price_check_open.get(),
            self.price_check_locked.get(),
            self.click_through.get()
        );
        if self.price_check_open.get() {
            self.hide_price_check();
        }
        self.price_check_locked.set(false);
        self.set_click_through(true);

        // Restoring click-through doesn't hand OS focus back to PoE on its
        // own (see activate_path_of_exile). Best-effort: a failure just
        // means the user clicks the game once more themselves.
        if let Some(connection) = self.kwin()
            && let Err(err) = kwin_tracker::activate_path_of_exile(&connection).await
        {
            eprintln!("[main] failed to re-activate Path of Exile: {err}");
        }
    }
}

/// `None` if `fut` didn't finish within `duration`. For awaits on the GTK
/// main thread that must not hang the hotkey loop (AGENTS.md).
pub(crate) async fn with_timeout<F: std::future::Future>(
    duration: std::time::Duration,
    fut: F,
) -> Option<F::Output> {
    let fut = std::pin::pin!(fut);
    let timer = std::pin::pin!(glib::timeout_future(duration));
    match futures_util::future::select(fut, timer).await {
        futures_util::future::Either::Left((value, _)) => Some(value),
        futures_util::future::Either::Right(_) => None,
    }
}

fn build_ui(app: &Application) {
    // Single instance (real main.ts's requestSingleInstanceLock): GApplication
    // is already unique per APP_ID over D-Bus — a second launch just asks
    // this process to activate again and exits. Without this guard that
    // re-activation built a second overlay, server and portal sessions here.
    if !app.windows().is_empty() {
        println!("[main] already running; ignoring second launch");
        return;
    }
    let window = ApplicationWindow::builder()
        .application(app)
        .title("poe-kde-overlay")
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Bottom, true);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, true);
    window.set_exclusive_zone(-1);
    // Starting mode: never steal focus from the game while click-through is
    // on. apply_click_through_to_surface flips this to OnDemand whenever
    // click-through goes off, so keypresses into our own UI (e.g. typing in
    // settings) reach it instead of the game.
    //
    // A layer-shell surface that never holds keyboard focus also can't
    // reliably read gdk::Clipboard (Wayland gates it by focus) — irrelevant
    // here since price-check reads go through the portal's Clipboard
    // interface instead (remote_input.rs).
    window.set_keyboard_mode(KeyboardMode::None);

    let logger = Arc::new(logger::Logger::new());
    let spawned: std::io::Result<Vec<server::Server>> = Game::ALL
        .into_iter()
        .filter(|game| {
            let available = game.available();
            if !available {
                eprintln!("[main] no {} renderer at {:?}: {} unsupported", game.name(), game.dist_dir(), game.label());
            }
            available
        })
        .map(|game| server::spawn(game, logger.clone()))
        .collect();
    let servers = match spawned {
        Ok(servers) => servers,
        Err(err) => {
            eprintln!("[main] failed to start local server: {err}");
            let dialog = gtk4::AlertDialog::builder()
                .message("poe-kde-overlay failed to start")
                .detail(format!("Could not start the local server: {err}"))
                .build();
            let app = app.clone();
            dialog.choose(
                None::<&ApplicationWindow>,
                gtk4::gio::Cancellable::NONE,
                move |_| {
                    app.quit();
                },
            );
            return;
        }
    };
    let games: Vec<(Game, u16, Arc<server::EventBus>)> =
        servers.iter().map(|s| (s.game, s.port, s.events.clone())).collect();
    for (game, port, _) in &games {
        println!("[main] {} server listening on 127.0.0.1:{port}", game.name());
    }

    let (tray_rx, tray_active) = tray::spawn(games.iter().map(|(game, _, _)| *game).collect());
    {
        let app = app.clone();
        let ports: Vec<(Game, u16)> = games.iter().map(|(game, port, _)| (*game, *port)).collect();
        glib::spawn_future_local(async move {
            while let Ok(action) = tray_rx.recv().await {
                match action {
                    tray::TrayAction::OpenSettings(game) => {
                        let Some((_, port)) = ports.iter().find(|(g, _)| *g == game) else { continue };
                        let _ = gtk4::gio::AppInfo::launch_default_for_uri(
                            &format!("http://127.0.0.1:{port}/"),
                            gtk4::gio::AppLaunchContext::NONE,
                        );
                    }
                    tray::TrayAction::OpenConfigFolder => {
                        let path = xdg::data_dir().join("poe-kde-overlay");
                        let uri = gtk4::gio::File::for_path(&path).uri();
                        let _ = gtk4::gio::AppInfo::launch_default_for_uri(&uri, gtk4::gio::AppLaunchContext::NONE);
                    }
                    tray::TrayAction::Quit => app.quit(),
                }
            }
        });
    }

    let user_content = webkit6::UserContentManager::new();
    user_content.add_script(&webkit6::UserScript::new(
        HOTKEY_UI_INFO_JS,
        webkit6::UserContentInjectedFrames::TopFrame,
        webkit6::UserScriptInjectionTime::End,
        &[],
        &[],
    ));
    let webview = WebView::builder().user_content_manager(&user_content).build();
    webview.set_background_color(&gtk4::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    // The renderer only enables overlay-mode behavior (vs. plain-browser
    // mode) when navigator.userAgent contains "Electron"
    // (renderer/src/web/background/IPC.ts).
    if let Some(settings) = webkit6::prelude::WebViewExt::settings(&webview) {
        let ua = settings
            .user_agent()
            .map(|s| s.to_string())
            .unwrap_or_default();
        settings.set_user_agent(Some(&format!("{ua} Electron/32.0.0")));

        // Already WebKit's default; set explicitly so a future WebKitGTK
        // default change can't silently regress rendering performance.
        settings.set_hardware_acceleration_policy(webkit6::HardwareAccelerationPolicy::Always);
    }
    // The renderer opens external links (e.g. the price-check popup's
    // "Trade" button, TradeLinks.vue) with plain `window.open(url)`. Real
    // Electron hands that to `shell.openExternal` (OverlayWindow.ts's
    // setWindowOpenHandler); WebKitGTK's equivalent is the "create" signal.
    // Launch the default browser and return None so no in-app popup opens.
    webview.connect_create(|_view, navigation_action| {
        if let Some(uri) = navigation_action.request().and_then(|req| req.uri())
            && let Err(err) =
                gtk4::gio::AppInfo::launch_default_for_uri(&uri, gtk4::gio::AppLaunchContext::NONE)
        {
            eprintln!("[main] failed to open external link {uri}: {err}");
        }
        None
    });

    // Blank until a game is focused (Overlay::switch_game loads its renderer).
    window.set_child(Some(&webview));

    // Transparent CSS background so the layer surface itself doesn't paint opaque.
    let css = gtk4::CssProvider::new();
    css.load_from_string("window { background: transparent; }");
    gtk4::style_context_add_provider_for_display(
        &WidgetExt::display(&window),
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    window.present();
    // Applies click_through's initial `true` to the surface once layout
    // has happened.
    {
        let window = window.clone();
        glib::idle_add_local_once(move || {
            apply_click_through_to_surface(true, &window);
        });
    }

    let overlay = Rc::new(Overlay {
        window: window.clone(),
        webview,
        games,
        active: Cell::new(None),
        idle: Arc::new(server::EventBus::new().0),
        attach_pending: Cell::new(false),
        tray_active,
        logger,
        click_through: Cell::new(true),
        price_check_open: Cell::new(false),
        price_check_locked: Cell::new(false),
        restore_clipboard: Cell::new(false),
        game_config: RefCell::new(String::new()),
        initial_delay_ms: Cell::new(48),
        hide_on_blur: Cell::new(false),
        should_show: Cell::new(true),
        auto_close_gen: Cell::new(0),
        active_window: RefCell::new(None),
        kwin_connection: RefCell::new(None),
        remote_input: RefCell::new(None),
    });

    // Renderer requests (server.rs's UiRequest), from either game: Quit and
    // the stash-search widget work from a browser tab too, as upstream.
    for backend in &servers {
        let app = app.clone();
        let overlay = overlay.clone();
        let ui_request_rx = backend.ui_request_rx.clone();
        glib::spawn_future_local(async move {
            while let Ok(request) = ui_request_rx.recv().await {
                match request {
                    server::UiRequest::Quit => app.quit(),
                    server::UiRequest::RenderState(should_show) => {
                        overlay.should_show.set(should_show);
                        overlay.sync_window_visibility();
                    }
                    // The renderer's title-bar "x" sends focus-game instead
                    // of hiding itself locally (OverlayWindow.ts).
                    server::UiRequest::FocusGame => overlay.close_all_ui().await,
                    // text-box.ts's stashSearch hands focus back to the game
                    // before typing into it.
                    server::UiRequest::StashSearch(text) => {
                        overlay.close_all_ui().await;
                        let action = host_config::ActionKind::StashSearch { text };
                        shortcuts::paste_action(&overlay, &action, "").await;
                    }
                }
            }
        });
    }

    // CLIENT->MAIN::update-host-config: restoreClipboard applies right away;
    // the shortcut list goes on to shortcuts.rs, which only reads it once
    // its GlobalShortcuts session is up (if that never happens, the
    // stash-search widget still gets the right restoreClipboard). Only the
    // active game's counts: the other one's comes from its browser tab, and
    // its renderer resends its own on the next switch.
    let (shortcuts_tx, shortcuts_rx) = async_channel::unbounded();
    for backend in &servers {
        let game = backend.game;
        let overlay = overlay.clone();
        let shortcuts_tx = shortcuts_tx.clone();
        let host_config_rx = backend.host_config_rx.clone();
        glib::spawn_future_local(async move {
            while let Ok(config) = host_config_rx.recv().await {
                if overlay.active.get() != Some(game) {
                    continue;
                }
                if overlay.attach_pending.replace(false) {
                    overlay.events().broadcast(&MainEvent::OverlayAttached);
                }
                overlay.restore_clipboard.set(config.restore_clipboard);
                overlay.game_config.replace(config.game_config);
                overlay.initial_delay_ms.set(config.initial_delay_ms);
                overlay.hide_on_blur.set(config.hide_overlay_on_blur);
                overlay.sync_window_visibility();
                let _ = shortcuts_tx.send(config.shortcuts).await;
            }
        });
    }

    let (sender, receiver) = async_channel::unbounded::<TrackerEvent>();
    let conn_rx = kwin_tracker::spawn(sender);
    {
        let overlay = overlay.clone();
        glib::spawn_future_local(async move {
            if let Ok(connection) = conn_rx.recv().await {
                overlay.kwin_connection.replace(Some(connection));
            }
        });
    }

    {
        let overlay = overlay.clone();
        // Per game, the PoE pid `overlay-attached` was last sent for.
        let mut attached_pid: std::collections::HashMap<Game, i64> = Default::default();
        // Bumped on every activation; a pending unload only fires if no
        // activation came after it.
        let focus_gen = Rc::new(Cell::new(0u64));
        glib::spawn_future_local(async move {
            while let Ok(event) = receiver.recv().await {
                match event {
                    TrackerEvent::Activated(w) => {
                        focus_gen.set(focus_gen.get() + 1);
                        // Our own layer-shell surface gets a genuine
                        // activation the moment it becomes keyboard-
                        // interactive (e.g. a locked price-check popup flips
                        // KeyboardMode to OnDemand). Recording it as
                        // `active_window` made every later hotkey's is-PoE
                        // check fail until PoE was re-activated for real.
                        if w.is_own_process() {
                            continue;
                        }
                        if w.is_path_of_exile() {
                            debug!(
                                "[kwin_tracker] activated: class={:?} caption={:?} pid={} {}x{} @ ({}, {})",
                                w.class_name, w.caption, w.pid, w.width, w.height, w.x, w.y
                            );
                            let shown = overlay.active.get();
                            // Class-only match (no game): keep what's shown.
                            if let Some(game) = w.game() {
                                if let Some((_, _, bus)) = overlay.games.iter().find(|(g, _, _)| *g == game) {
                                    bus.game_log(game_log::Command::Pid(w.pid));
                                }
                                overlay.switch_game(Some(game));
                            }
                            move_overlay_to_window_monitor(&overlay, &w);
                            // Upstream sends this on electron-overlay-window's
                            // `attach`, i.e. once per PoE window: the
                            // "overlay ready" toast (LoadingAnimation.vue, its
                            // only consumer in both renderers). Upstream never
                            // reloads, so our reloads (game switch, unload)
                            // don't re-send it; a new pid (game restarted)
                            // does. A renderer that just started loading gets
                            // it via attach_pending once it's up.
                            if let Some(game) = overlay.active.get()
                                && attached_pid.insert(game, w.pid) != Some(w.pid)
                            {
                                if overlay.active.get() != shown {
                                    overlay.attach_pending.set(true);
                                } else {
                                    overlay.events().broadcast(&MainEvent::OverlayAttached);
                                }
                            }
                            // Mirrors real APT's handlePoeWindowActiveChange:
                            // PoE *regaining* focus forces any interactive
                            // overlay state back to click-through.
                            if !overlay.click_through.get() {
                                overlay.close_all_ui().await;
                            }
                        } else {
                            debug!(
                                "[kwin_tracker] activated (not PoE): class={:?} caption={:?} pid={}",
                                w.class_name, w.caption, w.pid
                            );
                            // KWin reports no window-closed event, so "PoE
                            // closed" and "tabbed out for long" look the
                            // same: unload either way, freeing ~185 MB. The
                            // renderer reloads (1-2 s) on next PoE focus.
                            // ponytail: fixed 10 min, a setting if anyone
                            // wants it shorter.
                            let unfocused_at = focus_gen.get();
                            let focus_gen = focus_gen.clone();
                            let overlay = overlay.clone();
                            glib::timeout_add_seconds_local_once(600, move || {
                                if focus_gen.get() == unfocused_at {
                                    overlay.switch_game(None);
                                }
                            });
                        }
                        overlay.active_window.replace(Some(w));
                        overlay.sync_window_visibility();
                    }
                    TrackerEvent::OutputChanged(w) => {
                        // Follow PoE to another monitor without waiting for
                        // it to be re-activated.
                        if w.is_path_of_exile() {
                            debug!(
                                "[kwin_tracker] PoE moved output: {}x{} @ ({}, {})",
                                w.width, w.height, w.x, w.y
                            );
                            move_overlay_to_window_monitor(&overlay, &w);
                        }
                    }
                }
            }
        });
    }

    // Local Escape/Ctrl+W handling, mirroring real APT's OverlayWindow.ts
    // handleExtraCommands: only fires while our surface holds keyboard focus
    // (KeyboardMode::OnDemand, i.e. click-through off).
    {
        let overlay = overlay.clone();
        let key_controller = gtk4::EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key_controller.connect_key_pressed(move |_, key, _, modifiers| {
            let is_escape = key == gtk4::gdk::Key::Escape;
            let is_ctrl_w = key == gtk4::gdk::Key::w
                && modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
            if !is_escape && !is_ctrl_w {
                return glib::Propagation::Proceed;
            }
            let overlay = overlay.clone();
            glib::spawn_future_local(async move { overlay.close_all_ui().await });
            glib::Propagation::Stop
        });
        window.add_controller(key_controller);
    }

    {
        glib::spawn_future_local(async move {
            // Launched from a terminal (no systemd app-scope), so the portal
            // can't derive our app id on its own — register it explicitly
            // first, once, or every app-id-gated portal call (GlobalShortcuts,
            // RemoteDesktop) fails with "An app id is required". The portal
            // errors if this is called twice, so it happens exactly once here.
            if let Err(err) = ashpd::register_host_app(APP_ID.try_into().unwrap()).await {
                overlay.logger.error(&format!("[main] failed to register host app: {err}"));
                return;
            }

            let visibility = overlay_visibility::OverlayVisibility::new(overlay.clone());
            match remote_input::RemoteInput::setup(overlay.logger.clone(), move |depressed| {
                visibility.on_modifiers(depressed)
            })
            .await
            {
                Ok(ri) => {
                    overlay.remote_input.replace(Some(Rc::new(ri)));
                }
                Err(err) => overlay.logger.error(&format!("[main] remote_input setup failed: {err}")),
            }

            shortcuts::spawn(overlay, shortcuts_rx);
        });
    }
}

/// Applies `now_click_through` to the surface's actual Wayland input region,
/// unconditionally — split out of `set_click_through` so callers that need
/// to force a re-apply (the surface was remapped and may have silently
/// dropped its input region) can bypass its unchanged-state early return.
fn apply_click_through_to_surface(now_click_through: bool, window: &ApplicationWindow) {
    // OnDemand lets the compositor give this surface keyboard focus when
    // something in it is clicked (e.g. a text field), without us stealing
    // focus from the game while click-through is on.
    window.set_keyboard_mode(if now_click_through {
        KeyboardMode::None
    } else {
        KeyboardMode::OnDemand
    });

    let Some(surface) = window.surface() else {
        return;
    };
    if now_click_through {
        surface.set_input_region(Some(&Region::create()));
    } else {
        surface.set_input_region(None);
    }
}

/// A layer-shell surface with no explicit monitor set just lands on
/// whatever output the compositor considers default — not necessarily
/// where the game is on a multi-monitor setup. Changing the monitor remaps
/// the surface, which silently resets the click-through input region, so it
/// is re-applied afterwards; deferred to idle so the remap (not necessarily
/// synchronous with `set_monitor()`) has landed first.
fn move_overlay_to_window_monitor(overlay: &Rc<Overlay>, target: &kwin_tracker::WindowEvent) {
    let window = &overlay.window;
    let display = WidgetExt::display(window);
    let center_x = target.x + target.width / 2.0;
    let center_y = target.y + target.height / 2.0;

    let monitors = display.monitors();
    for i in 0..monitors.n_items() {
        let Some(obj) = monitors.item(i) else {
            continue;
        };
        let Ok(monitor) = obj.downcast::<gtk4::gdk::Monitor>() else {
            continue;
        };
        let geo = monitor.geometry();
        let (mx, my, mw, mh) = (
            geo.x() as f64,
            geo.y() as f64,
            geo.width() as f64,
            geo.height() as f64,
        );
        if center_x >= mx && center_x < mx + mw && center_y >= my && center_y < my + mh {
            if window.monitor().as_ref() != Some(&monitor) {
                debug!("[main] moving overlay to PoE's monitor");
                window.set_monitor(Some(&monitor));

                let overlay = overlay.clone();
                glib::idle_add_local_once(move || {
                    apply_click_through_to_surface(overlay.click_through.get(), &overlay.window);
                });
            }
            return;
        }
    }
}
