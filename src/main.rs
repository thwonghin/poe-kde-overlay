mod config_store;
mod host_config;
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

use gtk4::cairo::Region;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use kwin_tracker::TrackerEvent;
use webkit6::WebView;
use webkit6::prelude::*;

pub(crate) const APP_ID: &str = "io.github.thwonghin.AptKwinOverlay";

// Hotkeys are configured exclusively through KDE's own Global Shortcuts
// (System Settings, or ashpd's ConfigureShortcuts), not the renderer's own
// Settings UI — that UI is real, unmodified upstream Vue code we don't
// patch, so instead of hiding the tabs that reach them, we keep the tabs
// reachable but replace their content with a single explanatory banner,
// worded as a plain statement of where hotkeys live now (not as "this used
// to work here" — this app's own hotkey editing UI never actually took
// effect for most users anyway once ids became trigger-independent).
// Detected by literal English marker text (renderer/public/data/en/
// app_i18n.json) — only correct for the English locale; ponytail: good
// enough for now, a non-English label just means the tab's original
// content stays visible instead of being replaced, rather than anything
// breaking:
//   - "Hotkeys" tab (renderer/src/web/settings/hotkeys.vue) — detected via
//     its "You can clear hotkey by pressing Backspace" hint box, which is
//     always the first thing that tab renders. Only that hint box and the
//     rows container right after it get replaced/hidden — the stash-scroll
//     radio is a later, unrelated real setting and stays untouched.
//   - "Item info" tab (item-check/settings-item-check.vue) — no equivalent
//     static hint box, so detected via its first hotkey row's own label
//     ("Open item on wiki"). This tab's entire content is hotkey rows, so
//     everything in it gets hidden, leaving just the banner.
// Not annotated: stash-search/stopwatch/chat-command/ocr-gems hotkey
// fields — those map to action types we don't bind at all (Unsupported),
// so editing them has always been inert; that's a pre-existing
// WIP.md-documented gap, not something today's stable-id change made worse.
//
// Price Check's own tab keeps a real, functional pair of toggles ("smart
// initial search" / "locked initial search" — price-check/
// settings-price-check.vue) that happen to also render the relevant
// hotkey as a small text label next to each one, purely for the user's
// reference. The toggles themselves aren't hotkey config, so the whole
// row stays visible; only that now-possibly-stale label text (its source
// config value no longer reflects the real KDE-owned trigger) gets
// replaced with a pointer to where the actual key now lives, found the
// same way — by the literal English label of the row it's in.
const HOTKEY_UI_INFO_JS: &str = r#"
(function () {
  var CLEAR_HOTKEY_TEXT = 'You can clear hotkey by pressing Backspace';
  var ITEM_CHECK_FIRST_LABEL = 'Open item on wiki';
  var AUTO_SEARCH_LABEL = 'Perform an auto search, when pressing';
  var BANNER_TEXT = 'Hotkeys are configured in KDE System Settings → Shortcuts.';

  function makeBanner() {
    var banner = document.createElement('div');
    banner.textContent = BANNER_TEXT;
    banner.style.cssText =
      'margin-bottom:0.75rem;padding:0.5rem;border-radius:0.25rem;' +
      'background:#7c2d12;color:#fed7aa;font-size:0.875rem;';
    return banner;
  }

  function addHotkeyUiInfo() {
    // Hotkeys tab: the hint box + the rows container that follows it are
    // the only two children that are actually hotkey UI — the stash-scroll
    // radio (a real, unrelated setting) is a later sibling and stays put.
    var divs = document.querySelectorAll('div');
    for (var i = 0; i < divs.length; i++) {
      var marker = divs[i];
      if (marker.dataset.aptBannerAdded) continue;
      if (marker.textContent.trim() !== CLEAR_HOTKEY_TEXT) continue;
      var tabRoot = marker.parentElement;
      if (!tabRoot) continue;
      var rowsContainer = marker.nextElementSibling;
      tabRoot.insertBefore(makeBanner(), marker);
      marker.style.display = 'none';
      if (rowsContainer) rowsContainer.style.display = 'none';
      marker.dataset.aptBannerAdded = '1';
    }

    // Item info tab: its entire content is hotkey rows (no unrelated
    // settings mixed in), so every existing child of the tab root gets
    // hidden and the banner becomes the tab's only visible content.
    var labels = document.querySelectorAll('label');
    for (var j = 0; j < labels.length; j++) {
      if (labels[j].textContent.trim().indexOf(ITEM_CHECK_FIRST_LABEL) !== 0) continue;
      var firstRow = labels[j].parentElement;
      var tabRoot2 = firstRow ? firstRow.parentElement : null;
      if (!tabRoot2 || tabRoot2.dataset.aptBannerAdded) continue;
      tabRoot2.dataset.aptBannerAdded = '1';
      var existingRows = Array.prototype.slice.call(tabRoot2.children);
      tabRoot2.insertBefore(makeBanner(), tabRoot2.firstChild);
      for (var r = 0; r < existingRows.length; r++) existingRows[r].style.display = 'none';
    }

    var searchDivs = document.querySelectorAll('div');
    for (var k = 0; k < searchDivs.length; k++) {
      var el = searchDivs[k];
      if (el.dataset.aptLabelsFixed || el.children.length > 0) continue;
      if (el.textContent.trim() !== AUTO_SEARCH_LABEL) continue;
      el.dataset.aptLabelsFixed = '1';
      var container = el.nextElementSibling;
      if (!container) continue;
      // Source order matches Vue's v-if order: hotkeyQuick's span (if
      // rendered) always comes before hotkeyLocked's — same two terms
      // settings/hotkeys.vue itself uses for these ("Auto-hide Mode" /
      // "Open without auto-hide") so the label stays recognizable, just
      // without a specific (and potentially stale) key combo.
      var spans = container.querySelectorAll('span');
      var SPAN_LABELS = ['Auto-hide (KDE)', 'No auto-hide (KDE)'];
      for (var m = 0; m < spans.length; m++) {
        spans[m].textContent = SPAN_LABELS[m] || 'Set in KDE';
        spans[m].style.whiteSpace = 'nowrap';
      }
    }
  }
  // ponytail: whole-document MutationObserver rather than scoping to the
  // settings window container specifically — simplest thing that works,
  // scope tighter if it ever shows up as a real perf cost.
  new MutationObserver(addHotkeyUiInfo).observe(document.documentElement, {
    childList: true,
    subtree: true
  });
  addHotkeyUiInfo();
})();
"#;

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
        .title("apt-kwin-overlay")
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
    // click-through goes off, so clicking into a text field in our own UI
    // (e.g. typing in settings) actually reaches it instead of the game —
    // without that, mouse clicks land correctly (input region opens up) but
    // every keypress still goes to whatever holds keyboard focus, which was
    // always the game since this was never changed after startup.
    //
    // Confirmed via testing: a layer-shell surface that never holds keyboard
    // focus also can't reliably read gdk::Clipboard (Wayland gates clipboard
    // visibility by focus for privacy) — irrelevant here since price-check
    // clipboard reads go through the portal's Clipboard interface instead
    // (remote_input.rs), which works regardless of local focus.
    window.set_keyboard_mode(KeyboardMode::None);

    let (port, backend) = match server::spawn() {
        Ok(v) => v,
        Err(err) => {
            eprintln!("[main] failed to start local server: {err}");
            let dialog = gtk4::AlertDialog::builder()
                .message("apt-kwin-overlay failed to start")
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
    println!("[main] server listening on 127.0.0.1:{port}");

    let tray_rx = tray::spawn();
    {
        let app = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(action) = tray_rx.recv().await {
                match action {
                    tray::TrayAction::OpenBrowser => {
                        let _ = gtk4::gio::AppInfo::launch_default_for_uri(
                            &format!("http://127.0.0.1:{port}/"),
                            gtk4::gio::AppLaunchContext::NONE,
                        );
                    }
                    tray::TrayAction::OpenConfigFolder => {
                        let path = xdg::data_dir().join("apt-kwin-overlay");
                        let uri = gtk4::gio::File::for_path(&path).uri();
                        let _ = gtk4::gio::AppInfo::launch_default_for_uri(&uri, gtk4::gio::AppLaunchContext::NONE);
                    }
                    tray::TrayAction::Quit => app.quit(),
                }
            }
        });
    }
    {
        let app = app.clone();
        let quit_rx = backend.quit_rx.clone();
        glib::spawn_future_local(async move {
            if quit_rx.recv().await.is_ok() {
                app.quit();
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
    // (renderer/src/web/background/IPC.ts) — we're WebKitGTK, not Electron,
    // so without this it would silently fall back to browser mode.
    if let Some(settings) = webkit6::prelude::WebViewExt::settings(&webview) {
        let ua = settings
            .user_agent()
            .map(|s| s.to_string())
            .unwrap_or_default();
        settings.set_user_agent(Some(&format!("{ua} Electron/32.0.0")));

        // Confirmed via testing: this is WebKit's own default already (not
        // the bottleneck behind observed UI lag, which traced instead to
        // kwin_tracker.rs hammering KWin's scripting D-Bus interface — see
        // shortcuts.rs's spawn_auto_close). Set explicitly anyway so this
        // doesn't silently regress if a future WebKitGTK version changes
        // its default.
        settings.set_hardware_acceleration_policy(webkit6::HardwareAccelerationPolicy::Always);
    }
    // The renderer opens external links (e.g. the price-check popup's
    // "Trade" button, TradeLinks.vue) with plain `window.open(url)`. Real
    // Electron intercepts that via `setWindowOpenHandler` and hands it to
    // `shell.openExternal` (main/src/windowing/OverlayWindow.ts) instead of
    // letting Electron create a popup window. WebKitGTK's equivalent hook is
    // the "create" signal -- left unconnected, `window.open()` just does
    // nothing observable. Mirror Electron: launch the user's default
    // browser and return None so WebKit never creates an in-app popup.
    webview.connect_create(|_view, navigation_action| {
        if let Some(uri) = navigation_action.request().and_then(|req| req.uri())
            && let Err(err) =
                gtk4::gio::AppInfo::launch_default_for_uri(&uri, gtk4::gio::AppLaunchContext::NONE)
        {
            eprintln!("[main] failed to open external link {uri}: {err}");
        }
        None
    });

    webview.load_uri(&format!("http://127.0.0.1:{port}/"));

    // Starts true (not false-then-flipped): other logic reads this Cell
    // from the moment the app starts, before the deferred idle callback
    // below has had a chance to run.
    let click_through = Rc::new(Cell::new(true));
    // Host-side price-check popup bookkeeping, hoisted out of
    // shortcuts::spawn (which used to own them locally) so the
    // kwin_tracker::Activated handler below can also reset them via
    // shortcuts::close_all_ui when PoE regains focus.
    let price_check_open = Rc::new(Cell::new(false));
    let price_check_locked = Rc::new(Cell::new(false));

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
    // click_through's tracked state already defaults to true (see above);
    // this just applies that to the actual surface once layout has happened.
    {
        let window = window.clone();
        glib::idle_add_local_once(move || {
            apply_click_through_to_surface(true, &window);
        });
    }

    let active_window: Rc<RefCell<Option<kwin_tracker::WindowEvent>>> = Rc::new(RefCell::new(None));
    let (sender, receiver) = async_channel::unbounded::<TrackerEvent>();
    let conn_rx = kwin_tracker::spawn(sender);

    let kwin_connection: Rc<RefCell<Option<zbus::Connection>>> = Rc::new(RefCell::new(None));
    {
        let kwin_connection = kwin_connection.clone();
        glib::spawn_future_local(async move {
            if let Ok(connection) = conn_rx.recv().await {
                kwin_connection.replace(Some(connection));
            }
        });
    }

    {
        let active_window = active_window.clone();
        let window = window.clone();
        let click_through_for_monitor = click_through.clone();
        let events_for_tracker = backend.events.clone();
        let price_check_open_for_tracker = price_check_open.clone();
        let price_check_locked_for_tracker = price_check_locked.clone();
        let kwin_connection_for_tracker = kwin_connection.clone();
        let mut attached = false;
        glib::spawn_future_local(async move {
            while let Ok(event) = receiver.recv().await {
                match event {
                    TrackerEvent::Activated(w) => {
                        // Our own layer-shell surface gets a genuine
                        // window-activation event the moment it becomes
                        // keyboard-interactive (e.g. opening a locked
                        // price-check popup flips KeyboardMode to OnDemand) —
                        // confirmed live, this fires without the user even
                        // clicking anything. Ignore it entirely: it isn't a
                        // real "user switched to another app" event, and
                        // letting it overwrite `active_window` was making
                        // price_check's is_poe guard (below, via
                        // shortcuts::price_check) wrongly reject every hotkey
                        // press until PoE happened to get a fresh, real
                        // activation of its own — e.g. right after opening a
                        // locked popup and clicking the background to close
                        // it, both Ctrl+D and Ctrl+Alt+D would silently do
                        // nothing.
                        if w.is_own_process() {
                            continue;
                        }
                        // Layer-shell surfaces with no explicit monitor set
                        // just go wherever the compositor's default output
                        // is — which isn't necessarily where the game
                        // actually is on a multi-monitor setup. Follow PoE.
                        if w.is_path_of_exile() {
                            println!(
                                "[kwin_tracker] activated: class={:?} caption={:?} pid={} {}x{} @ ({}, {})",
                                w.class_name, w.caption, w.pid, w.width, w.height, w.x, w.y
                            );
                            move_overlay_to_window_monitor(&window, &w, &click_through_for_monitor);
                            // Real APT sends this when electron-overlay-window
                            // first attaches to PoE; drives the renderer's
                            // "overlay ready" toast (LoadingAnimation.vue).
                            // ponytail: once per process — PoE restarts
                            // aren't detected, so no re-toast on relaunch.
                            if !attached {
                                attached = true;
                                events_for_tracker
                                    .broadcast("MAIN->OVERLAY::overlay-attached", serde_json::Value::Null);
                            }
                            // Mirrors real APT's handlePoeWindowActiveChange:
                            // PoE *regaining* focus forces any
                            // locked/interactive overlay state back to
                            // click-through, the same path the renderer's
                            // own title-bar close button already uses.
                            if !click_through_for_monitor.get() {
                                shortcuts::close_all_ui(
                                    &window,
                                    &click_through_for_monitor,
                                    &events_for_tracker,
                                    &price_check_open_for_tracker,
                                    &price_check_locked_for_tracker,
                                    &kwin_connection_for_tracker,
                                )
                                .await;
                            }
                        }
                        if !w.is_path_of_exile() {
                            println!(
                                "[kwin_tracker] activated (not PoE): class={:?} caption={:?} pid={}",
                                w.class_name, w.caption, w.pid
                            );
                        }
                        active_window.replace(Some(w));
                    }
                    TrackerEvent::GeometryChanged(w) => {
                        if w.is_path_of_exile() {
                            println!(
                                "[kwin_tracker] geometry: class={:?} caption={:?} {}x{} @ ({}, {})",
                                w.class_name, w.caption, w.width, w.height, w.x, w.y
                            );
                        }
                    }
                }
            }
        });
    }

    // Local Escape/Ctrl+W handling, mirroring real APT's OverlayWindow.ts
    // handleExtraCommands: only fires while our surface holds keyboard focus
    // (KeyboardMode::OnDemand, i.e. click-through off) — a different mechanism
    // than the global-grab approach removed in df5aec9 (no portal session
    // involved), now viable since d749d98 made local focus real.
    {
        let window_for_keys = window.clone();
        let click_through_for_keys = click_through.clone();
        let events_for_keys = backend.events.clone();
        let price_check_open_for_keys = price_check_open.clone();
        let price_check_locked_for_keys = price_check_locked.clone();
        let kwin_connection_for_keys = kwin_connection.clone();

        let key_controller = gtk4::EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key_controller.connect_key_pressed(move |_, key, _, modifiers| {
            let is_escape = key == gtk4::gdk::Key::Escape;
            let is_ctrl_w = key == gtk4::gdk::Key::w
                && modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
            if !is_escape && !is_ctrl_w {
                return glib::Propagation::Proceed;
            }
            let window = window_for_keys.clone();
            let click_through = click_through_for_keys.clone();
            let events = events_for_keys.clone();
            let price_check_open = price_check_open_for_keys.clone();
            let price_check_locked = price_check_locked_for_keys.clone();
            let kwin_connection = kwin_connection_for_keys.clone();
            glib::spawn_future_local(async move {
                shortcuts::close_all_ui(
                    &window,
                    &click_through,
                    &events,
                    &price_check_open,
                    &price_check_locked,
                    &kwin_connection,
                )
                .await;
            });
            glib::Propagation::Stop
        });
        window.add_controller(key_controller);
    }

    let remote_input: Rc<RefCell<Option<remote_input::RemoteInput>>> = Rc::new(RefCell::new(None));
    {
        let window = window.clone();
        let click_through = click_through.clone();
        let kwin_connection = kwin_connection.clone();
        let remote_input = remote_input.clone();
        let events = backend.events.clone();
        let logger = backend.logger.clone();
        let active_window = active_window.clone();
        let ui_request_rx = backend.ui_request_rx.clone();
        let shortcut_config_rx = backend.shortcut_config_rx.clone();
        let restore_clipboard_rx = backend.restore_clipboard_rx.clone();
        let price_check_open = price_check_open.clone();
        let price_check_locked = price_check_locked.clone();
        let visibility =
            overlay_visibility::OverlayVisibility::new(events.clone(), click_through.clone());
        glib::spawn_future_local(async move {
            // Launched from a terminal (no systemd app-scope), so the portal
            // can't derive our app id on its own — register it explicitly
            // first, once, or every app-id-gated portal call (GlobalShortcuts,
            // RemoteDesktop) fails with "An app id is required". The portal
            // errors if this is called twice, so it happens exactly once here
            // rather than inside shortcuts.rs or remote_input.rs.
            if let Err(err) = ashpd::register_host_app(APP_ID.try_into().unwrap()).await {
                let msg = format!("[main] failed to register host app: {err}");
                eprintln!("{msg}");
                logger.write(&msg);
                return;
            }

            match remote_input::RemoteInput::setup(logger.clone(), move |depressed| {
                visibility.on_modifiers(depressed)
            })
            .await
            {
                Ok(ri) => {
                    remote_input.replace(Some(ri));
                }
                Err(err) => {
                    let msg = format!("[main] remote_input setup failed: {err}");
                    eprintln!("{msg}");
                    logger.write(&msg);
                }
            }

            shortcuts::spawn(
                window,
                click_through,
                kwin_connection,
                remote_input,
                events,
                logger,
                active_window,
                ui_request_rx,
                shortcut_config_rx,
                restore_clipboard_rx,
                price_check_open,
                price_check_locked,
            );
        });
    }
}

/// `can-target` only affects GTK's own internal hit-testing, not the input
/// region the compositor is told about — that requires poking the surface
/// directly, or clicks keep landing on our window no matter what.
fn toggle_click_through(
    window: &ApplicationWindow,
    click_through: &Rc<Cell<bool>>,
    events: &server::EventBus,
) {
    set_click_through(!click_through.get(), window, click_through, events);
}

/// Like `toggle_click_through`, but sets an absolute state rather than
/// flipping — needed by anything that decides the *desired* state itself
/// (e.g. `widget_area_tracker`'s mouse-distance logic) rather than reacting
/// to a keypress.
pub(crate) fn set_click_through(
    now_click_through: bool,
    window: &ApplicationWindow,
    click_through: &Rc<Cell<bool>>,
    events: &server::EventBus,
) {
    if click_through.get() == now_click_through {
        return;
    }
    click_through.set(now_click_through);
    apply_click_through_to_surface(now_click_through, window);

    // The renderer only auto-hides "hide-on-blur" popups (e.g. the
    // price-check window) in reaction to this event
    // (renderer/src/web/overlay/OverlayWindow.vue) — without it, anything
    // shown via item-text just stays on screen forever. click-through ON
    // maps to "overlay not active" (game usable, so any lingering popup
    // should go away); click-through OFF maps to "overlay active".
    events.broadcast(
        "MAIN->OVERLAY::focus-change",
        serde_json::json!({
            "game": now_click_through,
            "overlay": !now_click_through,
            "usingHotkey": true,
        }),
    );
}

/// Applies `now_click_through` to the surface's actual Wayland input region,
/// unconditionally — split out of `set_click_through` so callers that need
/// to force a re-apply (the surface was remapped and may have silently
/// dropped its input region) can do so without `set_click_through`'s
/// early-return-if-state-unchanged guard getting in the way.
fn apply_click_through_to_surface(now_click_through: bool, window: &ApplicationWindow) {
    // OnDemand lets the compositor give this surface keyboard focus when
    // something in it is clicked (e.g. a text field), without us stealing
    // focus from the game while click-through is on and nothing of ours is
    // meant to be interactive.
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
/// where the game actually is on a multi-monitor setup. gtk4-layer-shell
/// supports changing this on an already-mapped surface, but its own docs
/// warn it "gets remapped so the change can take effect" — that remap was
/// silently resetting the Wayland input region set for click-through,
/// leaving the overlay fully click-blocking after every monitor switch even
/// though our tracked state still said click-through was on. Re-applying
/// the input region right after fixes it; deferred to idle so the remap
/// (which isn't necessarily synchronous with `set_monitor()` returning) has
/// actually landed first.
fn move_overlay_to_window_monitor(
    window: &ApplicationWindow,
    target: &kwin_tracker::WindowEvent,
    click_through: &Rc<Cell<bool>>,
) {
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
                println!("[main] moving overlay to PoE's monitor");
                window.set_monitor(Some(&monitor));

                let window = window.clone();
                let click_through = click_through.clone();
                glib::idle_add_local_once(move || {
                    apply_click_through_to_surface(click_through.get(), &window);
                });
            }
            return;
        }
    }
}
