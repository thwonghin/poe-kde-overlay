use std::cell::{Cell, RefCell};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::Arc;

use ashpd::desktop::clipboard::{Clipboard, RequestClipboardOptions, SetSelectionOptions};
use ashpd::desktop::remote_desktop::{ConnectToEISOptions, DeviceType, RemoteDesktop};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use ashpd::enumflags2::BitFlags as AshpdBitFlags;
use futures_util::{AsyncReadExt, StreamExt};
use reis::enumflags2::BitFlags as EiBitFlags;
use reis::event::{DeviceCapability, EiEvent};
use reis::{ei, event};

use crate::logger::Logger;

// Raw Linux evdev keycodes (linux/input-event-codes.h). notify_keyboard_keycode
// (the plain RemoteDesktop D-Bus path) turned out to have zero real-world
// effect on this KDE version despite the session being genuinely granted —
// confirmed via xdg-desktop-portal-kde debug logs showing the calls arriving
// correctly. NotifyKeyboardKeycode goes through KDE's own `fake_input`
// Wayland protocol; ConnectToEIS instead hands us a raw libei connection that
// KWin's actual remote-input backend is built around, which is the path KDE
// Connect itself uses. Keycodes stay evdev numbering either way.
const KEY_LEFTCTRL: u32 = 29;
const KEY_C: u32 = 46;

/// Renderer key name (`ipc/KeyToCode.ts`'s vocabulary, as used in a
/// `shortcut` string like "Ctrl + D") → evdev keycode.
fn key_to_evdev(key: &str) -> Option<u32> {
    const LETTERS: &[u32] = &[
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47,
        17, 45, 21, 44,
    ];
    let code = match key {
        "Ctrl" => 29,
        "Shift" => 42,
        "Alt" => 56,
        "Backspace" => 14,
        "Tab" => 15,
        "Enter" => 28,
        "CapsLock" => 58,
        "Escape" => 1,
        "Space" => 57,
        "PageUp" => 104,
        "PageDown" => 109,
        "End" => 107,
        "Home" => 102,
        "ArrowLeft" => 105,
        "ArrowUp" => 103,
        "ArrowRight" => 106,
        "ArrowDown" => 108,
        "Insert" => 110,
        "Delete" => 111,
        "0" => 11,
        "Numpad0" => 82,
        "Numpad1" => 79,
        "Numpad2" => 80,
        "Numpad3" => 81,
        "Numpad4" => 75,
        "Numpad5" => 76,
        "Numpad6" => 77,
        "Numpad7" => 71,
        "Numpad8" => 72,
        "Numpad9" => 73,
        "NumpadMultiply" => 55,
        "NumpadAdd" => 78,
        "NumpadSubtract" => 74,
        "NumpadDecimal" => 83,
        "NumpadDivide" => 98,
        "F11" => 87,
        "F12" => 88,
        "Semicolon" => 39,
        "Equal" => 13,
        "Comma" => 51,
        "Minus" => 12,
        "Period" => 52,
        "Slash" => 53,
        "Backquote" => 41,
        "BracketLeft" => 26,
        "Backslash" => 43,
        "BracketRight" => 27,
        "Quote" => 40,
        _ => {
            let mut chars = key.chars();
            return match (chars.next(), chars.next()) {
                (Some(c @ 'A'..='Z'), None) => Some(LETTERS[(c as u8 - b'A') as usize]),
                // KEY_1..KEY_9 = 2..10
                (Some(c @ '1'..='9'), None) => Some((c as u8 - b'1') as u32 + 2),
                // F1..F10 = 59..68, F13..F24 = 183..194
                (Some('F'), Some(_)) => match key[1..].parse::<u32>().ok()? {
                    n @ 1..=10 => Some(58 + n),
                    n @ 13..=24 => Some(170 + n),
                    _ => None,
                },
                _ => None,
            };
        }
    };
    Some(code)
}

// Ported from HostClipboard.ts's POLL_DELAY/POLL_LIMIT: after pressing
// Ctrl+C, poll the clipboard every 48ms rather than assuming any single
// fixed wait is always long enough for the game to update it, and give up
// after roughly 500ms of polling with no item text ever showing up.
const CLIPBOARD_POLL_DELAY_MS: u64 = 48;
const CLIPBOARD_POLL_LIMIT_MS: u64 = 500;

// See read_clipboard_once_bounded's doc comment: bounds a single portal
// SelectionRead call, which has been observed to hang forever (a real
// xdg-desktop-portal-kde bug, not a timing issue on our side).
const CLIPBOARD_READ_TIMEOUT_MS: u64 = 200;

const CLIPBOARD_TEXT_MIME: &str = "text/plain;charset=utf-8";

// HostClipboard.ts's LANGUAGE_DETECTOR: the first line of a real PoE item
// clipboard dump always starts with one of these (localized "Item Class: "),
// so a prefix match is enough to tell a genuine item paste apart from
// whatever else happened to be on the clipboard.
const ITEM_TEXT_SIGNATURES: &[&str] = &[
    "Item Class: ",        // en
    "Класс предмета: ",    // ru
    "Classe d'objet: ",    // fr
    "Gegenstandsklasse: ", // de
    "Classe do Item: ",    // pt
    "Clase de objeto: ",   // es
    "ชนิดไอเทม: ",           // th
    "아이템 종류: ",           // ko
    "物品種類: ",             // cmn-Hant
    "物品类别: ",             // cmn-Hans
];

fn looks_like_item_text(text: &str) -> bool {
    ITEM_TEXT_SIGNATURES.iter().any(|sig| text.starts_with(sig))
}

fn restore_token_path() -> std::path::PathBuf {
    crate::xdg::data_dir().join("apt-kwin-overlay/remote_desktop_restore_token")
}

struct KeyboardHandle {
    device: ei::Device,
    keyboard: ei::Keyboard,
}

pub struct RemoteInput {
    context: ei::Context,
    connection: event::Connection,
    keyboard: Rc<RefCell<Option<KeyboardHandle>>>,
    // Reference point for frame() timestamps (microseconds, monotonic) — real
    // values, not a hardcoded 0, since the EIS server likely uses them for
    // event ordering and may misorder/drop identically-timestamped events.
    start: std::time::Instant,
    // Portal-based clipboard access (org.freedesktop.portal.Clipboard), tied
    // to this RemoteDesktop session. Reading via GTK4's own gdk::Clipboard
    // turned out to only work reliably while our own window had keyboard
    // focus — Wayland gates clipboard visibility by focus for privacy, and
    // our overlay is meant to never hold focus while the game does. This
    // portal exists specifically so a RemoteDesktop-style session can read
    // the clipboard regardless of local focus.
    clipboard: Clipboard,
    // Kept alive, not otherwise used post-setup: dropping the D-Bus portal
    // session could tear down the EIS grant it authorized, even though the
    // raw EIS socket (`context`) is a separate connection.
    #[allow(dead_code)]
    session: Session<RemoteDesktop>,
    // Content we've most recently claimed clipboard ownership of via
    // SetSelection (see write_clipboard_text) and must hand back whenever
    // the compositor asks for it via a SelectionTransfer request (handled
    // by the listener task spawned in `setup`). `None` until restoreClipboard
    // is used for the first time.
    pending_clipboard_write: Rc<RefCell<Option<String>>>,
    // Tracks whether the *current* clipboard selection owner offers
    // CLIPBOARD_TEXT_MIME, kept in sync by a listener task (spawned in
    // `setup`) on the portal's own SelectionOwnerChanged signal. Confirmed
    // live: our own WebKitGTK webview intermittently claims clipboard
    // ownership itself (offering only its internal
    // "org.webkitgtk.WebKit.custom-pasteboard-data" format, e.g. while a
    // price-check popup is interactive) — attempting a SelectionRead for a
    // mime type the current owner doesn't offer is what triggers the
    // xdg-desktop-portal-kde bug read_clipboard_once_bounded's doc comment
    // describes. Checking this first lets copy_item_text skip the read
    // entirely while we already know it would hit that bug, rather than
    // relying solely on the per-read timeout as a safety net. Starts `true`
    // (optimistic) since there's no way to query current state up front,
    // only be notified of changes.
    clipboard_offers_text: Rc<Cell<bool>>,
    logger: Arc<Logger>,
}

impl RemoteInput {
    pub async fn setup(logger: Arc<Logger>) -> ashpd::Result<Self> {
        let proxy = RemoteDesktop::new().await?;
        let session = proxy.create_session(CreateSessionOptions::default()).await?;

        let saved_token = std::fs::read_to_string(restore_token_path()).ok();

        proxy
            .select_devices(
                &session,
                ashpd::desktop::remote_desktop::SelectDevicesOptions::default()
                    .set_devices(AshpdBitFlags::from(DeviceType::Keyboard))
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(saved_token.as_deref()),
            )
            .await?
            .response()?;

        // Must be requested before the session starts (portal spec).
        let clipboard = Clipboard::new().await?;
        clipboard
            .request(&session, RequestClipboardOptions::default())
            .await?;

        let response = proxy
            .start(&session, None, Default::default())
            .await?
            .response()?;

        if let Some(token) = response.restore_token() {
            let path = restore_token_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, token);
        }

        let fd = proxy
            .connect_to_eis(&session, ConnectToEISOptions::default())
            .await?;
        let stream = UnixStream::from(fd);
        let context =
            ei::Context::new(stream).map_err(|err| ashpd::Error::IO(std::io::Error::other(err)))?;

        let (connection, mut events) = context
            .handshake_async_io("apt-kwin-overlay", ei::handshake::ContextType::Sender)
            .await
            .map_err(|err| ashpd::Error::IO(std::io::Error::other(err.to_string())))?;

        let keyboard = Rc::new(RefCell::new(None));
        {
            let keyboard = keyboard.clone();
            let event_loop_context = context.clone();
            gtk4::glib::spawn_future_local(async move {
                while let Some(result) = events.next().await {
                    let event = match result {
                        Ok(event) => event,
                        Err(err) => {
                            eprintln!("[remote_input] eis event stream error: {err}");
                            continue;
                        }
                    };
                    println!("[remote_input] eis event: {event:?}");
                    match event {
                        EiEvent::SeatAdded(added) => {
                            added
                                .seat
                                .bind_capabilities(EiBitFlags::from(DeviceCapability::Keyboard));
                            let _ = event_loop_context.flush();
                        }
                        EiEvent::DeviceAdded(added) => {
                            if let Some(kbd) = added.device.interface::<ei::Keyboard>() {
                                println!("[remote_input] keyboard device bound");
                                keyboard.replace(Some(KeyboardHandle {
                                    device: added.device.device().clone(),
                                    keyboard: kbd,
                                }));
                            } else {
                                println!("[remote_input] device added without keyboard interface");
                            }
                        }
                        EiEvent::DeviceResumed(resumed) => {
                            resumed.device.device().start_emulating(0, 0);
                            let _ = event_loop_context.flush();
                        }
                        _ => {}
                    }
                }
                println!("[remote_input] eis event stream ended");
            });
        }

        // Answers clipboard-read requests from the rest of the desktop while
        // we hold selection ownership (see write_clipboard_text) — the
        // portal doesn't let us just "set the clipboard"; instead we claim
        // ownership via SetSelection and then must serve SelectionTransfer
        // requests reactively for as long as we own it, mirroring the EIS
        // event loop above in shape (a persistent listener task), though it's
        // a separate D-Bus signal stream, not an EIS one. Uses its own
        // Clipboard handle (Session isn't Clone, but `self.clipboard` is
        // moved into `Self` below anyway, and each stream item already
        // carries its own reconstructed Session, so no session needs to be
        // shared with it).
        let pending_clipboard_write: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        {
            let pending_clipboard_write = pending_clipboard_write.clone();
            let logger = logger.clone();
            let transfer_clipboard = Clipboard::new().await?;
            gtk4::glib::spawn_future_local(async move {
                // Built inside the moved-in async block (rather than passed
                // in already-borrowing transfer_clipboard) so the stream's
                // borrow of transfer_clipboard and transfer_clipboard's own
                // later use below (selection_write/selection_write_done)
                // stay within the same scope -- both are shared (&self)
                // borrows, so they can coexist once they're not fighting
                // over which scope owns transfer_clipboard.
                let transfers = match transfer_clipboard
                    .receive_selection_transfer::<RemoteDesktop>()
                    .await
                {
                    Ok(transfers) => transfers,
                    Err(err) => {
                        let msg = format!(
                            "[remote_input] failed to listen for clipboard selection transfers: {err}"
                        );
                        eprintln!("{msg}");
                        logger.write(&msg);
                        return;
                    }
                };
                let mut transfers = std::pin::pin!(transfers);
                while let Some((session, _mime_type, serial)) = transfers.next().await {
                    let Some(text) = pending_clipboard_write.borrow().clone() else {
                        // Shouldn't normally happen -- we only get transfer
                        // requests for a selection we ourselves set.
                        continue;
                    };
                    let result: ashpd::Result<()> = async {
                        let fd = transfer_clipboard.selection_write(&session, serial).await?;
                        let mut file = std::fs::File::from(std::os::fd::OwnedFd::from(fd));
                        file.write_all(text.as_bytes())
                            .map_err(|err| ashpd::Error::IO(std::io::Error::other(err)))?;
                        transfer_clipboard
                            .selection_write_done(&session, serial, true)
                            .await
                    }
                    .await;
                    if let Err(err) = result {
                        let msg = format!("[remote_input] clipboard selection-transfer failed: {err}");
                        eprintln!("{msg}");
                        logger.write(&msg);
                    }
                }
            });
        }

        // See `clipboard_offers_text`'s doc comment: tracks whether the
        // current clipboard owner offers CLIPBOARD_TEXT_MIME, so
        // copy_item_text can skip a doomed SelectionRead instead of
        // hitting the portal's hang-on-not-offered-format bug. Own
        // Clipboard handle for the same reason as the transfer listener
        // above (independent long-lived signal subscription).
        let clipboard_offers_text: Rc<Cell<bool>> = Rc::new(Cell::new(true));
        {
            let clipboard_offers_text = clipboard_offers_text.clone();
            let logger = logger.clone();
            let owner_clipboard = Clipboard::new().await?;
            gtk4::glib::spawn_future_local(async move {
                let changes = match owner_clipboard
                    .receive_selection_owner_changed::<RemoteDesktop>()
                    .await
                {
                    Ok(changes) => changes,
                    Err(err) => {
                        let msg = format!(
                            "[remote_input] failed to listen for clipboard ownership changes: {err}"
                        );
                        eprintln!("{msg}");
                        logger.write(&msg);
                        return;
                    }
                };
                let mut changes = std::pin::pin!(changes);
                while let Some((_session, change)) = changes.next().await {
                    let offers_text = change.mime_types().iter().any(|m| m == CLIPBOARD_TEXT_MIME);
                    clipboard_offers_text.set(offers_text);
                }
            });
        }

        Ok(Self {
            context,
            connection,
            keyboard,
            start: std::time::Instant::now(),
            clipboard,
            session,
            pending_clipboard_write,
            clipboard_offers_text,
            logger,
        })
    }

    async fn read_clipboard_once(&self) -> ashpd::Result<Option<String>> {
        let fd = self
            .clipboard
            .selection_read(&self.session, CLIPBOARD_TEXT_MIME)
            .await?;
        let file = std::fs::File::from(std::os::fd::OwnedFd::from(fd));
        let mut async_file =
            async_io::Async::new(file).map_err(|err| ashpd::Error::IO(std::io::Error::other(err)))?;
        let mut bytes = Vec::new();
        async_file
            .read_to_end(&mut bytes)
            .await
            .map_err(|err| ashpd::Error::IO(std::io::Error::other(err)))?;
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Bounds a single clipboard read so a portal-side failure can't hang
    /// forever. Confirmed live against `xdg-desktop-portal-kde`: when we ask
    /// for a mime type the current selection owner doesn't currently offer,
    /// its journal logs `"requesting not offered format"` immediately
    /// followed by `QDBusConnection: error: could not send reply message to
    /// service "": Marshalling failed: Invalid file descriptor passed in
    /// arguments` -- its own error-reply path is broken and never sends any
    /// D-Bus reply back (success or failure), so an unbounded await on
    /// `read_clipboard_once` hangs forever. Since `copy_item_text` is
    /// awaited inline in shortcuts.rs's hotkey-dispatch loop, a single hang
    /// here would silently wedge every future hotkey press until the
    /// process is restarted, not just fail one price-check.
    async fn read_clipboard_once_bounded(&self) -> ashpd::Result<Option<String>> {
        let read = std::pin::pin!(self.read_clipboard_once());
        let timeout = std::pin::pin!(async {
            gtk4::glib::timeout_future(std::time::Duration::from_millis(CLIPBOARD_READ_TIMEOUT_MS))
                .await;
            Err(ashpd::Error::IO(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "clipboard selection-read timed out (portal never replied)",
            )))
        });
        match futures_util::future::select(read, timeout).await {
            futures_util::future::Either::Left((result, _)) => result,
            futures_util::future::Either::Right((result, _)) => result,
        }
    }

    /// Skips the read entirely when `clipboard_offers_text` already says the
    /// current owner doesn't have our mime type -- see that field's doc
    /// comment. This is the main defense against the portal's
    /// not-offered-format hang bug; `read_clipboard_once_bounded`'s timeout
    /// is the backstop for the narrow race where ownership changes between
    /// this check and the actual read.
    async fn read_clipboard_if_offered(&self) -> ashpd::Result<Option<String>> {
        if !self.clipboard_offers_text.get() {
            return Ok(None);
        }
        self.read_clipboard_once_bounded().await
    }

    /// Polls the clipboard for item text (see CLIPBOARD_POLL_DELAY_MS/
    /// CLIPBOARD_POLL_LIMIT_MS) instead of assuming a single read some fixed
    /// delay after Ctrl+C is always fast enough. When `restore` is true,
    /// also writes back whatever was on the clipboard beforehand once
    /// polling finishes (success or timeout) -- unless that pre-existing
    /// value already looked like item text itself, in which case there's no
    /// way to tell whether it's stale leftover text or the fresh copy racing
    /// ahead of us, so nothing is restored (matches HostClipboard.ts's own
    /// defensive handling of the same race).
    ///
    /// Ported from HostClipboard.ts's readItemText(): clipboard content is
    /// cleared *before* polling starts, unconditionally (not gated on
    /// `restore` -- only the write-back afterward is). Without this,
    /// content left over from a previous price-check (or anything else)
    /// that already happens to look like valid item text gets accepted
    /// immediately on the poll loop's very first tick if the game hasn't
    /// updated the clipboard yet by then, silently showing stale data for
    /// whatever's actually under the cursor now -- confirmed live, this is
    /// exactly what was happening. Not porting the KDE "prevent empty
    /// clipboard" workaround's throwaway-marker detail (writing
    /// `__APT_FORCE_EMPTY_<timestamp>` instead of a plain empty string) --
    /// that's specifically about Electron's clipboard API being vetoed by
    /// Klipper; revisit only if writing an empty string here turns out not
    /// to actually clear it in testing.
    pub async fn copy_item_text(&self, restore: bool) -> ashpd::Result<Option<String>> {
        // Press *before* the snapshot/clear, unlike real HostClipboard.ts.
        // Tried the upstream order (snapshot first) live: the pre-read then
        // hits whatever owned the clipboard before (often not offering text)
        // and trips the portal's not-offered-format hang (see
        // read_clipboard_once_bounded), wedging later calls. Pressing first
        // means PoE already owns the selection by the time we read.
        self.press_ctrl_c();

        let previous = match self.read_clipboard_if_offered().await {
            Ok(text) => text,
            Err(err) => {
                let msg = format!("[remote_input] clipboard pre-read (for staleness check) failed: {err}");
                eprintln!("{msg}");
                self.logger.write(&msg);
                None
            }
        };

        if let Err(err) = self.write_clipboard_text("").await {
            let msg = format!("[remote_input] clipboard pre-clear failed: {err}");
            eprintln!("{msg}");
            self.logger.write(&msg);
            // Keep going -- polling can still work, just with a higher risk
            // of the stale-content race this clear exists to prevent.
        }

        // Real wall-clock time, not a count of ticks -- a single tick can
        // itself cost up to CLIPBOARD_READ_TIMEOUT_MS when the portal hangs,
        // so assuming every tick costs exactly CLIPBOARD_POLL_DELAY_MS would
        // let this loop run several times longer than intended before
        // giving up.
        let start = std::time::Instant::now();
        let result = loop {
            gtk4::glib::timeout_future(std::time::Duration::from_millis(CLIPBOARD_POLL_DELAY_MS))
                .await;

            let text = match self.read_clipboard_if_offered().await {
                Ok(text) => text,
                Err(err) => {
                    let msg = format!("[remote_input] clipboard poll read failed: {err}");
                    eprintln!("{msg}");
                    self.logger.write(&msg);
                    None
                }
            };
            if text.as_deref().is_some_and(looks_like_item_text) {
                break text;
            }
            if start.elapsed().as_millis() as u64 >= CLIPBOARD_POLL_LIMIT_MS {
                let msg = "[remote_input] clipboard poll timed out, no item text found";
                println!("{msg}");
                self.logger.write(msg);
                break None;
            }
        };

        if restore {
            // Don't restore `previous` if it already looked like item text
            // itself -- there's no way to tell whether that was genuinely
            // stale leftover text or the fresh copy racing ahead of us, so
            // restoring it would risk leaving stale item text sitting in
            // the clipboard instead of the empty string this cleared it to.
            let restore_value = match previous.as_deref() {
                Some(text) if !looks_like_item_text(text) => text,
                _ => "",
            };
            if let Err(err) = self.write_clipboard_text(restore_value).await {
                let msg = format!("[remote_input] clipboard restore failed: {err}");
                eprintln!("{msg}");
                self.logger.write(&msg);
            }
        }

        Ok(result)
    }

    // NOTE (verify live, same category as this file's other portal-behavior
    // assumptions confirmed against a real KDE session): unclear whether
    // SetSelection makes us the clipboard owner only until some other app's
    // own copy naturally reclaims it (matching normal desktop clipboard
    // behavior), or whether it "sticks" until we call SetSelection again. If
    // restoring the clipboard ever shadows the user's later copies, this is
    // the mechanism to revisit.
    async fn write_clipboard_text(&self, text: &str) -> ashpd::Result<()> {
        self.pending_clipboard_write.replace(Some(text.to_string()));
        self.clipboard
            .set_selection(
                &self.session,
                SetSelectionOptions::default().set_mime_types(&["text/plain;charset=utf-8"]),
            )
            .await
    }

    pub fn press_ctrl_c(&self) {
        self.press_keys(&[
            (KEY_LEFTCTRL, ei::keyboard::KeyState::Press),
            (KEY_C, ei::keyboard::KeyState::Press),
            (KEY_C, ei::keyboard::KeyState::Released),
            (KEY_LEFTCTRL, ei::keyboard::KeyState::Released),
        ]);
    }

    /// Port of Shortcuts.ts's key-up toggle before acting on a hotkey: the
    /// user is often still physically holding the shortcut (e.g. the `D` of
    /// Ctrl+D) when we inject Ctrl+C, and PoE then sees Ctrl+D+C and
    /// sometimes ignores it (observed live as "clipboard poll timed out").
    /// ponytail: uses the renderer's configured `shortcut`; if the user
    /// rebound the hotkey in KDE System Settings these may be the wrong keys
    /// (stray key-ups are harmless). Upgrade path: track the portal's
    /// bound `trigger_description` instead.
    pub fn release_shortcut_keys(&self, shortcut: &str) {
        let keys: Vec<_> = shortcut
            .rsplit(" + ")
            .filter_map(key_to_evdev)
            .map(|code| (code, ei::keyboard::KeyState::Released))
            .collect();
        self.press_keys(&keys);
    }

    fn press_keys(&self, keys: &[(u32, ei::keyboard::KeyState)]) {
        let handle = self.keyboard.borrow();
        let Some(handle) = handle.as_ref() else {
            let msg = "[remote_input] keyboard device not ready yet";
            eprintln!("{msg}");
            self.logger.write(msg);
            return;
        };

        for &(keycode, state) in keys {
            handle.keyboard.key(keycode, state);
            let timestamp = self.start.elapsed().as_micros() as u64;
            handle.device.frame(self.connection.serial(), timestamp);
        }
        let _ = self.context.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_to_evdev_matches_input_event_codes() {
        let cases = [
            ("A", 30), ("C", 46), ("D", 32), ("Z", 44), ("1", 2), ("9", 10), ("0", 11),
            ("F1", 59), ("F10", 68), ("F11", 87), ("F13", 183), ("F24", 194),
            ("Ctrl", 29), ("Space", 57), ("Numpad0", 82),
        ];
        for (key, code) in cases {
            assert_eq!(key_to_evdev(key), Some(code), "{key}");
        }
        assert_eq!(key_to_evdev("F25"), None);
        assert_eq!(key_to_evdev(""), None);
    }

    #[test]
    fn looks_like_item_text_matches_every_language_signature() {
        for sig in ITEM_TEXT_SIGNATURES {
            assert!(looks_like_item_text(&format!("{sig}Body Armours\nRarity: Rare")));
        }
    }

    #[test]
    fn looks_like_item_text_rejects_unrelated_text() {
        assert!(!looks_like_item_text(""));
        assert!(!looks_like_item_text("just some random clipboard text"));
        assert!(!looks_like_item_text("item class: lowercase doesn't count"));
    }
}
