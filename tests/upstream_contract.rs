//! Fails when renderer markup or text `src/` relies on disappears from the
//! vendored upstream, e.g. after a submodule bump renames an i18n key.
//! IPC events and payloads are checked by the compiler instead: see
//! src/ipc.rs.

use std::path::Path;

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|e| panic!("{}: {e} (submodule checked out?)", full.display()))
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
