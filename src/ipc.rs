//! Both renderers' `ipc/types.ts` events (vendor/awakened-poe-trade for
//! poe1, vendor/exiled-exchange-2 for poe2) as Rust types: every
//! WebSocket message in either direction goes through these. `cargo test`
//! exports them with ts-rs and has `tsc` check them against upstream's own
//! `IpcEvent` (tests/ipc_contract_poe{1,2}.ts), so an upstream rename, type change,
//! new required field or new client event fails the build's tests.
//!
//! Structs only carry the fields we use; the check makes sure those exist
//! upstream with the same types.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::host_config::HostConfig;

/// CLIENT->MAIN / OVERLAY->MAIN: what the renderer sends us.
#[derive(Debug, Deserialize, TS)]
#[serde(tag = "name", content = "payload")]
pub enum ClientEvent {
    #[serde(rename = "CLIENT->MAIN::used-recently", rename_all = "camelCase")]
    UsedRecently { is_overlay: bool },
    #[serde(rename = "OVERLAY->MAIN::track-area")]
    TrackArea(TrackArea),
    #[serde(rename = "OVERLAY->MAIN::focus-game")]
    FocusGame,
    #[serde(rename = "CLIENT->MAIN::update-host-config")]
    UpdateHostConfig(HostConfig),
    #[serde(rename = "CLIENT->MAIN::user-action")]
    UserAction(UserAction),
    #[serde(rename = "CLIENT->MAIN::save-config", rename_all = "camelCase")]
    SaveConfig { contents: String, is_temporary: bool },
    // poe2 (EE2) only. render-state drives its opt-in hideOverlayOnBlur;
    // write-data (its item library) is ignored.
    #[serde(rename = "OVERLAY->MAIN::render-state", rename_all = "camelCase")]
    RenderState { should_show: bool },
    #[serde(rename = "CLIENT->MAIN::write-data")]
    WriteData(#[ts(type = "unknown")] serde::de::IgnoredAny),
    #[serde(rename = "CLIENT->MAIN::re-parse-log")]
    ReparseLog,
}

#[derive(Debug, Deserialize, TS)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum UserAction {
    /// AppTray.ts listens for the same event.
    Quit,
    /// The stash-search widget's click path (WidgetStashSearch.vue).
    StashSearch { text: String },
    // Updater actions: no updater in a locally built binary.
    CheckForUpdate,
    UpdateAndRestart,
    /// Runtime leniency only; hidden from the type check so a new upstream
    /// action still fails it.
    #[serde(other)]
    #[ts(skip)]
    Unknown,
}

/// `IpcTrackArea`'s payload, in the overlay WebView's CSS pixels (`area` is
/// relative to the overlay window; `from` is the position we sent with
/// item-text, i.e. global).
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TrackArea {
    pub hold_key: String,
    pub area: Rect,
}

#[derive(Debug, Clone, Deserialize, TS)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x > self.x && x < self.x + self.width && y > self.y && y < self.y + self.height
    }
}

/// MAIN->CLIENT / MAIN->OVERLAY: what we send the renderer.
#[derive(Debug, Serialize, TS)]
#[serde(tag = "name", content = "payload")]
pub enum MainEvent {
    #[serde(rename = "MAIN->CLIENT::log-entry")]
    LogEntry { message: String },
    #[serde(rename = "MAIN->CLIENT::config-changed")]
    ConfigChanged { contents: String },
    #[serde(rename = "MAIN->CLIENT::widget-action")]
    WidgetAction { target: String },
    #[serde(rename = "MAIN->CLIENT::item-text", rename_all = "camelCase")]
    ItemText {
        target: String,
        clipboard: String,
        position: Point,
        focus_overlay: bool,
    },
    #[serde(rename = "MAIN->OVERLAY::focus-change", rename_all = "camelCase")]
    FocusChange {
        game: bool,
        overlay: bool,
        using_hotkey: bool,
    },
    #[serde(rename = "MAIN->OVERLAY::hide-exclusive-widget")]
    HideExclusiveWidget,
    #[serde(rename = "MAIN->OVERLAY::overlay-attached")]
    OverlayAttached,
    #[serde(rename = "MAIN->CLIENT::game-log")]
    GameLog { lines: Vec<String> },
    #[serde(rename = "MAIN->OVERLAY::visibility", rename_all = "camelCase")]
    Visibility { is_visible: bool },
}

#[derive(Debug, Serialize, TS)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// GET /config's body (`HostState`).
#[derive(Debug, Serialize, TS)]
pub struct HostState {
    pub contents: Option<String>,
    pub version: String,
    pub updater: UpdateInfo,
}

#[derive(Debug, Serialize, TS)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum UpdateInfo {
    #[serde(rename_all = "camelCase")]
    UpdateNotAvailable {
        /// Epoch ms; `0` made the About page render "last checked" as 1970.
        #[ts(type = "number")]
        checked_at: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Exports the types next to tests/ipc_contract_poe{1,2}.ts and runs
    /// each renderer's own `tsc` on its contract.
    #[test]
    fn rust_ipc_types_match_upstream_typescript() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = root.join("tests/generated");
        let _ = std::fs::remove_dir_all(&out);
        let config = ts_rs::Config::new().with_out_dir(&out);
        ClientEvent::export_all(&config).unwrap();
        MainEvent::export_all(&config).unwrap();
        HostState::export_all(&config).unwrap();

        for (vendor, contract) in [
            ("awakened-poe-trade", "tests/ipc_contract_poe1.ts"),
            ("exiled-exchange-2", "tests/ipc_contract_poe2.ts"),
        ] {
            let tsc = root.join(format!("vendor/{vendor}/renderer/node_modules/.bin/tsc"));
            assert!(tsc.exists(), "{} missing: run ./scripts/build-renderer.sh", tsc.display());
            let result = std::process::Command::new(&tsc)
                .current_dir(root)
                .args(["--noEmit", "--strict", "--target", "es2022", "--module", "esnext"])
                .args(["--moduleResolution", "bundler", contract])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "tsc rejected the Rust IPC types ({contract}):\n{}",
                String::from_utf8_lossy(&result.stdout)
            );
        }
    }

    #[test]
    fn wire_format_matches_upstream_event_shape() {
        let event: ClientEvent = serde_json::from_str(
            r#"{"name":"CLIENT->MAIN::user-action","payload":{"action":"check-for-update"}}"#,
        )
        .unwrap();
        assert!(matches!(event, ClientEvent::UserAction(UserAction::CheckForUpdate)));
        // Runtime stays lenient about an action upstream adds later.
        let event: ClientEvent =
            serde_json::from_str(r#"{"name":"CLIENT->MAIN::user-action","payload":{"action":"new"}}"#).unwrap();
        assert!(matches!(event, ClientEvent::UserAction(UserAction::Unknown)));
        let event: ClientEvent =
            serde_json::from_str(r#"{"name":"OVERLAY->MAIN::render-state","payload":{"shouldShow":false}}"#).unwrap();
        assert!(matches!(event, ClientEvent::RenderState { should_show: false }));
        let event: ClientEvent = serde_json::from_str(r#"{"name":"OVERLAY->MAIN::focus-game"}"#).unwrap();
        assert!(matches!(event, ClientEvent::FocusGame));

        let sent = serde_json::to_value(MainEvent::Visibility { is_visible: true }).unwrap();
        assert_eq!(sent, serde_json::json!({ "name": "MAIN->OVERLAY::visibility", "payload": { "isVisible": true } }));
        let sent = serde_json::to_value(MainEvent::OverlayAttached).unwrap();
        assert_eq!(sent, serde_json::json!({ "name": "MAIN->OVERLAY::overlay-attached" }));
    }
}
