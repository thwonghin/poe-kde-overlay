# apt-kwin-overlay: remaining gaps vs. real Awakened Poe Trade

Audit of `main/src/*` in the vendored, unmodified `awakened-poe-trade`
submodule against what `apt-kwin-overlay`'s Rust backend actually
implements. The renderer (Vue UI) is 100% the real, unmodified app —
everything below is about the `main/` process we replaced.

Items that were open gaps here and have since been closed — config-driven
hotkeys (via KDE's own Global Shortcuts), clipboard read reliability +
restore-on-copy, local Escape/Ctrl+W handling, in-app logger wiring,
resetting overlay state when PoE regains OS focus, and a tray icon (via
`ksni`, with the `CLIENT->MAIN::user-action` quit wiring and the
`isOverlay`/`wasUsedRecently` `EventBus` fix that shipped alongside it),
`trigger-event` hotkeys (stopwatch/delve grid), the `overlay-attached`
"overlay ready" toast, honoring the `windowTitle` setting, and gating
toggle-overlay activation on PoE focus, releasing the hotkey's own
keys before injecting Ctrl+C, Alt-hold-to-hide (`overlay_visibility.rs`),
and the price-check popup tracker's hold-to-pin / hover-to-interact plus
`keepModKeys` (`spawn_auto_close` in `shortcuts.rs`) —
have been removed from this list to keep it focused on what's actually
still open. See git history for how each landed; the reasoning behind
non-obvious decisions (e.g. why hotkeys are now configured in KDE System
Settings instead of APT's own UI) is preserved as code comments at the
relevant call sites (`host_config.rs`, `main.rs`, `tray.rs`), not
duplicated here.

Ordered roughly by how much it'd actually change day-to-day use, not by file.

## 1. Stash scroll-wheel navigation

Real: Ctrl+scroll while hovering the stash tab bar (not over the sidebar) taps
Left/Right arrow to change tabs (`Shortcuts.ts`'s `uIOhook.on('wheel', ...)` +
`isStashArea`). Needs real PoE window bounds (`GameWindow.bounds`,
`uiSidebarWidth`) to know where "the stash tab bar" actually is on screen.

We have no scroll-event capture at all (libei/EIS only ever *emits* input for
us, doesn't capture the user's real mouse wheel), and no window-bounds-based
region logic. Not attempted. The `stashScroll` config field isn't parsed
either — relevant if this is ever picked up.

## 2. Hotkey actions not yet bound: paste-in-chat, stash-search

Parsed as `ActionKind::Unsupported` (inert) in `host_config.rs`. Closer than
it looks: `main/src/shortcuts/text-box.ts`'s `typeInChat`/`stashSearch` are
just `uiohook`-style key-tap sequences (`V`/`Ctrl+V`, `Enter`, `Home`,
`Delete`, `Ctrl+A`, `Ctrl+F`, `ArrowUp`, `Escape`) wrapped in a
clipboard-save/restore. `remote_input.rs::press_keys` is already a generic
`&[(keycode, KeyState)]` injector (`press_ctrl_c` just calls it with two
keys), and the clipboard-write wrapper these sequences need
(`RemoteInput::write_clipboard_text`, via `ashpd`'s `Clipboard` portal's
`set_selection`/`selection_write`/`selection_write_done`) already exists,
built for the clipboard-restore work, and `remote_input.rs::key_to_evdev`
already maps every renderer key name to an evdev keycode. Remaining work is
just a generic multi-key-tap helper — no new portal capability needed. The same `stashSearch` helper also backs
`CLIENT->MAIN::user-action {action:"stash-search"}` (the stash-search
widget's click path, `WidgetStashSearch.vue`), which `server.rs` currently
ignores — wire both when this lands.

## 4. No `logKeys` debug logging

`Shortcuts.ts` has a `logKeys` config flag (a renderer Settings toggle) that,
when on, logs every raw keydown/keyup and which action type fired to the
Log tab — a live debugging aid for hotkey issues. `host_config.rs` doesn't
parse `logKeys` at all, and we'd have only modifier transitions to log (the
EIS `KeyboardModifiers` feed `overlay_visibility.rs` uses), not real
keydown/keyup events.

## 5. No game-log-driven features (`GameLogWatcher.ts`)

Whisper/trade/zone-change notifications from tailing `Client.txt`. Genuinely
separate, sizeable feature — would need a file watcher on the log path
(`cfg.clientLog` from config) and its own event stream into
`MAIN->CLIENT::game-log`.

## 6. Not applicable / correctly out of scope, no action needed

- **`AppUpdater.ts`** (real electron-updater auto-update flow) — we already
  stub this correctly (always `update-not-available`). Self-updating doesn't
  make sense for a locally-built dev binary; leave as-is permanently.
- **`vision/*`** (OCR for heist gems, incl. the `ocr-text` copy-item hotkey
  action) — Windows-only upstream already (uses a Windows-specific
  screenshot path), correctly out of scope regardless of our platform.
- **`HostClipboard.ts`'s KDE/Proton-specific empty-clipboard workarounds** —
  likely not directly applicable since our capture path is portal-based, not
  Electron's clipboard API. Revisit only if stale/empty clipboard reads
  actually show up in testing (haven't so far).
- **`GameConfig.ts`** (reads PoE's `production_Config.ini` for the player's
  "advanced item description" keybind) — as of PoE 3.29, `Shortcuts.ts`
  hardcodes `showModsKey = 'Ctrl'` and ignores whatever `GameConfig` found
  anyway. Close to dead code in the real app too now; not worth porting.
## 7. Small differences: proxy cookies, single-instance lock

Neither looks urgent; noting them so they don't get re-discovered from
scratch later.

- **Proxy doesn't forward `Set-Cookie` / has no outbound cookie jar.** Real
  `proxy.ts` uses Electron's `net.request` with `useSessionCookies: true` —
  the app's persistent session cookie jar is attached to outbound requests
  to `pathofexile.com`/etc. and updated from responses automatically,
  and `proxy.ts` additionally strips the `Partitioned` attribute from
  `Set-Cookie` response headers so Chromium's cookie store will actually
  accept cross-context Cloudflare cookies. Our `proxy.rs` forwards only
  `content-type` and `x-rate-limit-*` response headers
  (`write_response_with_headers` call in `proxy::handle`) — `Set-Cookie` is
  silently dropped, and there's no cookie jar on the `ureq::Agent` side to
  attach anything back on the next request either way. Checked the renderer
  for any cookie/credential dependency (`grep -rn "POESESSID\|document.cookie\|withCredentials"
  renderer/src`) and found none, so this likely doesn't affect price-check
  or trade-search today — but it's a genuine behavioral difference from
  real APT's proxy, worth remembering if GGG-side requests ever start
  behaving inconsistently (e.g. increasingly aggressive Cloudflare
  challenges from what looks like a cookie-less client on every request).
- **No single-instance lock.** Real `main.ts` calls
  `app.requestSingleInstanceLock()` and exits immediately if another
  instance already holds it. `main.rs` has no equivalent — running the
  binary twice spawns two overlay windows, two local HTTP/WS servers, and
  two competing `ashpd` `GlobalShortcuts`/`RemoteDesktop` sessions with no
  guard against it. Minor (accidental double-launch, not a real usage
  path), but a one-time `flock`-style lock file (e.g. under
  `xdg::data_dir()`) would close it cheaply if it ever bites someone.

## Suggested order if picked back up

1. §2 (paste-in-chat/stash-search hotkey actions) — needs a generic
   multi-tap helper (`key_to_evdev` covers the keycodes); the clipboard-write wrapper it
   also needs already exists (`RemoteInput::write_clipboard_text`) and can
   be reused as-is.
2. §4 (`logKeys`) — low value with only modifier transitions to log.
3. §5 (game log watcher) — biggest standalone feature, independent of
   everything else; tackle whenever, in its own pass.
4. §7 — proxy cookies and the single-instance lock have no confirmed
   symptom; pick up opportunistically.

(§1, stash scroll-wheel navigation, is deliberately left off this list —
needs real window-bounds tracking plus scroll-event capture we don't have
today, a bigger lift than anything above for a lower-traffic feature.)

## Productionalization: non-feature gaps

Separate axis from everything above — not feature parity with upstream
APT, just packaging/robustness of this repo's own Rust backend.

**Done**: PKGBUILD (`makepkg -si` local install on Arch/CachyOS — not
published to AUR, registration's closed to new accounts); GitHub Actions
`ci.yml` (build check every push/PR), `bump-submodule.yml` (daily poll +
validated PR for upstream awakened-poe-trade commits), `release.yml`
(tag push → build → GitHub Release with a tarball); `server.rs`'s renderer
asset path now resolves at runtime (`APT_KWIN_OVERLAY_DIST` env var →
`/usr/share/apt-kwin-overlay/dist` → dev fallback via `.cargo/config.toml`)
instead of baking in a build-time path that broke once packaged; crash
resilience (`server::spawn()` failure in `main.rs` now shows a
`gtk4::AlertDialog` and quits cleanly instead of panicking); XDG basedir
compliance (`xdg::data_dir()` respects `$XDG_DATA_HOME`, used by
`config_store.rs`, `uploads.rs`, `remote_input.rs`); branding
(`APP_ID` is `io.github.thwonghin.AptKwinOverlay`, window title is
"apt-kwin-overlay"); `.desktop` file (`data/io.github.thwonghin.AptKwinOverlay.desktop`,
installed to `/usr/share/applications` by `PKGBUILD`'s `package()`, reuses
the real APT icon already vendored at `renderer/dist/icon.png`) — needed
for `ashpd::register_host_app` to succeed on KDE, which looks up an
installed app matching `APP_ID` via the portal's Registry interface.

**Remaining**: nothing structural. Two small, low-priority items — see §7
above (no single-instance lock; proxy doesn't forward `Set-Cookie`/has no
outbound cookie jar). Neither has a confirmed real-world symptom.
