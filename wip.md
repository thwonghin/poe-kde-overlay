# WIP: PoE2 (Exiled Exchange 2) gaps

PoE2 runs through the EE2 renderer as a peer of PoE1 (`bb10c52`, CI in
`bf410c9`). Price-check works. These are what's still missing or assumed.

## Pending live check

- **Game-log watcher** (`src/game_log.rs`). Works live in PoE2 (log found
  via the game's open fds in a custom Wine prefix). Still check PoE1.

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
2. **Advanced-descriptions key.** PoE2 copies use Ctrl+Alt+C, assuming
   PoE2's default Alt. EE2 reads the real key from
   `poe2_production_Config.ini` (`GameConfig.ts`), so a rebound key breaks
   advanced copies here. Marked `ponytail:` in `Game::copy_with_alt`
   (`src/game.rs`).
3. **Ignored EE2 settings.** `hideOverlayOnBlur` (with
   `OVERLAY->MAIN::render-state`) and `initialDelay`, the clipboard poll's
   first delay, are accepted but do nothing.

## Known limits (by design)

- A hotkey pressed during the 1–2 s WebView reload after switching games
  is lost (`ponytail:` in `Overlay::switch_game`).
- The first EE2 bump PR will be flagged "review needed". EE2's `main/src`
  has files we don't reimplement (`FileWriter.ts`).
