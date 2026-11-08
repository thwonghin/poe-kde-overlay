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
    /// Shortcuts.ts: only release the trigger's non-modifier key and leave
    /// held modifiers alone (the renderer sets this for the hold-to-pin
    /// price-check hotkey and toggle-overlay).
    #[serde(default, rename = "keepModKeys")]
    pub keep_mod_keys: bool,
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
    // text-box.ts: clipboard write + key-tap sequences (remote_input.rs).
    PasteInChat {
        text: String,
        #[serde(default)]
        send: bool,
    },
    StashSearch { text: String },
    // ocr-text (Windows-only upstream) / test-only — parsed but inert.
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
                logger.error(&format!(
                    "[host_config] hotkey \"{}\" is reserved by the game, skipping",
                    a.shortcut
                ));
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

/// The renderer's named keys (`ipc/KeyToCode.ts`'s closed vocabulary) as
/// `(name, evdev keycode, XKB keysym name)`. Letters, digits and F-keys are
/// computed in `key_to_evdev`/`key_to_xkb` instead of listed.
///
/// XKB names need exact spelling (confirmed live: a blanket lowercase
/// "enter" silently fails to bind; the keysym is "Return"). Evdev codes are
/// linux/input-event-codes.h, what EIS injection takes.
const NAMED_KEYS: &[(&str, u32, &str)] = &[
    ("Ctrl", 29, "Control_L"),
    ("Shift", 42, "Shift_L"),
    ("Alt", 56, "Alt_L"),
    ("Space", 57, "space"),
    ("Enter", 28, "Return"),
    ("Escape", 1, "Escape"),
    ("Tab", 15, "Tab"),
    ("Backspace", 14, "BackSpace"),
    ("CapsLock", 58, "Caps_Lock"),
    ("PageUp", 104, "Page_Up"),
    ("PageDown", 109, "Page_Down"),
    ("End", 107, "End"),
    ("Home", 102, "Home"),
    ("ArrowLeft", 105, "Left"),
    ("ArrowUp", 103, "Up"),
    ("ArrowRight", 106, "Right"),
    ("ArrowDown", 108, "Down"),
    ("Insert", 110, "Insert"),
    ("Delete", 111, "Delete"),
    ("Numpad0", 82, "KP_0"),
    ("Numpad1", 79, "KP_1"),
    ("Numpad2", 80, "KP_2"),
    ("Numpad3", 81, "KP_3"),
    ("Numpad4", 75, "KP_4"),
    ("Numpad5", 76, "KP_5"),
    ("Numpad6", 77, "KP_6"),
    ("Numpad7", 71, "KP_7"),
    ("Numpad8", 72, "KP_8"),
    ("Numpad9", 73, "KP_9"),
    ("NumpadMultiply", 55, "KP_Multiply"),
    ("NumpadAdd", 78, "KP_Add"),
    ("NumpadSubtract", 74, "KP_Subtract"),
    ("NumpadDecimal", 83, "KP_Decimal"),
    ("NumpadDivide", 98, "KP_Divide"),
    ("Semicolon", 39, "semicolon"),
    ("Equal", 13, "equal"),
    ("Comma", 51, "comma"),
    ("Minus", 12, "minus"),
    ("Period", 52, "period"),
    ("Slash", 53, "slash"),
    ("Backquote", 41, "grave"),
    ("BracketLeft", 26, "bracketleft"),
    ("Backslash", 43, "backslash"),
    ("BracketRight", 27, "bracketright"),
    ("Quote", 40, "apostrophe"),
];

fn named_key(key: &str) -> Option<&'static (&'static str, u32, &'static str)> {
    NAMED_KEYS.iter().find(|(name, _, _)| *name == key)
}

fn function_key_number(key: &str) -> Option<u32> {
    key.strip_prefix('F').filter(|n| !n.is_empty())?.parse().ok()
}

/// Renderer key name → evdev keycode.
pub fn key_to_evdev(key: &str) -> Option<u32> {
    const LETTERS: &[u32] = &[
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47,
        17, 45, 21, 44,
    ];
    if let Some(&(_, code, _)) = named_key(key) {
        return Some(code);
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c @ 'A'..='Z'), None) => Some(LETTERS[(c as u8 - b'A') as usize]),
        (Some('0'), None) => Some(11),
        // KEY_1..KEY_9 = 2..10
        (Some(c @ '1'..='9'), None) => Some((c as u8 - b'1') as u32 + 2),
        // F1..F10 = 59..68, F11/F12 = 87/88, F13..F24 = 183..194
        _ => match function_key_number(key)? {
            n @ 1..=10 => Some(58 + n),
            11 => Some(87),
            12 => Some(88),
            n @ 13..=24 => Some(170 + n),
            _ => None,
        },
    }
}

/// Renderer key name → XKB keysym name.
fn key_to_xkb(key: &str) -> String {
    match named_key(key) {
        Some(&(_, _, xkb)) => xkb.to_string(),
        // F1..F24 already match XKB naming as-is.
        None if function_key_number(key).is_some() => key.to_string(),
        // Single letters (A-Z) and digits (0-9): XKB names these lowercase.
        None => key.to_ascii_lowercase(),
    }
}

/// Translates the renderer's shortcut string ("Ctrl + Alt + D", built by
/// renderer/src/web/Config.ts's getConfigForHost) into what the
/// GlobalShortcuts portal's `preferred_trigger` expects: uppercase
/// modifiers joined by "+", then the XKB key name ("CTRL+ALT+d"). Confirmed
/// live: the renderer's own spaced/title-case string silently binds nothing.
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
        logger.error(&format!("[host_config] bad update-host-config payload: {err}"));
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
            keep_mod_keys: true,
            action: ActionKind::ToggleOverlay,
        },
        ShortcutAction {
            shortcut: "Ctrl + D".to_string(),
            keep_mod_keys: true,
            action: ActionKind::CopyItem {
                target: "price-check".to_string(),
                focus_overlay: false,
            },
        },
        ShortcutAction {
            shortcut: "Ctrl + Alt + D".to_string(),
            keep_mod_keys: false,
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
        keep_mod_keys: false,
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

/// Stable across runs and Rust versions, unlike `DefaultHasher`.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    })
}

/// Stable, content-derived shortcut id — deliberately independent of
/// `action.shortcut` (the trigger key). Hotkeys are owned by KDE's Global
/// Shortcuts (see `HOTKEY_UI_INFO_JS` in main.rs), and `apply_shortcuts`
/// only binds an id the *first* time it's seen. If the id included the
/// trigger, rebinding in KDE and then having the renderer resend its
/// (still-original) config would mint a second, competing binding for the
/// same action. Keying on action type + target + focus_overlay avoids that.
/// `None` for `Unsupported` — those are never registered.
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
        // Text can be anything (incl. non-ASCII), so hash rather than slug.
        ActionKind::PasteInChat { text, send } => {
            Some(format!("paste-in-chat-{send}-{:016x}", fnv1a(text)))
        }
        ActionKind::StashSearch { text } => Some(format!("stash-search-{:016x}", fnv1a(text))),
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
    fn key_to_evdev_matches_input_event_codes() {
        let cases = [
            ("A", 30), ("C", 46), ("D", 32), ("Z", 44), ("1", 2), ("9", 10), ("0", 11),
            ("F1", 59), ("F10", 68), ("F11", 87), ("F12", 88), ("F13", 183), ("F24", 194),
            ("Ctrl", 29), ("Space", 57), ("Numpad0", 82),
        ];
        for (key, code) in cases {
            assert_eq!(key_to_evdev(key), Some(code), "{key}");
        }
        assert_eq!(key_to_evdev("F25"), None);
        assert_eq!(key_to_evdev("F"), Some(33)); // the letter, not a function key
        assert_eq!(key_to_evdev(""), None);
    }

    #[test]
    fn parses_supported_actions_and_skips_unsupported_without_erroring() {
        let payload = serde_json::json!({
            "shortcuts": [
                { "shortcut": "Ctrl + D", "action": { "type": "copy-item", "target": "price-check", "focusOverlay": false } },
                { "shortcut": "Shift + Space", "action": { "type": "toggle-overlay" } },
                { "shortcut": "Ctrl + Shift + S", "action": { "type": "stash-search", "text": "foo" } },
                { "shortcut": "Ctrl + T", "action": { "type": "trigger-event", "target": "delve-grid" } },
                { "shortcut": "F5", "action": { "type": "paste-in-chat", "text": "/hideout", "send": true } },
                { "shortcut": "Ctrl + O", "action": { "type": "ocr-text", "target": "heist-gems" } },
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
        assert_eq!(parsed.shortcuts.len(), 6);
        assert!(matches!(
            parsed.shortcuts[0].action,
            ActionKind::CopyItem { ref target, focus_overlay: false } if target == "price-check"
        ));
        assert!(matches!(parsed.shortcuts[1].action, ActionKind::ToggleOverlay));
        assert!(matches!(
            parsed.shortcuts[2].action,
            ActionKind::StashSearch { ref text } if text == "foo"
        ));
        assert!(matches!(
            parsed.shortcuts[3].action,
            ActionKind::TriggerEvent { ref target } if target == "delve-grid"
        ));
        assert!(matches!(
            parsed.shortcuts[4].action,
            ActionKind::PasteInChat { ref text, send: true } if text == "/hideout"
        ));
        assert!(matches!(parsed.shortcuts[5].action, ActionKind::Unsupported));
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
                keep_mod_keys: false,
                action: ActionKind::CopyItem {
                    target: "price-check".to_string(),
                    focus_overlay: false,
                },
            },
            ShortcutAction {
                shortcut: "Ctrl + D".to_string(),
                keep_mod_keys: false,
                action: ActionKind::CopyItem {
                    target: "price-check".to_string(),
                    focus_overlay: false,
                },
            },
            ShortcutAction {
                shortcut: "Ctrl + D".to_string(),
                keep_mod_keys: false,
                action: ActionKind::CopyItem {
                    target: "item-check".to_string(),
                    focus_overlay: true,
                },
            },
            ShortcutAction {
                shortcut: "Shift + Space".to_string(),
                keep_mod_keys: false,
                action: ActionKind::ToggleOverlay,
            },
            ShortcutAction {
                shortcut: "Shift + Space".to_string(),
                keep_mod_keys: false,
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
            keep_mod_keys: false,
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
            keep_mod_keys: false,
            action: ActionKind::CopyItem {
                target: "price-check".to_string(),
                focus_overlay: false,
            },
        };
        let different_target = ShortcutAction {
            shortcut: "Ctrl + D".to_string(),
            keep_mod_keys: false,
            action: ActionKind::CopyItem {
                target: "item-check".to_string(),
                focus_overlay: false,
            },
        };
        let different_focus_overlay = ShortcutAction {
            shortcut: "Ctrl + D".to_string(),
            keep_mod_keys: false,
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
                keep_mod_keys: false,
                action: ActionKind::Unsupported
            }),
            None
        );
    }
}
