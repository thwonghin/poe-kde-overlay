# WIP: PoE2 (Exiled Exchange 2) gaps

PoE2 runs through the EE2 renderer as a peer of PoE1 (`bb10c52`, CI in
`bf410c9`). Price-check works. These are what's still missing or assumed.

## Pending live check

- **`hideOverlayOnBlur` / `initialDelay`** (poe2). With hideOverlayOnBlur
  on, the overlay surface unmaps while PoE isn't focused (unless the
  overlay is interactive) or no widget wants showing
  (`Overlay::sync_window_visibility`). Check that it comes back with a
  working click-through region, and that a price-check from the hidden
  state isn't slow (WebKit may throttle an unmapped page). initialDelay
  sets the clipboard poll's first wait.

- **Advanced-descriptions key** (`src/game_config.rs`). poe2 copies now
  hold `show_advanced_item_descriptions` from poe2_production_Config.ini
  (gameConfig, else the game's Wine prefix, else EE2's guesses), re-read
  on every copy. Check the Log tab names the right ini, and that a
  rebound key still copies tiers/ranges.

- **`search-same-priced` hotkey.** EE2-only copy-item target, now in
  `extra_registerable_actions` (`src/host_config.rs`) with a drift test
  (`every_vendor_copy_item_target_is_registerable`). Check that "Copy item
  (search-same-priced)" shows up in System Settings → Shortcuts →
  poe-kde-overlay, bind it, and try it in PoE2.

## Gaps

1. **Item library.** EE2's `log-item` hotkey and `CLIENT->MAIN::write-data`
   (`FileWriter.ts`). `write-data` is ignored, so the library tab does
   nothing. `log-item` is listed as unsupported in the drift test. `re-parse-log`
   should also flush its client-log channel (`ponytail:` in `server.rs`).

## Known limits (by design)

- A hotkey pressed during the 1–2 s WebView reload after switching games
  is lost (`ponytail:` in `Overlay::switch_game`).
- The first EE2 bump PR will be flagged "review needed". EE2's `main/src`
  has files we don't reimplement (`FileWriter.ts`).
