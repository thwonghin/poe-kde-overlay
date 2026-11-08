// Parses the subset of the renderer's HostConfig (ipc/types.ts) we act on:
// the user's configured hotkey list, the restoreClipboard flag, and
// windowTitle. Everything else in that payload (clientLog/gameConfig/
// stashScroll/overlayKey/logKeys/language) is intentionally not modeled
// here — serde ignores unknown fields by default, so this just picks what
// we use out of the full blob the renderer sends on
// CLIENT->MAIN::update-host-config.

use std::collections::HashSet;
use std::sync::Arc;

use serde::Deserialize;

use crate::logger::Logger;

#[derive(Debug, Clone, Default, Deserialize)]
struct HostConfig {
    #[serde(default)]
    shortcuts: Vec<ShortcutAction>,
    #[serde(default, rename = "restoreClipboard")]
    restore_clipboard: bool,
    #[serde(default, rename = "windowTitle")]
    window_title: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShortcutAction {
    pub shortcut: String,
    pub action: ActionKind,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ActionKind {
    ToggleOverlay,
    #[serde(rename_all = "camelCase")]
    CopyItem {
        target: String,
        #[serde(default)]
        focus_overlay: bool,
    },
    // Real Shortcuts.ts just broadcasts MAIN->CLIENT::widget-action for
    // this (stopwatch start/stop/reset, delve-grid toggle) — no input
    // injection involved.
    TriggerEvent { target: String },
    // ocr-text / stash-search / paste-in-chat / test-only —
    // not implemented yet (need EIS text-injection + clipboard-write
    // primitives that don't exist in remote_input.rs), parsed but inert.
    #[serde(other)]
    Unsupported,
}

// Shortcut.ts's own reserved list (main/src/shortcuts/Shortcuts.ts's
// `updateActions`): these are keys our own copy-item/paste-in-chat/
// stash-search flows inject into the game. Binding one of them globally
// would both break the game's native use of it and collide with our own
// injected keystrokes (e.g. price-check's hotkey set to "Ctrl + C" would
// globally grab the exact key price_check() itself presses to copy text).
const RESERVED: &[&str] = &[
    "Ctrl + C",
    "Ctrl + V",
    "Ctrl + A",
    "Ctrl + F",
    "Ctrl + Enter",
    "Home",
    "Delete",
    "Enter",
    "ArrowUp",
    "ArrowRight",
    "ArrowLeft",
];

fn filter_valid(actions: Vec<ShortcutAction>, logger: &Arc<Logger>) -> Vec<ShortcutAction> {
    let mut seen = HashSet::new();
    actions
        .into_iter()
        .filter(|a| {
            if RESERVED.contains(&a.shortcut.as_str()) {
                let msg = format!(
                    "[host_config] hotkey \"{}\" is reserved by the game, skipping",
                    a.shortcut
                );
                eprintln!("{msg}");
                logger.write(&msg);
                return false;
            }
            true
        })
        .filter(|a| {
            // toggle-overlay is exempt from the dedup drop, matching
            // Shortcuts.ts's own carve-out (updateActions's final filter).
            matches!(a.action, ActionKind::ToggleOverlay) || seen.insert(a.shortcut.clone())
        })
        .collect()
}

/// Translates the renderer's own shortcut string format ("Ctrl + D",
/// "Ctrl + Alt + D", "Shift + Space" — title-case words joined by " + ",
/// built by renderer/src/web/Config.ts's getConfigForHost) into the format
/// ashpd's GlobalShortcuts portal's `preferred_trigger` actually expects:
/// uppercase modifier keywords immediately followed by "+", with the base
/// key lowercased (e.g. "CTRL+ALT+d"). Confirmed empirically against a live
/// KDE session: feeding the renderer's own spaced/title-case string straight
/// into `preferred_trigger` silently fails to bind to any physical key
/// (empty `trigger_description`, no `Activated` ever fires) — this exact
/// uppercase-modifiers/no-spaces/lowercase-key shape is what actually works.
/// Translates the renderer's own key names (`ipc/KeyToCode.ts`'s `KeyToCode`
/// object — a closed, finite vocabulary the hotkey-capture UI can ever
/// produce: single letters/digits, "Space", and named keys like "Enter",
/// "ArrowUp", "F5", "Delete", "Semicolon") into XKB keysym names. Confirmed
/// live: a blanket lowercase (our first attempt) works for single letters
/// and "space" by coincidence, but silently fails to bind for anything else
/// — e.g. "Enter" lowercased is "enter", which isn't a real X11 keysym; the
/// actual keysym for that key is "Return". Named/multi-char keys need exact
/// XKB spelling; only single printable characters (letters, digits, and the
/// few punctuation names below) actually use lowercase.
fn key_to_xkb(key: &str) -> String {
    match key {
        "Space" => "space".to_string(),
        "Enter" => "Return".to_string(),
        "Escape" => "Escape".to_string(),
        "Tab" => "Tab".to_string(),
        "Backspace" => "BackSpace".to_string(),
        "CapsLock" => "Caps_Lock".to_string(),
        "PageUp" => "Page_Up".to_string(),
        "PageDown" => "Page_Down".to_string(),
        "End" => "End".to_string(),
        "Home" => "Home".to_string(),
        "ArrowLeft" => "Left".to_string(),
        "ArrowUp" => "Up".to_string(),
        "ArrowRight" => "Right".to_string(),
        "ArrowDown" => "Down".to_string(),
        "Insert" => "Insert".to_string(),
        "Delete" => "Delete".to_string(),
        "Numpad0" => "KP_0".to_string(),
        "Numpad1" => "KP_1".to_string(),
        "Numpad2" => "KP_2".to_string(),
        "Numpad3" => "KP_3".to_string(),
        "Numpad4" => "KP_4".to_string(),
        "Numpad5" => "KP_5".to_string(),
        "Numpad6" => "KP_6".to_string(),
        "Numpad7" => "KP_7".to_string(),
        "Numpad8" => "KP_8".to_string(),
        "Numpad9" => "KP_9".to_string(),
        "NumpadMultiply" => "KP_Multiply".to_string(),
        "NumpadAdd" => "KP_Add".to_string(),
        "NumpadSubtract" => "KP_Subtract".to_string(),
        "NumpadDecimal" => "KP_Decimal".to_string(),
        "NumpadDivide" => "KP_Divide".to_string(),
        "Semicolon" => "semicolon".to_string(),
        "Equal" => "equal".to_string(),
        "Comma" => "comma".to_string(),
        "Minus" => "minus".to_string(),
        "Period" => "period".to_string(),
        "Slash" => "slash".to_string(),
        "Backquote" => "grave".to_string(),
        "BracketLeft" => "bracketleft".to_string(),
        "Backslash" => "backslash".to_string(),
        "BracketRight" => "bracketright".to_string(),
        "Quote" => "apostrophe".to_string(),
        // F1..F24 already match XKB naming as-is.
        f if f.len() >= 2 && f.starts_with('F') && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            f.to_string()
        }
        // Single letters (A-Z) and digits (0-9): XKB names these lowercase.
        other => other.to_ascii_lowercase(),
    }
}

pub fn to_portal_trigger(shortcut: &str) -> String {
    let tokens: Vec<&str> = shortcut.split(" + ").collect();
    let Some((key, modifiers)) = tokens.split_last() else {
        return String::new();
    };
    modifiers
        .iter()
        .map(|m| m.to_ascii_uppercase())
        .chain(std::iter::once(key_to_xkb(key)))
        .collect::<Vec<_>>()
        .join("+")
}

pub struct ParsedHostConfig {
    pub shortcuts: Vec<ShortcutAction>,
    pub restore_clipboard: bool,
    pub window_title: String,
}

pub fn parse_host_config(payload: &serde_json::Value, logger: &Arc<Logger>) -> ParsedHostConfig {
    let parsed = serde_json::from_value::<HostConfig>(payload.clone()).unwrap_or_else(|err| {
        let msg = format!("[host_config] bad update-host-config payload: {err}");
        eprintln!("{msg}");
        logger.write(&msg);
        HostConfig::default()
    });
    ParsedHostConfig {
        shortcuts: filter_valid(parsed.shortcuts, logger),
        restore_clipboard: parsed.restore_clipboard,
        window_title: parsed.window_title,
    }
}

/// The 3 hotkeys we've always bound, expressed as ShortcutActions so they
/// go through the same ID/dispatch machinery as anything the renderer sends
/// via update-host-config.
pub fn default_actions() -> Vec<ShortcutAction> {
    vec![
        ShortcutAction {
            shortcut: "Shift + Space".to_string(),
            action: ActionKind::ToggleOverlay,
        },
        ShortcutAction {
            shortcut: "Ctrl + D".to_string(),
            action: ActionKind::CopyItem {
                target: "price-check".to_string(),
                focus_overlay: false,
            },
        },
        ShortcutAction {
            shortcut: "Ctrl + Alt + D".to_string(),
            action: ActionKind::CopyItem {
                target: "price-check".to_string(),
                focus_overlay: true,
            },
        },
    ]
}

/// The rest of the copy-item targets the original renderer UI could
/// configure — item-check/settings-item-check.vue's wiki/PoEDB/Craft of
/// Exile/find-in-stash keys, and item-check's own advanced-description
/// hotkey (item-check/settings-item-check.vue, hotkeys.vue) — registered
/// with no default trigger (empty `shortcut`; `to_portal_trigger` maps
/// that to an empty XKB trigger string, which `apply_shortcuts` turns into
/// `preferred_trigger(None)`), same as real APT itself leaves them unset
/// until a user picks a key. Registering them anyway (rather than only on
/// demand, as `update-host-config` used to) is what makes them show up as
/// nameable, bindable entries in KDE's own Global Shortcuts settings at
/// all — since our renderer's Hotkeys/Item Info tabs no longer expose a way
/// to set them, that's now the only place a user could ever discover or
/// enable them.
pub fn extra_registerable_actions() -> Vec<ShortcutAction> {
    let copy_item = |target: &str, focus_overlay: bool| ShortcutAction {
        shortcut: String::new(),
        action: ActionKind::CopyItem {
            target: target.to_string(),
            focus_overlay,
        },
    };
    vec![
        copy_item("item-check", true),
        copy_item("open-wiki", false),
        copy_item("open-poedb", false),
        copy_item("open-craft-of-exile", false),
        copy_item("search-similar", false),
    ]
}

/// Stable, content-derived shortcut id — deliberately independent of
/// `action.shortcut` (the trigger key). Hotkeys are now managed exclusively
/// through KDE's own Global Shortcuts (System Settings, or ConfigureShortcuts)
/// rather than the renderer's Settings > Hotkeys tab, which is hidden at the
/// DOM level (see `HIDE_HOTKEYS_TAB_JS` in main.rs). Once an id is bound, its
/// physical key is entirely KDE's to own — `apply_shortcuts` only ever binds
/// an id the *first* time it's seen (see `all_shortcuts` in shortcuts.rs), so
/// if the id included the trigger, changing the hotkey in KDE and then
/// having the renderer resend its (unrelated, still-original) config would
/// look like "a new id" and mint a second, competing binding for the same
/// action. Keying only on action type + target + focus_overlay avoids that:
/// the same action always maps to the same id no matter what key is
/// currently assigned to it.
pub fn shortcut_id(action: &ShortcutAction) -> Option<String> {
    let slug = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
    };
    match &action.action {
        ActionKind::ToggleOverlay => Some("toggle-overlay".to_string()),
        ActionKind::CopyItem {
            target,
            focus_overlay,
        } => Some(format!("copy-item-{}-{}", slug(target), focus_overlay)),
        ActionKind::TriggerEvent { target } => Some(format!("trigger-event-{}", slug(target))),
        ActionKind::Unsupported => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_logger() -> Arc<Logger> {
        Arc::new(Logger::new(Arc::new(crate::server::EventBus::new().0)))
    }

    #[test]
    fn to_portal_trigger_matches_known_working_defaults() {
        // These 3 exact strings are confirmed live (against a real KDE
        // GlobalShortcuts portal session) to actually bind and fire —
        // anything this function produces for the 3 defaults must match.
        assert_eq!(to_portal_trigger("Shift + Space"), "SHIFT+space");
        assert_eq!(to_portal_trigger("Ctrl + D"), "CTRL+d");
        assert_eq!(to_portal_trigger("Ctrl + Alt + D"), "CTRL+ALT+d");
    }

    #[test]
    fn to_portal_trigger_empty_shortcut_is_empty_trigger() {
        // extra_registerable_actions() registers actions with no default
        // trigger — apply_shortcuts relies on this producing an empty
        // string (which it turns into preferred_trigger(None)), not on
        // any special-cased empty check here.
        assert_eq!(to_portal_trigger(""), "");
    }

    #[test]
    fn extra_registerable_actions_have_unique_ids_distinct_from_defaults() {
        let mut ids: Vec<Option<String>> = default_actions().iter().map(shortcut_id).collect();
        ids.extend(extra_registerable_actions().iter().map(shortcut_id));

        let unique: HashSet<_> = ids.iter().cloned().collect();
        assert_eq!(unique.len(), ids.len(), "expected no duplicate ids among defaults + extras");
        assert!(ids.iter().all(Option::is_some));
    }

    #[test]
    fn to_portal_trigger_maps_named_keys_to_xkb_keysym_names() {
        // "Enter" lowercased is "enter", which isn't a real X11 keysym (the
        // actual name is "Return") — this is exactly the bug a real user
        // hit with "Shift + Enter" as their toggle-overlay key.
        assert_eq!(to_portal_trigger("Shift + Enter"), "SHIFT+Return");
        assert_eq!(to_portal_trigger("Ctrl + Escape"), "CTRL+Escape");
        assert_eq!(to_portal_trigger("Ctrl + ArrowUp"), "CTRL+Up");
        assert_eq!(to_portal_trigger("Ctrl + F5"), "CTRL+F5");
        assert_eq!(to_portal_trigger("Ctrl + Delete"), "CTRL+Delete");
    }

    #[test]
    fn parses_supported_actions_and_skips_unsupported_without_erroring() {
        let payload = serde_json::json!({
            "shortcuts": [
                { "shortcut": "Ctrl + D", "action": { "type": "copy-item", "target": "price-check", "focusOverlay": false } },
                { "shortcut": "Shift + Space", "action": { "type": "toggle-overlay" } },
                { "shortcut": "Ctrl + Shift + S", "action": { "type": "stash-search", "text": "foo" } },
                { "shortcut": "Ctrl + T", "action": { "type": "trigger-event", "target": "delve-grid" } },
            ],
            "restoreClipboard": true,
            "clientLog": null,
            "gameConfig": null,
            "stashScroll": false,
            "overlayKey": "Shift + Space",
            "logKeys": false,
            "windowTitle": "Path of Exile",
            "language": "en",
        });

        let parsed = parse_host_config(&payload, &test_logger());
        assert_eq!(parsed.shortcuts.len(), 4);
        assert!(matches!(
            parsed.shortcuts[0].action,
            ActionKind::CopyItem { ref target, focus_overlay: false } if target == "price-check"
        ));
        assert!(matches!(parsed.shortcuts[1].action, ActionKind::ToggleOverlay));
        assert!(matches!(parsed.shortcuts[2].action, ActionKind::Unsupported));
        assert!(matches!(
            parsed.shortcuts[3].action,
            ActionKind::TriggerEvent { ref target } if target == "delve-grid"
        ));
        assert!(parsed.restore_clipboard);
        assert_eq!(parsed.window_title, "Path of Exile");
    }

    #[test]
    fn restore_clipboard_defaults_to_false_when_omitted() {
        let payload = serde_json::json!({ "shortcuts": [] });
        assert!(!parse_host_config(&payload, &test_logger()).restore_clipboard);
    }

    #[test]
    fn drops_reserved_and_duplicate_shortcuts() {
        let actions = vec![
            ShortcutAction {
                shortcut: "Ctrl + C".to_string(),
                action: ActionKind::CopyItem {
                    target: "price-check".to_string(),
                    focus_overlay: false,
                },
            },
            ShortcutAction {
                shortcut: "Ctrl + D".to_string(),
                action: ActionKind::CopyItem {
                    target: "price-check".to_string(),
                    focus_overlay: false,
                },
            },
            ShortcutAction {
                shortcut: "Ctrl + D".to_string(),
                action: ActionKind::CopyItem {
                    target: "item-check".to_string(),
                    focus_overlay: true,
                },
            },
            ShortcutAction {
                shortcut: "Shift + Space".to_string(),
                action: ActionKind::ToggleOverlay,
            },
            ShortcutAction {
                shortcut: "Shift + Space".to_string(),
                action: ActionKind::ToggleOverlay,
            },
        ];

        let filtered = filter_valid(actions, &test_logger());
        // Ctrl+C dropped (reserved); second Ctrl+D dropped (duplicate
        // shortcut string); both Shift+Space entries kept (toggle-overlay
        // is exempt from the dedup drop).
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].shortcut, "Ctrl + D");
        assert_eq!(filtered[1].shortcut, "Shift + Space");
        assert_eq!(filtered[2].shortcut, "Shift + Space");
    }

    #[test]
    fn shortcut_id_ignores_trigger_but_distinguishes_target_and_focus_overlay() {
        let a = ShortcutAction {
            shortcut: "Ctrl + D".to_string(),
            action: ActionKind::CopyItem {
                target: "price-check".to_string(),
                focus_overlay: false,
            },
        };
        // Same action, different trigger key: same id. KDE (or
        // ConfigureShortcuts) owns rebinding the physical key now — the id
        // must stay put regardless of what trigger the config carries, or
        // a KDE-side rebind followed by an unrelated config resend would
        // look like a brand new action and mint a second, competing bind.
        let same_action_different_key = ShortcutAction {
            shortcut: "Ctrl + E".to_string(),
            action: ActionKind::CopyItem {
                target: "price-check".to_string(),
                focus_overlay: false,
            },
        };
        let different_target = ShortcutAction {
            shortcut: "Ctrl + D".to_string(),
            action: ActionKind::CopyItem {
                target: "item-check".to_string(),
                focus_overlay: false,
            },
        };
        let different_focus_overlay = ShortcutAction {
            shortcut: "Ctrl + D".to_string(),
            action: ActionKind::CopyItem {
                target: "price-check".to_string(),
                focus_overlay: true,
            },
        };

        assert_eq!(shortcut_id(&a), shortcut_id(&same_action_different_key));
        assert_ne!(shortcut_id(&a), shortcut_id(&different_target));
        assert_ne!(shortcut_id(&a), shortcut_id(&different_focus_overlay));
        assert_eq!(
            shortcut_id(&ShortcutAction {
                shortcut: "x".to_string(),
                action: ActionKind::Unsupported
            }),
            None
        );
    }
}
