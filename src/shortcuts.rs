use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ashpd::WindowIdentifier;
use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::Session;
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use futures_util::StreamExt;
use gtk4::glib;
use gtk4_layer_shell::LayerShell;

use crate::host_config::{
    ActionKind, ShortcutAction, default_actions, extra_registerable_actions,
    shortcut_id,
};
use crate::ipc::{MainEvent, Point};
use crate::remote_input::{MOD_ALT, MOD_CTRL, RemoteInput};
use crate::{Overlay, with_timeout};

pub fn spawn(overlay: Rc<Overlay>, shortcuts_rx: async_channel::Receiver<Vec<ShortcutAction>>) {
    glib::spawn_future_local(async move {
        if let Err(err) = run(&overlay, shortcuts_rx).await {
            overlay.logger.error(&format!("[shortcuts] error: {err}"));
        }
    });
}

/// Every NewShortcut we've ever wanted bound (keyed by id, never shrinks),
/// the current id → action dispatch map, and the trigger KDE actually has
/// bound per id (renderer format, see `host_config::trigger_to_shortcut`).
#[derive(Default)]
struct Bindings {
    all_shortcuts: HashMap<String, NewShortcut>,
    dispatch: HashMap<String, ShortcutAction>,
    triggers: HashMap<String, String>,
}

impl Bindings {
    fn record_triggers(&mut self, shortcuts: &[ashpd::desktop::global_shortcuts::Shortcut]) {
        for s in shortcuts {
            let parsed = crate::host_config::trigger_to_shortcut(s.trigger_description());
            println!(
                "[shortcuts] {} is bound to {:?} (parsed {parsed:?})",
                s.id(),
                s.trigger_description()
            );
            match parsed {
                Some(trigger) => self.triggers.insert(s.id().to_string(), trigger),
                None => self.triggers.remove(s.id()),
            };
        }
    }

    /// The keys to release before injecting input for `id`: what KDE has
    /// bound if we could parse it, else the renderer's configured shortcut
    /// (only right if the user never rebound it in KDE).
    fn hotkey(&self, id: &str, action: &ShortcutAction) -> String {
        self.triggers.get(id).cloned().unwrap_or_else(|| action.shortcut.clone())
    }
}

fn shortcut_description(action: &ActionKind) -> String {
    match action {
        // Matches real APT's own action type name and hotkeys.vue's row
        // label rather than our click-through implementation detail.
        ActionKind::ToggleOverlay => "Toggle overlay".to_string(),
        // Locked/unlocked price-check share target "price-check"; without
        // the suffix both show up in KDE's Shortcuts list under one name.
        ActionKind::CopyItem {
            target,
            focus_overlay: true,
        } => format!("Copy item ({target}, locked)"),
        ActionKind::CopyItem {
            target,
            focus_overlay: false,
        } => format!("Copy item ({target})"),
        ActionKind::TriggerEvent { target } => format!("Widget action ({target})"),
        ActionKind::PasteInChat { text, .. } => format!("Paste in chat ({text})"),
        ActionKind::StashSearch { text } => format!("Stash search ({text})"),
        ActionKind::OcrText | ActionKind::TestOnly | ActionKind::Unsupported => "Unsupported".to_string(),
    }
}

/// Binds any actions not already bound this session and refreshes the
/// dispatch map from `actions`. Confirmed empirically against a live KDE
/// session: the portal's own doc calls BindShortcuts additive, but KDE's
/// kglobalaccel backend replaces this app-id's *entire* persisted grant set
/// with whatever the *latest* call supplied. So the *full* accumulated
/// `all_shortcuts` list is resupplied on every call that has anything new,
/// never just the delta. `dispatch` is replaced wholesale from the current
/// config, so actions dropped from it have no entry left (pressing their
/// old id is a harmless no-op). Ids are trigger-independent (see
/// `host_config::shortcut_id`), so a hotkey change alone touches nothing.
async fn apply_shortcuts(
    proxy: &GlobalShortcuts,
    session: &Session<GlobalShortcuts>,
    window_id: Option<&WindowIdentifier>,
    bindings: &RefCell<Bindings>,
    actions: Vec<ShortcutAction>,
) -> ashpd::Result<()> {
    let mut added_new = false;
    let mut fresh_dispatch = HashMap::new();

    for action in actions {
        let Some(id) = shortcut_id(&action) else {
            continue;
        };
        if !bindings.borrow().all_shortcuts.contains_key(&id) {
            // Empty shortcut (extra_registerable_actions' unset-by-default
            // entries) → no preferred trigger, so it still shows up in KDE's
            // Global Shortcuts settings as a bindable-but-unset entry.
            let trigger = crate::host_config::to_portal_trigger(&action.shortcut);
            let preferred_trigger = (!trigger.is_empty()).then_some(trigger.as_str());
            let new_shortcut = NewShortcut::new(id.clone(), shortcut_description(&action.action))
                .preferred_trigger(preferred_trigger);
            bindings.borrow_mut().all_shortcuts.insert(id.clone(), new_shortcut);
            added_new = true;
        }
        fresh_dispatch.insert(id, action);
    }

    if added_new {
        let full_list: Vec<NewShortcut> = bindings.borrow().all_shortcuts.values().cloned().collect();
        let request = proxy
            .bind_shortcuts(session, &full_list, window_id, BindShortcutsOptions::default())
            .await?;
        bindings.borrow_mut().record_triggers(request.response()?.shortcuts());
    }

    bindings.borrow_mut().dispatch = fresh_dispatch;
    Ok(())
}

/// The portal has been observed emitting multiple Activated signals for one
/// physical keypress — without this, duplicate Ctrl+C injections and proxy
/// requests. Per id, so two different hotkeys pressed in quick succession
/// both still fire.
#[derive(Default)]
struct Debounce {
    last: HashMap<String, Instant>,
}

impl Debounce {
    const WINDOW: Duration = Duration::from_millis(300);

    fn allow(&mut self, id: &str, now: Instant) -> bool {
        if self.last.get(id).is_some_and(|t| now.duration_since(*t) < Self::WINDOW) {
            return false;
        }
        self.last.insert(id.to_string(), now);
        true
    }
}

async fn run(
    overlay: &Rc<Overlay>,
    shortcuts_rx: async_channel::Receiver<Vec<ShortcutAction>>,
) -> ashpd::Result<()> {
    let proxy = GlobalShortcuts::new().await?;
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await?;
    let window_id = WindowIdentifier::from_native(&overlay.window).await;

    // Registers every copy-item target the original renderer UI could ever
    // configure, not just the 3 with a sensible default trigger — our
    // Hotkeys/Item Info tabs no longer expose them, so KDE's own Global
    // Shortcuts settings are the only place a user can find them.
    let bindings = Rc::new(RefCell::new(Bindings::default()));
    let mut initial_actions = default_actions();
    initial_actions.extend(extra_registerable_actions());
    apply_shortcuts(&proxy, &session, window_id.as_ref(), &bindings, initial_actions).await?;

    let mut activated = proxy.receive_activated().await?;

    // The user rebinding a hotkey in KDE System Settings.
    {
        let bindings = bindings.clone();
        let changes = proxy.receive_shortcuts_changed().await?;
        glib::spawn_future_local(async move {
            let mut changes = std::pin::pin!(changes);
            while let Some(change) = changes.next().await {
                bindings.borrow_mut().record_triggers(change.shortcuts());
            }
        });
    }

    // Live-updates the same session's bindings whenever the renderer sends
    // CLIENT->MAIN::update-host-config.
    {
        let bindings = bindings.clone();
        let overlay = overlay.clone();
        glib::spawn_future_local(async move {
            while let Ok(shortcuts) = shortcuts_rx.recv().await {
                let result =
                    apply_shortcuts(&proxy, &session, window_id.as_ref(), &bindings, shortcuts).await;
                if let Err(err) = result {
                    overlay
                        .logger
                        .error(&format!("[shortcuts] failed to apply updated shortcuts: {err}"));
                }
            }
        });
    }

    let mut debounce = Debounce::default();

    while let Some(event) = activated.next().await {
        let id = event.shortcut_id();
        if !debounce.allow(id, Instant::now()) {
            debug!("[shortcuts] Activated id={id} dropped by debounce");
            continue;
        }

        let Some((action, hotkey)) = bindings
            .borrow()
            .dispatch
            .get(id)
            .map(|action| (action.clone(), bindings.borrow().hotkey(id, action)))
        else {
            debug!("[shortcuts] Activated id={id} has no dispatch entry");
            continue;
        };
        match action.action {
            ActionKind::ToggleOverlay => {
                debug!(
                    "[shortcuts] toggle-overlay activated, click_through was {}",
                    overlay.click_through.get()
                );
                // Real APT only registers hotkeys while PoE is focused, so
                // activating over another app is a no-op there. Deactivating
                // stays unconditional so the overlay can't get stuck active.
                if overlay.click_through.get() && !overlay.poe_active() {
                    debug!("[shortcuts] toggle-overlay ignored: active window is not Path of Exile");
                } else {
                    // Shortcuts.ts: areaTracker.removeListeners() first.
                    overlay.auto_close_gen.set(overlay.auto_close_gen.get() + 1);
                    overlay.toggle_click_through();
                }
            }
            ActionKind::CopyItem {
                ref target,
                focus_overlay,
            } => price_check(overlay, target, focus_overlay, &hotkey, action.keep_mod_keys).await,
            ActionKind::TriggerEvent { target } => overlay
                .events()
                .broadcast(&MainEvent::WidgetAction { target }),
            ActionKind::PasteInChat { .. } | ActionKind::StashSearch { .. } => {
                // Real APT has every hotkey unregistered while the overlay
                // is interactive; typing into PoE then would be surprising.
                if overlay.click_through.get() {
                    paste_action(overlay, &action.action, &hotkey).await
                }
            }
            ActionKind::OcrText | ActionKind::TestOnly | ActionKind::Unsupported => {}
        }
    }

    Ok(())
}

/// paste-in-chat / stash-search (text-box.ts), only into a PoE window
/// confirmed focused right now: a stray Enter or Ctrl+A landing in another
/// app is worse than a dropped press. Bounded like price_check, since this
/// runs inline in the hotkey loop.
pub(crate) async fn paste_action(overlay: &Overlay, action: &ActionKind, hotkey: &str) {
    let restore = overlay.restore_clipboard.get();
    let work = async {
        let connection = overlay.kwin()?;
        let (_, _, poe_active) = crate::kwin_tracker::query_cursor_pos(&connection).await.ok()?;
        if !poe_active {
            return None;
        }
        let remote = overlay.remote()?;
        match action {
            ActionKind::PasteInChat { text, send } => {
                remote.type_in_chat(text, *send, hotkey, restore).await
            }
            ActionKind::StashSearch { text } => remote.stash_search(text, hotkey, restore).await,
            _ => {}
        }
        Some(())
    };
    let msg = match with_timeout(Duration::from_secs(2), work).await {
        Some(Some(())) => return,
        Some(None) => "[shortcuts] paste skipped: Path of Exile not focused (or input not ready)",
        None => "[shortcuts] paste timed out (portal or KWin call hung)",
    };
    overlay.logger.error(msg);
}

/// Handles any `copy-item` action: presses Ctrl+C, reads the clipboard, and
/// broadcasts item-text with whatever `target` the bound action specifies
/// (open-wiki, open-poedb, item-check, … all go through this same flow, per
/// renderer/src/web/Config.ts's getConfigForHost — the renderer decides
/// what UI to show per target).
async fn price_check(
    overlay: &Rc<Overlay>,
    target: &str,
    focus_overlay: bool,
    shortcut: &str,
    keep_mod_keys: bool,
) {
    debug!(
        "[shortcuts] price_check called, target={target}, focus_overlay={focus_overlay}, price_check_open={}",
        overlay.price_check_open.get()
    );
    // Locked popup open: the overlay holds keyboard focus, so an injected
    // Ctrl+C would land in our own WebView, not the game — pressing again
    // just closes it and hands control back (real APT has shortcuts
    // unregistered entirely while the overlay is active). An *unlocked*
    // popup falls through to a fresh check like real Shortcuts.ts, which
    // re-reads on every press.
    if overlay.price_check_open.get() && overlay.price_check_locked.get() {
        overlay.hide_price_check();
        overlay.set_click_through(true);
        overlay.price_check_locked.set(false);
        return;
    }

    // The global shortcut fires regardless of which window has focus; only
    // forward it to the game, or it would Ctrl+C whatever is selected in
    // e.g. a browser.
    if !overlay.poe_active() {
        debug!("[shortcuts] price-check ignored: active window is not Path of Exile");
        return;
    }

    // Class-only PoE match before any caption named the game: no renderer
    // to show the result in, nor a way to tell which copy keys it wants.
    let Some(game) = overlay.active.get() else {
        debug!("[shortcuts] price-check ignored: no game identified yet");
        return;
    };

    let started = Instant::now();
    let Some(remote) = overlay.remote() else {
        overlay.logger.error("[shortcuts] price-check: remote input not ready yet");
        return;
    };

    // Cursor lookup (a KWin-scripting round trip) also reports the window
    // that's active *right now*; Ctrl+C is only sent once that confirms PoE.
    // Running it in parallel with the copy was faster but let Ctrl+C land
    // in whatever window had just taken focus (confirmed live: a terminal).
    let cursor_query = async {
        let Some(connection) = overlay.kwin() else {
            overlay.logger.error("[shortcuts] price-check: kwin connection not ready yet");
            return None;
        };
        match crate::kwin_tracker::query_cursor_pos(&connection).await {
            Ok(pos) => Some(pos),
            Err(err) => {
                overlay
                    .logger
                    .error(&format!("[shortcuts] price-check cursor query failed: {err}"));
                None
            }
        }
    };

    // copy_item_text presses Ctrl+C, snapshots + clears the clipboard, then
    // polls (48ms steps, up to ~500ms) for the game's copy to land, via the
    // portal's clipboard (gdk::Clipboard only reflects content while our own
    // window has keyboard focus). Overall cap: some portal calls in there
    // (the clipboard clear/restore writes) have no timeout of their own, and
    // this runs inline in the hotkey loop.
    let work = async {
        let cursor = cursor_query.await;
        // Cursor query failed → can't re-verify; fall back to the cached
        // check above rather than breaking price-check outright.
        if cursor.is_some_and(|(_, _, poe_active)| !poe_active) {
            return None;
        }
        // Off the main thread: a file read plus /proc lookups.
        let show_mods_key = if game.holds_show_mods_key() {
            let configured = overlay.game_config.borrow().clone();
            let pid = overlay.active_window.borrow().as_ref().map(|w| w.pid);
            let logger = overlay.logger.clone();
            gtk4::gio::spawn_blocking(move || crate::game_config::show_mods_key(&configured, pid, &logger))
                .await
                .ok()
        } else {
            None
        };
        let copied = remote
            .copy_item_text(overlay.restore_clipboard.get(), shortcut, keep_mod_keys, show_mods_key.as_deref())
            .await;
        Some((cursor, copied))
    };
    let (cursor, copied) = match with_timeout(Duration::from_secs(2), work).await {
        Some(Some(result)) => result,
        Some(None) => {
            debug!("[shortcuts] price-check skipped: focus left Path of Exile before Ctrl+C");
            return;
        }
        None => {
            overlay
                .logger
                .error("[shortcuts] price-check timed out (portal or KWin call hung)");
            return;
        }
    };
    let (x, y, _) = cursor.unwrap_or((0.0, 0.0, true));

    let clipboard = match copied {
        Ok(Some(text)) => text,
        Ok(None) => {
            debug!("[shortcuts] price-check clipboard: empty");
            return;
        }
        Err(err) => {
            overlay
                .logger
                .error(&format!("[shortcuts] price-check clipboard read failed: {err}"));
            return;
        }
    };
    debug!(
        "[shortcuts] price-check clipboard read ({} chars) in {}ms, sending item-text at ({x}, {y})",
        clipboard.len(),
        started.elapsed().as_millis()
    );

    // focusOverlay matches the real app's hotkey semantics
    // (PriceCheckWindow.vue): plain Ctrl+D auto-closes on mouse-move-away
    // (spawn_auto_close), Ctrl+Alt+D ("hotkeyLocked") opens interactive.
    overlay.events().clear_track_area();
    overlay.events().send_last_active(&MainEvent::ItemText {
        target: target.to_string(),
        clipboard,
        position: Point { x, y },
        focus_overlay,
    });
    overlay.price_check_open.set(true);
    // Every copy-item stops the previous popup tracker (Shortcuts.ts's
    // areaTracker.removeListeners() before item-text). Otherwise an
    // unlocked popup's tracker would see the overlay go interactive for a
    // new locked popup, find the cursor outside its old area, and close it.
    overlay.auto_close_gen.set(overlay.auto_close_gen.get() + 1);
    // Mirrors Shortcuts.ts's `action.focusOverlay && this.overlay.wasUsedRecently`:
    // if a plain browser tab (tray's "Open in Browser") got the item-text,
    // there's no overlay popup to interact with, so don't steal input from
    // the game.
    if focus_overlay && overlay.events().overlay_was_used_recently() {
        overlay.set_click_through(false);
        overlay.price_check_locked.set(true);
    } else if target == "price-check" {
        // Upstream only tracks once the popup reports its area, which only
        // the price-check popup does (PriceCheckWindow.vue). open-wiki,
        // open-poedb, … show no popup, so there is nothing to auto-close.
        spawn_auto_close(overlay.clone(), x, y);
    }
}

/// Upstream's threshold is the renderer's closeThreshold (2.5 * font size,
/// ~40px); kept at a more generous 80px so small mouse drift while reading
/// doesn't close the popup prematurely.
const CLOSE_THRESHOLD: f64 = 80.0;

/// The price-check `holdKey` (track-area's "Ctrl"/"Alt") currently held,
/// from the seat's depressed-modifier mask; Ctrl wins if both are.
fn held_modifier(depressed: u32) -> &'static str {
    if depressed & MOD_CTRL != 0 {
        "Ctrl"
    } else if depressed & MOD_ALT != 0 {
        "Alt"
    } else {
        ""
    }
}

#[derive(Debug, PartialEq)]
enum TrackerStep {
    Wait,
    /// Hide the popup: the cursor moved away from the item.
    Close,
    /// Make the overlay interactive: the cursor is over the popup.
    Activate,
    /// Hand control back to the game: the cursor left the interactive popup.
    ReturnToGame,
}

/// One poll of WidgetAreaTracker.ts's mousemove handler (see
/// `spawn_auto_close`). `distance` is from where the item was copied.
fn tracker_step(click_through: bool, holding: bool, inside: bool, distance: f64) -> TrackerStep {
    if click_through && !holding {
        if distance > CLOSE_THRESHOLD {
            TrackerStep::Close
        } else {
            TrackerStep::Wait
        }
    } else if inside {
        if click_through {
            TrackerStep::Activate
        } else {
            TrackerStep::Wait
        }
    } else if !click_through {
        TrackerStep::ReturnToGame
    } else {
        TrackerStep::Wait
    }
}

/// Port of main/src/windowing/WidgetAreaTracker.ts's mousemove handler,
/// polled via the same one-shot KWin cursor query used to capture `from`
/// (we have no continuous mousemove stream on Wayland):
/// - overlay not interactive and the price-check `holdKey` not held: close
///   the popup once the cursor moves far enough from where the item was;
/// - otherwise, cursor inside the popup's area (OVERLAY->MAIN::track-area):
///   make the overlay interactive (hold-to-pin / hover-to-interact);
/// - otherwise, if interactive: hand control back to the game and stop.
///
/// ponytail: upstream's mousedown-in-area handler is skipped; a click
/// inside the area is always preceded by a move there, which this covers.
fn spawn_auto_close(overlay: Rc<Overlay>, from_x: f64, from_y: f64) {
    // POLL_INTERVAL: each tick is a full KWin loadScript/run/unloadScript
    // round trip, measured at ~1ms (median, 50 queries). An earlier 40ms
    // attempt felt laggy compositor-wide; if that comes back, return to
    // 80ms. workspace.cursorPosChanged (push-based) doesn't fire on real
    // mouse movement on this KWin version, so polling it is.
    const POLL_INTERVAL: Duration = Duration::from_millis(40);

    // Mirrors WidgetAreaTracker's removeListeners() on every new item-text:
    // a newer check supersedes this loop, or it would close the new popup
    // as soon as the cursor left the *previous* item's position.
    let generation = overlay.auto_close_gen.get() + 1;
    overlay.auto_close_gen.set(generation);
    let superseded = {
        let overlay = overlay.clone();
        move || overlay.auto_close_gen.get() != generation
    };

    glib::spawn_future_local(async move {
        loop {
            glib::timeout_future(POLL_INTERVAL).await;

            if !overlay.price_check_open.get() || superseded() {
                return; // closed some other way, or superseded by a newer check
            }

            let Some(connection) = overlay.kwin() else {
                continue;
            };
            let Ok((x, y, _)) = crate::kwin_tracker::query_cursor_pos(&connection).await else {
                continue;
            };

            // Re-check after the (slow) cursor query: a newer check may have
            // started while it was in flight, and closing now would hide
            // *that* popup.
            if superseded() {
                return;
            }

            let track = overlay.events().track_area();
            let mods = overlay.remote().map_or(0, |r: Rc<RemoteInput>| r.depressed_mods());
            let holding = track
                .as_ref()
                .is_some_and(|t| t.hold_key == held_modifier(mods));
            // `area` is relative to the overlay window, which covers its
            // monitor; the cursor is in global compositor coordinates.
            let (ox, oy) = overlay.window.monitor().map_or((0.0, 0.0), |m| {
                let geo = gtk4::prelude::MonitorExt::geometry(&m);
                (geo.x() as f64, geo.y() as f64)
            });
            let inside = track
                .as_ref()
                .is_some_and(|t| t.area.contains(x - ox, y - oy));

            let distance = ((x - from_x).powi(2) + (y - from_y).powi(2)).sqrt();
            match tracker_step(overlay.click_through.get(), holding, inside, distance) {
                TrackerStep::Wait => {}
                TrackerStep::Close => {
                    debug!("[shortcuts] auto-close: cursor moved {distance:.0}px");
                    overlay.hide_price_check();
                    return;
                }
                TrackerStep::Activate => {
                    debug!("[shortcuts] popup tracker: cursor entered popup, activating overlay");
                    overlay.set_click_through(false);
                    // Same state as a locked popup: the overlay owns input,
                    // so the next hotkey press closes instead of copying.
                    overlay.price_check_locked.set(true);
                }
                TrackerStep::ReturnToGame => {
                    debug!("[shortcuts] popup tracker: cursor left popup, returning to game");
                    overlay.close_all_ui().await;
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_is_per_hotkey() {
        let mut d = Debounce::default();
        let t = Instant::now();
        assert!(d.allow("a", t));
        assert!(!d.allow("a", t + Duration::from_millis(100)), "duplicate signal");
        assert!(d.allow("b", t + Duration::from_millis(100)), "other hotkey");
        assert!(d.allow("a", t + Duration::from_millis(400)), "after the window");
    }

    #[test]
    fn held_modifier_prefers_ctrl() {
        assert_eq!(held_modifier(0), "");
        assert_eq!(held_modifier(MOD_ALT), "Alt");
        assert_eq!(held_modifier(MOD_CTRL), "Ctrl");
        assert_eq!(held_modifier(MOD_CTRL | MOD_ALT), "Ctrl");
        assert_eq!(held_modifier(crate::remote_input::MOD_SHIFT), "");
    }

    #[test]
    fn tracker_closes_unpinned_popup_once_cursor_moves_away() {
        assert_eq!(tracker_step(true, false, false, 10.0), TrackerStep::Wait);
        assert_eq!(tracker_step(true, false, false, CLOSE_THRESHOLD + 1.0), TrackerStep::Close);
        // Not holding: moving onto the popup still counts as moving away
        // (upstream only pins via holdKey or an interactive overlay).
        assert_eq!(tracker_step(true, false, true, CLOSE_THRESHOLD + 1.0), TrackerStep::Close);
    }

    #[test]
    fn tracker_hold_to_pin_and_hover_to_interact() {
        // Holding the holdKey: never closes on distance.
        assert_eq!(tracker_step(true, true, false, 1000.0), TrackerStep::Wait);
        // ... and entering the popup makes it interactive.
        assert_eq!(tracker_step(true, true, true, 1000.0), TrackerStep::Activate);
        // Interactive and still over it: stay.
        assert_eq!(tracker_step(false, false, true, 1000.0), TrackerStep::Wait);
        assert_eq!(tracker_step(false, true, true, 1000.0), TrackerStep::Wait);
        // Interactive and the cursor left: back to the game.
        assert_eq!(tracker_step(false, false, false, 0.0), TrackerStep::ReturnToGame);
        assert_eq!(tracker_step(false, true, false, 0.0), TrackerStep::ReturnToGame);
    }
}
