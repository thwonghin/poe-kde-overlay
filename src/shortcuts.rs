use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use ashpd::WindowIdentifier;
use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::Session;
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use futures_util::StreamExt;
use gtk4::{ApplicationWindow, glib};
use serde_json::json;

use crate::host_config::{
    ActionKind, ShortcutAction, default_actions, extra_registerable_actions, shortcut_id,
};
use crate::kwin_tracker::WindowEvent;
use crate::logger::Logger;
use crate::remote_input::RemoteInput;
use crate::server::EventBus;
use crate::{set_click_through, toggle_click_through};

pub fn spawn(
    window: ApplicationWindow,
    click_through: Rc<Cell<bool>>,
    kwin_connection: Rc<RefCell<Option<zbus::Connection>>>,
    remote_input: Rc<RefCell<Option<RemoteInput>>>,
    events: Arc<EventBus>,
    logger: Arc<Logger>,
    active_window: Rc<RefCell<Option<WindowEvent>>>,
    focus_game_rx: async_channel::Receiver<()>,
    shortcut_config_rx: async_channel::Receiver<Vec<ShortcutAction>>,
    restore_clipboard_rx: async_channel::Receiver<bool>,
    price_check_open: Rc<Cell<bool>>,
    price_check_locked: Rc<Cell<bool>>,
) {
    {
        let window = window.clone();
        let click_through = click_through.clone();
        let events = events.clone();
        let price_check_open = price_check_open.clone();
        let price_check_locked = price_check_locked.clone();
        let kwin_connection = kwin_connection.clone();
        glib::spawn_future_local(async move {
            // The renderer's own title-bar "x" button sends this instead of
            // just hiding itself locally (main/src/windowing/OverlayWindow.ts's
            // assertGameActive handler for the same event) — it expects us to
            // actually restore click-through and close whatever's open.
            while focus_game_rx.recv().await.is_ok() {
                close_all_ui(
                    &window,
                    &click_through,
                    &events,
                    &price_check_open,
                    &price_check_locked,
                    &kwin_connection,
                )
                .await;
            }
        });
    }

    let restore_clipboard: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    {
        let restore_clipboard = restore_clipboard.clone();
        glib::spawn_future_local(async move {
            while let Ok(value) = restore_clipboard_rx.recv().await {
                restore_clipboard.set(value);
            }
        });
    }

    glib::spawn_future_local(async move {
        if let Err(err) = run(
            &window,
            &click_through,
            &kwin_connection,
            &remote_input,
            &events,
            &logger,
            &price_check_open,
            &price_check_locked,
            &active_window,
            shortcut_config_rx,
            &restore_clipboard,
        )
        .await
        {
            eprintln!("[shortcuts] error: {err}");
        }
    });
}

/// Force click-through back on and close any open overlay widgets, as an
/// always-available safety hatch regardless of what's currently showing.
/// Matches real APT's `assertGameActive` (main/src/windowing/OverlayWindow.ts).
pub(crate) async fn close_all_ui(
    window: &ApplicationWindow,
    click_through: &Rc<Cell<bool>>,
    events: &Arc<EventBus>,
    price_check_open: &Rc<Cell<bool>>,
    price_check_locked: &Rc<Cell<bool>>,
    kwin_connection: &Rc<RefCell<Option<zbus::Connection>>>,
) {
    println!(
        "[shortcuts] close_all_ui called, price_check_open={}, price_check_locked={}, click_through was {}",
        price_check_open.get(),
        price_check_locked.get(),
        click_through.get()
    );
    if price_check_open.get() {
        events.broadcast(
            "MAIN->OVERLAY::hide-exclusive-widget",
            serde_json::Value::Null,
        );
        price_check_open.set(false);
    }
    price_check_locked.set(false);
    set_click_through(true, window, click_through, events);

    // Restoring click-through doesn't hand real OS focus back to PoE on its
    // own -- see activate_path_of_exile's doc comment for why. Best-effort:
    // a failure here just means the user has to click the game once more
    // themselves, not a reason to abort the rest of this cleanup.
    let connection = kwin_connection.borrow().clone();
    if let Some(connection) = connection {
        if let Err(err) = crate::kwin_tracker::activate_path_of_exile(&connection).await {
            eprintln!("[shortcuts] failed to re-activate Path of Exile: {err}");
        }
    }
}

/// Binds any actions not already bound this session and refreshes the
/// dispatch map from `actions`. Confirmed empirically against a live KDE
/// session (checking ~/.config/kglobalshortcutsrc after two separate
/// bind_shortcuts calls): the portal's own doc calls BindShortcuts additive,
/// but KDE's kglobalaccel backend actually replaces this app-id's *entire*
/// persisted grant set with whatever's supplied in the *latest* call — a
/// second call with only the newly-added shortcuts silently dropped every
/// previously-bound one. So `all_shortcuts` accumulates every NewShortcut
/// we've ever wanted bound (keyed by id, never shrinks) and the *full*
/// accumulated list is resupplied on every call that has anything new,
/// never just the delta. `dispatch` is wholesale-replaced each call from
/// the current config only, so actions dropped from it simply have no
/// entry left to look up (their old id, if pressed, is a harmless no-op).
/// Since `host_config::shortcut_id` is now trigger-independent (stable per
/// action, not per hotkey — see its doc comment), a "dropped" id here means
/// the action itself disappeared from config (e.g. a widget got disabled),
/// not that its hotkey merely changed — changing the hotkey no longer
/// touches `all_shortcuts`/`dispatch` at all, since the id doesn't change.
async fn apply_shortcuts(
    proxy: &GlobalShortcuts,
    session: &Session<GlobalShortcuts>,
    window_id: Option<&WindowIdentifier>,
    all_shortcuts: &Rc<RefCell<HashMap<String, NewShortcut>>>,
    dispatch: &Rc<RefCell<HashMap<String, ShortcutAction>>>,
    actions: Vec<ShortcutAction>,
) -> ashpd::Result<()> {
    let mut added_new = false;
    let mut fresh_dispatch = HashMap::new();

    for action in actions {
        let Some(id) = shortcut_id(&action) else {
            continue;
        };
        if !all_shortcuts.borrow().contains_key(&id) {
            let description = match &action.action {
                // Matches real APT's own action type name ('toggle-overlay')
                // and the row's own label in hotkeys.vue ("Overlay") rather
                // than leaking our internal click-through implementation
                // detail into KDE's user-facing Shortcuts list.
                ActionKind::ToggleOverlay => "Toggle overlay".to_string(),
                // Locked/unlocked price-check share the same target
                // ("price-check") — without the focus_overlay suffix both
                // would show up in KDE's own Shortcuts list under the
                // identical name "Copy item (price-check)", indistinguishable
                // from each other there.
                ActionKind::CopyItem {
                    target,
                    focus_overlay: true,
                } => format!("Copy item ({target}, locked)"),
                ActionKind::CopyItem {
                    target,
                    focus_overlay: false,
                } => format!("Copy item ({target})"),
                ActionKind::TriggerEvent { target } => format!("Widget action ({target})"),
                ActionKind::Unsupported => unreachable!(),
            };
            // Empty shortcut (extra_registerable_actions' unset-by-default
            // entries) -> empty trigger -> register with no preferred
            // trigger at all, so it still shows up in KDE's own Global
            // Shortcuts settings as a nameable, bindable-but-currently-unset
            // entry rather than silently not existing there.
            let trigger = crate::host_config::to_portal_trigger(&action.shortcut);
            let preferred_trigger = if trigger.is_empty() { None } else { Some(trigger.as_str()) };
            let new_shortcut = NewShortcut::new(id.clone(), description).preferred_trigger(preferred_trigger);
            all_shortcuts.borrow_mut().insert(id.clone(), new_shortcut);
            added_new = true;
        }
        fresh_dispatch.insert(id, action);
    }

    if added_new {
        let full_list: Vec<NewShortcut> = all_shortcuts.borrow().values().cloned().collect();
        let request = proxy
            .bind_shortcuts(
                session,
                &full_list,
                window_id,
                BindShortcutsOptions::default(),
            )
            .await?;
        let bound = request.response()?;
        for s in bound.shortcuts() {
            println!(
                "[shortcuts] bound {} -> {}",
                s.id(),
                s.trigger_description()
            );
        }
    }

    *dispatch.borrow_mut() = fresh_dispatch;
    Ok(())
}

async fn run(
    window: &ApplicationWindow,
    click_through: &Rc<Cell<bool>>,
    kwin_connection: &Rc<RefCell<Option<zbus::Connection>>>,
    remote_input: &Rc<RefCell<Option<RemoteInput>>>,
    events: &Arc<EventBus>,
    logger: &Arc<Logger>,
    price_check_open: &Rc<Cell<bool>>,
    price_check_locked: &Rc<Cell<bool>>,
    active_window: &Rc<RefCell<Option<WindowEvent>>>,
    shortcut_config_rx: async_channel::Receiver<Vec<ShortcutAction>>,
    restore_clipboard: &Rc<Cell<bool>>,
) -> ashpd::Result<()> {
    let proxy = GlobalShortcuts::new().await?;
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await?;
    let window_id = WindowIdentifier::from_native(window).await;

    let all_shortcuts: Rc<RefCell<HashMap<String, NewShortcut>>> = Rc::new(RefCell::new(HashMap::new()));
    let dispatch: Rc<RefCell<HashMap<String, ShortcutAction>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Registers every copy-item target the original renderer UI could ever
    // configure, not just the 3 with a sensible default trigger — otherwise,
    // since our Hotkeys/Item Info tabs no longer expose a way to set them,
    // there'd be no way to ever discover or enable them via KDE's own
    // Global Shortcuts settings either.
    let mut initial_actions = default_actions();
    initial_actions.extend(extra_registerable_actions());
    apply_shortcuts(
        &proxy,
        &session,
        window_id.as_ref(),
        &all_shortcuts,
        &dispatch,
        initial_actions,
    )
    .await?;

    // Debounce: the portal has been observed emitting multiple Activated
    // signals for what looked like a single physical keypress (several
    // rapid modifier transitions logged for one press during testing) —
    // without this, that means duplicate Ctrl+C injections and duplicate
    // proxy/network requests per press.
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);
    let mut last_activation: Option<std::time::Instant> = None;

    let mut activated = proxy.receive_activated().await?;

    // Live-updates the same session's bindings whenever the renderer sends
    // a new CLIENT->MAIN::update-host-config — moved into its own task since
    // it needs to hold proxy/session for the rest of the app's lifetime,
    // same as the activation loop below.
    {
        let all_shortcuts = all_shortcuts.clone();
        let dispatch = dispatch.clone();
        let logger = logger.clone();
        glib::spawn_future_local(async move {
            while let Ok(actions) = shortcut_config_rx.recv().await {
                if let Err(err) = apply_shortcuts(
                    &proxy,
                    &session,
                    window_id.as_ref(),
                    &all_shortcuts,
                    &dispatch,
                    actions,
                )
                .await
                {
                    let msg = format!("[shortcuts] failed to apply updated shortcuts: {err}");
                    eprintln!("{msg}");
                    logger.write(&msg);
                }
            }
        });
    }

    while let Some(event) = activated.next().await {
        let now = std::time::Instant::now();
        if last_activation.is_some_and(|t| now.duration_since(t) < DEBOUNCE) {
            println!("[shortcuts] Activated id={} dropped by debounce", event.shortcut_id());
            continue;
        }
        last_activation = Some(now);

        let Some(action) = dispatch.borrow().get(event.shortcut_id()).cloned() else {
            println!(
                "[shortcuts] Activated id={} has no dispatch entry ({} known)",
                event.shortcut_id(),
                dispatch.borrow().len()
            );
            continue;
        };
        match action.action {
            ActionKind::ToggleOverlay => {
                println!(
                    "[shortcuts] toggle-overlay activated, click_through was {}",
                    click_through.get()
                );
                // Real APT only registers hotkeys while PoE is focused (and
                // handles overlayKey locally while the overlay is active), so
                // activating over another app is a no-op there. Deactivating
                // stays unconditional so the overlay can't get stuck active.
                let poe_active = active_window
                    .borrow()
                    .as_ref()
                    .is_some_and(WindowEvent::is_path_of_exile);
                if click_through.get() && !poe_active {
                    println!("[shortcuts] toggle-overlay ignored: active window is not Path of Exile");
                } else {
                    toggle_click_through(window, click_through, events)
                }
            }
            ActionKind::CopyItem {
                target,
                focus_overlay,
            } => {
                price_check(
                    window,
                    click_through,
                    kwin_connection,
                    remote_input,
                    events,
                    logger,
                    price_check_open,
                    price_check_locked,
                    active_window,
                    &target,
                    focus_overlay,
                    &action.shortcut,
                    restore_clipboard,
                )
                .await
            }
            ActionKind::TriggerEvent { target } => {
                events.broadcast("MAIN->CLIENT::widget-action", json!({ "target": target }))
            }
            ActionKind::Unsupported => {}
        }
    }

    Ok(())
}

thread_local! {
    static AUTO_CLOSE_GEN: Cell<u64> = const { Cell::new(0) };
}

/// Handles any `copy-item` action: presses Ctrl+C, reads the clipboard, and
/// broadcasts item-text with whatever `target` the bound action specifies
/// (real APT's own copy-item targets aren't just "price-check" — open-wiki,
/// open-craft-of-exile, open-poedb, search-similar, and item-check all go
/// through this same flow, per renderer/src/web/Config.ts's
/// getConfigForHost — the renderer decides what UI to show per target).
async fn price_check(
    window: &ApplicationWindow,
    click_through: &Rc<Cell<bool>>,
    kwin_connection: &Rc<RefCell<Option<zbus::Connection>>>,
    remote_input: &Rc<RefCell<Option<RemoteInput>>>,
    events: &Arc<EventBus>,
    logger: &Arc<Logger>,
    price_check_open: &Rc<Cell<bool>>,
    price_check_locked: &Rc<Cell<bool>>,
    active_window: &Rc<RefCell<Option<WindowEvent>>>,
    target: &str,
    focus_overlay: bool,
    shortcut: &str,
    restore_clipboard: &Rc<Cell<bool>>,
) {
    println!(
        "[shortcuts] price_check called, target={target}, focus_overlay={focus_overlay}, price_check_open={}",
        price_check_open.get()
    );
    // Locked popup open: the overlay holds keyboard focus, so an injected
    // Ctrl+C would land in our own WebView, not the game — pressing again
    // just closes it and hands control back (real APT has shortcuts
    // unregistered entirely while the overlay is active). An *unlocked*
    // popup falls through to a fresh check like real Shortcuts.ts, which
    // re-reads on every press; toggling it closed here used to swallow
    // every second press (e.g. hovering a second item while the first
    // popup was still up).
    if price_check_open.get() && price_check_locked.get() {
        events.broadcast(
            "MAIN->OVERLAY::hide-exclusive-widget",
            serde_json::Value::Null,
        );
        price_check_open.set(false);
        set_click_through(true, window, click_through, events);
        price_check_locked.set(false);
        return;
    }

    // The global shortcut fires regardless of which window has focus; without
    // this check, pressing it over any other app (browser, terminal, ...)
    // would Ctrl+C-and-read whatever text is selected there. Only forward the
    // hotkey to the game.
    let is_poe = active_window
        .borrow()
        .as_ref()
        .is_some_and(WindowEvent::is_path_of_exile);
    if !is_poe {
        println!("[shortcuts] price-check ignored: active window is not Path of Exile");
        return;
    }

    let started = std::time::Instant::now();
    let remote_ref = remote_input.borrow();
    let Some(remote) = remote_ref.as_ref() else {
        let msg = "[shortcuts] price-check: remote input not ready yet";
        eprintln!("{msg}");
        logger.write(msg);
        return;
    };

    // Cursor lookup (a KWin-scripting round trip) also reports the window
    // that's active *right now*; Ctrl+C is only sent once that confirms PoE.
    // Running it in parallel with the copy was faster but let Ctrl+C land
    // in whatever window had just taken focus (confirmed live: a terminal).
    let cursor_query = async {
        let connection = kwin_connection.borrow().clone();
        match connection {
            Some(connection) => match crate::kwin_tracker::query_cursor_pos(&connection).await {
                Ok(pos) => Some(pos),
                Err(err) => {
                    let msg = format!("[shortcuts] price-check cursor query failed: {err}");
                    eprintln!("{msg}");
                    logger.write(&msg);
                    None
                }
            },
            None => {
                let msg = "[shortcuts] price-check: kwin connection not ready yet";
                eprintln!("{msg}");
                logger.write(msg);
                None
            }
        }
    };

    // copy_item_text presses Ctrl+C, snapshots + clears the clipboard, then
    // polls (48ms steps, up to ~500ms) for the game's copy to land. Uses the
    // portal's clipboard rather than gdk::Clipboard, which only reliably
    // reflects content while our own window has keyboard focus.
    //
    // Overall cap: some portal calls in copy_item_text (e.g. the clipboard
    // clear/restore writes) have no timeout of their own, and this runs
    // inline in the hotkey loop — one hung call used to freeze every later
    // hotkey until restart. Confirmed live once after a portal read hang.
    const PRICE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    let work = std::pin::pin!(async {
        let cursor = cursor_query.await;
        // Cursor query failed → can't re-verify; fall back to the cached
        // check above rather than breaking price-check outright.
        if cursor.is_some_and(|(_, _, poe_active)| !poe_active) {
            return None;
        }
        remote.release_shortcut_keys(shortcut);
        Some((cursor, remote.copy_item_text(restore_clipboard.get()).await))
    });
    let timeout = std::pin::pin!(glib::timeout_future(PRICE_CHECK_TIMEOUT));
    let (cursor, copied) = match futures_util::future::select(work, timeout).await {
        futures_util::future::Either::Left((Some(result), _)) => result,
        futures_util::future::Either::Left((None, _)) => {
            println!("[shortcuts] price-check skipped: focus left Path of Exile before Ctrl+C");
            return;
        }
        futures_util::future::Either::Right(_) => {
            let msg = "[shortcuts] price-check timed out (portal or KWin call hung)";
            eprintln!("{msg}");
            logger.write(msg);
            return;
        }
    };
    let (x, y, _) = cursor.unwrap_or((0.0, 0.0, true));

    let clipboard = match copied {
        Ok(Some(text)) => text,
        Ok(None) => {
            println!("[shortcuts] price-check clipboard: empty");
            return;
        }
        Err(err) => {
            let msg = format!("[shortcuts] price-check clipboard read failed: {err}");
            eprintln!("{msg}");
            logger.write(&msg);
            return;
        }
    };
    println!(
        "[shortcuts] price-check clipboard read ({} chars) in {}ms, sending item-text at ({x}, {y})",
        clipboard.len(),
        started.elapsed().as_millis()
    );

    // `target` is whatever the bound action's config specifies (e.g. the
    // renderer's PriceCheckWindow.vue listens for target:"price-check").
    // focusOverlay matches the real app's own hotkey semantics (renderer/
    // src/web/price-check/PriceCheckWindow.vue): plain Ctrl+D sends
    // focusOverlay:false and auto-closes on mouse-move-away; Ctrl+Alt+D
    // ("hotkeyLocked") sends focusOverlay:true, which skips that (real
    // APT's own hover-to-interact replacement for the auto-close path —
    // ours is a simpler Rust-side poll, see spawn_auto_close).
    events.send_last_active(
        "MAIN->CLIENT::item-text",
        json!({
            "target": target,
            "clipboard": clipboard,
            "position": { "x": x, "y": y },
            "focusOverlay": focus_overlay,
        }),
    );
    price_check_open.set(true);
    if focus_overlay && events.overlay_was_used_recently() {
        // The locked popup is meant to be interacted with (read, click,
        // copy) without the game stealing clicks underneath it. Only do
        // this when the overlay itself (not a plain browser tab opened via
        // the tray's "Open in Browser") was the last client to report
        // activity — otherwise the item-text payload above went to that
        // browser tab, not anything the overlay is showing, and forcing
        // click-through off here would just steal input from the game for
        // no visible popup. Mirrors real Shortcuts.ts's own single
        // condition (`action.focusOverlay && this.overlay.wasUsedRecently`).
        set_click_through(false, window, click_through, events);
        price_check_locked.set(true);
    } else {
        spawn_auto_close(
            x,
            y,
            kwin_connection.clone(),
            events.clone(),
            price_check_open.clone(),
        );
    }
}

/// Ported from main/src/windowing/WidgetAreaTracker.ts: closes the popup
/// once the cursor has moved far enough from where the item was originally
/// hovered. Polls via the same one-shot KWin cursor query used to capture
/// `from` in the first place, rather than the real implementation's
/// continuous uiohook mousemove stream (which we don't have on Wayland).
fn spawn_auto_close(
    from_x: f64,
    from_y: f64,
    kwin_connection: Rc<RefCell<Option<zbus::Connection>>>,
    events: Arc<EventBus>,
    price_check_open: Rc<Cell<bool>>,
) {
    // The real default (2.5 * font size, ~50px) is paired with a
    // hold-Ctrl-to-pin escape hatch we don't have; a bit more generous here
    // so small mouse drift while reading doesn't close it prematurely, but
    // not so generous it feels laggy to dismiss.
    //
    // POLL_INTERVAL was tried at 40ms for a snappier close, but each tick
    // costs a full KWin loadScript/run/unloadScript D-Bus round trip plus
    // temp-file I/O (query_cursor_pos is a one-shot mechanism, not meant for
    // hot-loop polling) -- 25 of those a second was enough to make the whole
    // compositor feel laggy, not just this popup. A push-based replacement
    // (workspace.cursorPosChanged) was tried and reverted: the signal exists
    // but doesn't actually fire on real mouse movement on this KWin version,
    // so cursor tracking silently stopped updating and the popup never
    // auto-closed at all. Back to 80ms, the original value, which never had
    // either problem.
    const CLOSE_THRESHOLD: f64 = 80.0;
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(80);

    // Mirrors WidgetAreaTracker's removeListeners() on every new item-text:
    // a newer check supersedes this loop, or it would close the new popup
    // as soon as the cursor left the *previous* item's position.
    let generation = AUTO_CLOSE_GEN.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });

    glib::spawn_future_local(async move {
        loop {
            glib::timeout_future(POLL_INTERVAL).await;

            if !price_check_open.get() || AUTO_CLOSE_GEN.with(Cell::get) != generation {
                return; // closed some other way, or superseded by a newer check
            }

            let connection = kwin_connection.borrow().clone();
            let Some(connection) = connection else {
                continue;
            };
            let Ok((x, y, _)) = crate::kwin_tracker::query_cursor_pos(&connection).await else {
                continue;
            };

            // Re-check after the (slow) cursor query: a newer check may have
            // started while it was in flight, and closing now would hide
            // *that* popup.
            if AUTO_CLOSE_GEN.with(Cell::get) != generation {
                return;
            }
            let distance = ((x - from_x).powi(2) + (y - from_y).powi(2)).sqrt();
            if distance > CLOSE_THRESHOLD {
                println!("[shortcuts] auto-close: cursor moved {distance:.0}px");
                events.broadcast(
                    "MAIN->OVERLAY::hide-exclusive-widget",
                    serde_json::Value::Null,
                );
                price_check_open.set(false);
                return;
            }
        }
    });
}
