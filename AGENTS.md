# AGENTS.md

Rust/GTK4 replacement for the Electron `main/` process of two PoE
price-check overlays on KDE Plasma (Wayland), served as equal peers:
**poe1** = Awakened PoE Trade (`vendor/awakened-poe-trade`), **poe2** =
Exiled Exchange 2 (`vendor/exiled-exchange-2`, its fork). The UI is the
real, unmodified upstream renderer of whichever game is focused
(`Overlay::switch_game`); `src/*.rs` reimplements both vendors'
`main/src/*` (IPC types: `ipc/types.ts`). In our code, say poe1/poe2 (see
`src/game.rs`), never apt/ee2, and never make one game a default or
fallback for the other.

## Start here

- Deliberate shortcuts are marked `ponytail:` in the code, each with its
  ceiling; `grep -rn 'ponytail:' src` lists them.
- Non-obvious decisions live as code comments at the call sites (e.g. why
  hotkeys are owned by KDE Global Shortcuts, why price-check presses Ctrl+C
  before the clipboard snapshot, why WebSocket sockets are thread-owned).
  Read them before changing that code.

## Build / run / test

```sh
./scripts/build-renderer.sh   # once, or after bumping either submodule
cargo run --release           # dev run; dist path comes from .cargo/config.toml
cargo test
```

Live behavior (portal, KWin scripting, EIS input) can only be verified on a
real KDE session with PoE running — say so when a change is untested live.

## Conventions

- Never modify either vendored submodule; work around them from `src/`.
- Lists mirrored from both vendors' `main/` (item-text signatures, proxy
  hosts) have drift tests that read the vendor sources; update the list,
  not the test, when one fails after a bump.
- When porting upstream behavior, read the vendor TS files directly rather
  than relying on summaries — validation/edge-case logic is easy to miss.
- The GTK main thread runs the hotkey loop: never block it (no shared
  locks held across blocking I/O, bound every portal/D-Bus await).
