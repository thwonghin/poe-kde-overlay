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
- **Follow-up commit:** CI now runs `cargo test` and clippy
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

Still open:

## 1. Possible race in `copy_item_text`

`remote_input.rs`: presses Ctrl+C, reads the clipboard (`previous`), then
clears it and polls. If PoE finishes copying *before* that first read, the
clear erases the fresh item text and the check fails with "clipboard poll
timed out". Works in testing, so PoE is evidently slower than that read.
Revisit if those timeouts show up intermittently.

## 2. 500ms KWin script timeout is a guess

`kwin_tracker.rs`'s `SCRIPT_TIMEOUT` bounds each disposable-script round
trip (cursor query, activate PoE). Picked, not measured. If `never replied`
shows up in the log during normal play, raise it — the auto-close poll just
skips that tick, but a price check fails outright.

## Known simplifications (`ponytail:` notes in the code)

Deliberate, each with a stated ceiling; listed so they're not forgotten.

- **English-only Settings detection** (`main.rs`, `HOTKEY_UI_INFO_JS`): the
  hotkey tabs are found by English label text; in other languages the
  original hotkey UI stays visible instead of the KDE banner.
- **Alt-hold hide only un-hides on modifier changes**
  (`overlay_visibility.rs`): upstream also un-hides on any key or mouse
  move while Alt is held; we only see modifier transitions.
- **No mousedown-in-area handler** (`shortcuts.rs`, `spawn_auto_close`): a
  click inside the popup is covered by the move that precedes it.
