//! Fails when a name `src/` relies on disappears from the vendored
//! upstream, e.g. after a submodule bump renames an IPC event or an i18n
//! key. Complements bump-submodule.yml's "Detect vendor contract-surface
//! changes" step, which only flags `main/src`/`ipc` diffs for review and
//! doesn't look at the renderer at all.

use std::path::Path;

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|e| panic!("{}: {e} (submodule checked out?)", full.display()))
}

/// Every `"X->Y::name"` string literal in non-test code under `src/`.
/// Scanned rather than listed so new events are covered without anyone
/// remembering to add them here.
fn event_names_used_by_src() -> Vec<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        // Test modules broadcast made-up names (MAIN->CLIENT::test etc.).
        // Escaped quotes dropped so they don't flip in/out-of-string parity.
        let code = text.split("#[cfg(test)]").next().unwrap().replace("\\\"", "");
        for literal in code.split('"').skip(1).step_by(2) {
            let Some((route, name)) = literal.split_once("::") else { continue };
            let Some((from, to)) = route.split_once("->") else { continue };
            let upper = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_uppercase());
            let kebab = !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-');
            if upper(from) && upper(to) && kebab {
                names.push(literal.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[test]
fn ipc_event_names_exist_upstream() {
    let types = read("vendor/awakened-poe-trade/ipc/types.ts");
    let names = event_names_used_by_src();
    // Sanity check on the scan itself: src/ uses well over this many.
    assert!(names.len() >= 10, "scan found only {names:?}");
    let missing: Vec<_> = names.iter().filter(|n| !types.contains(&format!("'{n}'"))).collect();
    assert!(missing.is_empty(), "not in ipc/types.ts: {missing:?}");
}

#[test]
fn ipc_payload_fields_and_action_types_exist_upstream() {
    let types = read("vendor/awakened-poe-trade/ipc/types.ts");
    // ponytail: hand-listed — the serde renames and json! keys in src/ that
    // cross the wire. Add one here when adding a payload field.
    let fields = [
        "shortcuts", "restoreClipboard", "windowTitle", "keepModKeys", "focusOverlay",
        "holdKey", "area", "isOverlay", "contents", "isTemporary", "message", "target",
        "clipboard", "position", "isVisible", "game", "overlay", "usingHotkey", "text", "send",
    ];
    let missing: Vec<_> = fields.iter().filter(|f| !types.contains(&format!(" {f}"))).collect();
    assert!(missing.is_empty(), "payload fields not in ipc/types.ts: {missing:?}");

    let literals = [
        "copy-item", "toggle-overlay", "trigger-event", "paste-in-chat", "stash-search",
        "quit", "update-not-available",
    ];
    let missing: Vec<_> = literals.iter().filter(|l| !types.contains(&format!("'{l}'"))).collect();
    assert!(missing.is_empty(), "literals not in ipc/types.ts: {missing:?}");
}

/// hotkey_ui_info.js finds the Settings tabs by these labels: the key path
/// it fetches translations by, and the English text it matches before any
/// translation has loaded.
#[test]
fn settings_banner_markers_exist_upstream() {
    let js = read("src/hotkey_ui_info.js");
    let en: serde_json::Value =
        serde_json::from_str(&read("vendor/awakened-poe-trade/renderer/public/data/en/app_i18n.json"))
            .unwrap();
    for (section, key) in [
        ("settings", "clear_hotkey"),
        ("item", "open_on_wiki"),
        ("price_check", "auto_search"),
    ] {
        assert!(js.contains(&format!("['{section}', '{key}']")), "{section}.{key} not used by the JS");
        let text = en[section][key]
            .as_str()
            .unwrap_or_else(|| panic!("{section}.{key} missing from en/app_i18n.json"));
        assert!(js.contains(&format!("'{text}'")), "JS's English fallback for {section}.{key} isn't {text:?}");
    }
}

/// shortcuts.rs's popup tracker relies on the price-check window answering
/// item-text for target "price-check" with a track-area carrying holdKey
/// and area.
#[test]
fn price_check_window_still_sends_track_area() {
    let vue = read("vendor/awakened-poe-trade/renderer/src/web/price-check/PriceCheckWindow.vue");
    for needle in [
        "'MAIN->CLIENT::item-text'",
        "e.target !== 'price-check'",
        "'OVERLAY->MAIN::track-area'",
        "holdKey:",
        "area: {",
    ] {
        assert!(vue.contains(needle), "PriceCheckWindow.vue no longer contains {needle:?}");
    }
}
