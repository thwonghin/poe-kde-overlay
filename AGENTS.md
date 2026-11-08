# AGENTS.md

Rust/GTK4 replacement for Awakened PoE Trade's Electron `main/` process on
KDE Plasma (Wayland). The UI is the real, unmodified upstream renderer from
the `vendor/awakened-poe-trade` submodule; `src/*.rs` reimplements
`vendor/awakened-poe-trade/main/src/*` (IPC types: `ipc/types.ts`).

## Start here

- **`WIP.md`** is the live tracker for open parity gaps vs. upstream and
  productionalization status. Read it before picking up work; when an item
  closes, move it into its intro's closed list in the same change.
- Non-obvious decisions live as code comments at the call sites (e.g. why
  hotkeys are owned by KDE Global Shortcuts, why price-check presses Ctrl+C
  before the clipboard snapshot, why WebSocket sockets are thread-owned).
  Read them before changing that code.

## Build / run / test

```sh
./scripts/build-renderer.sh   # once, or after bumping the submodule
cargo run --release           # dev run; dist path comes from .cargo/config.toml
cargo test
```

Live behavior (portal, KWin scripting, EIS input) can only be verified on a
real KDE session with PoE running — say so when a change is untested live.

## Conventions

- Never modify the vendored submodule; work around it from `src/`.
- When porting upstream behavior, read the vendor TS files directly rather
  than relying on summaries — validation/edge-case logic is easy to miss.
- The GTK main thread runs the hotkey loop: never block it (no shared
  locks held across blocking I/O, bound every portal/D-Bus await).
