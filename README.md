# apt-kwin-overlay

A native KWin/Wayland backend for [Awakened PoE Trade](https://github.com/SnosMe/awakened-poe-trade) (APT).

APT's overlay (hotkeys, click-through, global input injection, clipboard
capture) is built entirely on Electron/X11 APIs and doesn't work under
Wayland. This project throws away APT's Electron `main` process and
replaces it with a small Rust/GTK4 host that does the same job using
native Wayland/KDE mechanisms — while running APT's **real, unmodified**
Vue renderer (vendored as a git submodule) on top of it, talking over the
same WebSocket/HTTP protocol it always has.

If you're on KDE Plasma (Wayland) and want APT's price-check overlay
without falling back to X11/XWayland, this is what that looks like.

## How it works

```
┌─────────────────────────────┐        ┌──────────────────────────────┐
│  vendor/awakened-poe-trade   │  HTTP/  │        apt-kwin-overlay       │
│  renderer (unmodified Vue UI) │◄──WS──►│  (this repo, Rust + GTK4)     │
└─────────────────────────────┘        └──────────────────────────────┘
                                          │
                                          ├─ gtk4-layer-shell   fullscreen click-through overlay surface
                                          ├─ WebKitGTK          renders the vendored renderer
                                          ├─ xdg-desktop-portal global shortcuts, remote input (EIS),
                                          │                     clipboard read/write
                                          ├─ KWin scripting     window tracking (find/follow PoE),
                                          │  (org.kde.KWin D-Bus)  cursor position queries
                                          └─ local HTTP/WS server  serves the renderer's dist/ and
                                                                   speaks its `MAIN<->OVERLAY`/`CLIENT`
                                                                   protocol in place of Electron IPC
```

Everything under `src/` is a from-scratch reimplementation of one narrow
slice of APT's `main/src/*` — just enough of the host contract for the real
renderer to think it's still running inside Electron:

| Module | Replaces (upstream `main/src/*`) | Does |
|---|---|---|
| `main.rs` | window-management bits of `OverlayWindow.ts` | Builds the layer-shell surface, toggles click-through/keyboard-focus mode, handles Escape/Ctrl+W locally while a widget is interactive, injected JS pointing the renderer's now-inert Hotkeys UI at KDE Settings |
| `server.rs` | `server.ts`, IPC bus | Local HTTP server serving the renderer's `dist/`, plus the WebSocket event bus (`MAIN->OVERLAY`, `OVERLAY->MAIN`, etc.) the renderer expects instead of Electron IPC |
| `shortcuts.rs` | `shortcuts/Shortcuts.ts` | Binds hotkeys dynamically (from the renderer's own shortcut config) via the `GlobalShortcuts` portal, dispatches price-check/toggle-overlay/copy-item actions, popup auto-close |
| `host_config.rs` | config parsing bits of `Shortcuts.ts`/`ConfigStore.ts` | Parses the renderer's `update-host-config` shortcut list and `restoreClipboard` flag; pre-registers all configurable actions with KDE so they're nameable/bindable in System Settings → Shortcuts |
| `remote_input.rs` | `HostClipboard.ts`, synthetic key injection | Simulates Ctrl+C, reads clipboard with retry/validation, and restores it afterward via the `RemoteDesktop`/`ConnectToEIS` and `Clipboard` portals (libei), since there's no other portal-sanctioned way to inject input or read/write the clipboard on Wayland |
| `kwin_tracker.rs` | window-tracking bits of `OverlayWindow.ts` | Loads a small persistent KWin script over D-Bus to track window activation/geometry (find PoE, follow it across monitors, reset overlay state when PoE regains focus) and do one-shot cursor-position queries |
| `proxy.rs` | `proxy.ts` | Proxies renderer requests to poe.ninja / pathofexile.com (same host allowlist, UA override) |
| `config_store.rs` | `ConfigStore.ts` | Persists the renderer's settings blob to disk, opaquely |
| `uploads.rs` | upload handling | Screenshot/image upload storage, served back over HTTP |
| `logger.rs` | `RemoteLogger.ts` | In-memory log buffer replayed to newly connected WS clients |

The renderer itself lives in `vendor/awakened-poe-trade` as an unmodified
git submodule — no patches, no fork. We only build its `renderer/` (the
Vue app), never its `main/` (the Electron process this project replaces).

## Status

This is a working spike, not a polished release. Known differences from real
APT: no Ctrl+scroll stash-tab navigation, no `logKeys` key logging, no
game-log watcher (the pinned renderer discards its lines anyway), and no
Windows-only OCR or self-updater.

## Requirements

- KDE Plasma on Wayland (relies on KWin's scripting D-Bus interface and
  `xdg-desktop-portal-kde` specifically — other compositors/portal
  backends are untested and likely won't work as-is)
- Rust (2024 edition toolchain)
- `webkit2gtk-6.0` / `gtk4-layer-shell` system libraries (dev packages for
  whichever your distro calls them)
- Node.js + npm, only for the one-time renderer build

## Building

```sh
git submodule update --init

# Build the vendored, unmodified renderer (only needed once, or after
# updating the submodule):
./scripts/build-renderer.sh

cargo build --release
```

### Installing on Arch/CachyOS via the included PKGBUILD

```sh
git clone https://github.com/thwonghin/apt-kwin-overlay.git
cd apt-kwin-overlay
makepkg -si
```

Installs the `apt-kwin-overlay` binary to `/usr/bin` and its renderer
assets to `/usr/share/apt-kwin-overlay/dist`.

## Running

```sh
cargo run --release
```

On first run you'll be prompted by KDE's portal to grant:
global shortcuts, remote desktop (input injection), screencast (part of
the RemoteDesktop session), and clipboard access. These are required for
hotkeys and price-check to work at all.

The overlay is a transparent, click-through, always-on-top layer-shell
surface. Toggle it and trigger a price-check with the same default hotkeys
as stock APT (Shift+Space to toggle, Ctrl+D to price-check, Ctrl+Alt+D for a
locked price-check). While a popup is open, Escape or Ctrl+W closes it too.

To change hotkeys, use KDE's own **System Settings → Shortcuts** (search for
"apt-kwin-overlay") rather than APT's in-overlay Hotkeys/Item Info Settings
tabs — those are replaced with a banner pointing here, since KDE's
`GlobalShortcuts` portal has no way for this app to silently rebind an
already-granted key the way Electron's global-shortcut API did (see
`host_config.rs`'s doc comments for the empirical detail).

## License

MIT, see [LICENSE](LICENSE). The vendored `awakened-poe-trade` submodule
keeps its own MIT license.
