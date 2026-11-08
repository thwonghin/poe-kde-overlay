# apt-kwin-overlay: code-quality follow-ups

From a full read of `src/` plus CI/packaging (2026-11-07). Fixed so far:

- **`0185438`:** path traversal in static files and `/uploads`; the proxy
  turning upstream 4xx/5xx into a 502 (and a new agent per request);
  unbounded and leaking KWin script calls; the global hotkey debounce; the
  uncapped request body; KWin scripts in the shared `/tmp`; cross-origin
  WebSocket connections; the six hand-plumbed handles (now one
  `Rc<Overlay>`); four server→GTK channels (now two); duplicated
  timeout/log/key-table code; `RefCell` borrows held across awaits; the
  unbounded log history; misplaced doc comments; dead `WindowEvent.id`; the
  env-mutating test.
- **`a90cabf`:** live Log tab entries running together (no newline);
  per-modifier EIS console spam; history-style comments.
- **`40dce72`:** CI now runs `cargo test` and clippy
  (`-D warnings`); key release before Ctrl+C uses the trigger KDE actually
  has bound (bind response + `ShortcutsChanged`, falling back to the
  renderer's shortcut when the description can't be parsed); the tracker
  script hooks `outputChanged` instead of every window's
  `frameGeometryChanged`, and the overlay follows PoE to another monitor
  without waiting for re-activation; one Rust definition of "is PoE" that
  the activate script's JS is generated from; `restoreClipboard` no longer
  depends on the GlobalShortcuts session; "overlay ready" re-sent when PoE
  restarts (new pid); 10s read timeout on HTTP connections and a 30s
  timeout on proxied requests; the proxy passes any method through; per-event
  logging behind `APT_KWIN_OVERLAY_DEBUG`.
- **`b4c0d38`:** WebSocket client threads sleep on a `polling` poller
  (socket readiness + a notify from senders) instead of a 15ms read
  timeout — no idle wakeups (62/s → ~0 measured), and queued messages go
  out immediately.

- **`841d33b`:** the price-check clipboard poll wakes when PoE takes the
  clipboard instead of always sleeping 48ms first (49-50ms → 3-10ms
  Ctrl+C to item text, measured live); popup tracker decision, held
  modifier and debounce pulled out as pure, unit-tested functions.
- **`7f4a6bb`:**
  - Clipboard race: if PoE copied before our clear, the clear erased its
    item text and the check timed out. A clipboard change that brings no
    item text now triggers one more Ctrl+C. The timeout message now says
    whether the clipboard changed at all.
  - Settings banner works in every UI language: the injected script loads
    the active language's `app_i18n.json` and matches on English or that
    translation (checked against mock tabs in en/ru/ko/cmn-Hant).
  - (docs) Dropped "500ms KWin script timeout is a guess": cursor queries measure
    1.0ms median, 1.8ms max (50 queries against a live KWin), so 500ms is
    over 250× headroom.
- **`9b13a5d`:** Tests and CI:
  - `tests/upstream_contract.rs` fails if an IPC event name `src/` uses
    (scanned from string literals), a payload field, a Settings-banner
    i18n key or its English text, or the PriceCheckWindow.vue track-area
    handshake disappears from the vendored upstream.
  - The injected Settings script lives in `src/hotkey_ui_info.js`
    (`include_str!`); CI runs `node --check` on it.
  - Unit tests: last-active routing and re-trust on close (`server.rs`),
    exact-host proxy allowlist and stripped headers, and the sticky
    `.tmp` config switch (`ConfigStore` now takes its path).

Nothing open beyond the deliberate simplifications below.

## Known simplifications (`ponytail:` notes in the code)

Deliberate, each with a stated ceiling; listed so they're not forgotten.

- **Alt-hold hide only un-hides on modifier changes**
  (`overlay_visibility.rs`): upstream also un-hides on any key or mouse
  move while Alt is held; we only see modifier transitions.
- **No mousedown-in-area handler** (`shortcuts.rs`, `spawn_auto_close`): a
  click inside the popup is covered by the move that precedes it.
