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

use crate::host_config::key_to_evdev;
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

// XKB modifier mask bits as reported by ei_keyboard.modifiers (depressed).
pub const MOD_SHIFT: u32 = 1 << 0;
pub const MOD_CTRL: u32 = 1 << 2;
pub const MOD_ALT: u32 = 1 << 3;

/// One key tap, optionally with Ctrl held (uIOhook.keyTap(key, [Ctrl])).
type Tap = (bool, &'static str);

// text-box.ts's AUTO_CLEAR: chat prefixes PoE clears the input for itself.
const AUTO_CLEAR: &[char] = &['#', '%', '@', '$', '&', '/'];
const PLACEHOLDER_LAST: &str = "@last";

/// Port of text-box.ts's typeInChat: what to put on the clipboard, then
/// which keys to tap. `@last` as a prefix/suffix replies to the last
/// whisper (Ctrl+Enter reopens chat with the last whisper target).
fn chat_sequence(text: &str, send: bool) -> (String, Vec<Tap>) {
    let mut taps = Vec::new();
    let clip = if text.starts_with(PLACEHOLDER_LAST) {
        taps.push((true, "Enter"));
        text.chars().skip(PLACEHOLDER_LAST.len() + 1).collect()
    } else if let Some(rest) = text.strip_suffix(PLACEHOLDER_LAST) {
        // Home twice: the first one focuses the input on a controller.
        taps.extend([(true, "Enter"), (false, "Home"), (false, "Home"), (false, "Delete")]);
        rest.to_string()
    } else {
        taps.push((false, "Enter"));
        if !text.starts_with(AUTO_CLEAR) {
            taps.push((true, "A"));
        }
        text.to_string()
    };
    taps.push((true, "V"));
    if send {
        // Send, then reopen chat and step back to restore the last chat.
        taps.extend([
            (false, "Enter"),
            (false, "Enter"),
            (false, "ArrowUp"),
            (false, "ArrowUp"),
            (false, "Escape"),
        ]);
    }
    (clip, taps)
}

/// Port of text-box.ts's stashSearch key sequence.
const STASH_SEARCH_TAPS: &[Tap] = &[(true, "F"), (true, "V"), (false, "Enter")];

// HostClipboard.ts's RESTORE_AFTER.
const RESTORE_AFTER_MS: u64 = 120;

fn is_mod_key(key: &str) -> bool {
    matches!(key, "Ctrl" | "Shift" | "Alt")
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
    // Latest depressed-modifier mask from the EIS KeyboardModifiers events,
    // which reflect the seat (the user's real keyboard), not just our own
    // injected keys — see overlay_visibility.rs.
    depressed_mods: Rc<Cell<u32>>,
    // HostClipboard.restoreShortly's isRestored throttle: drops a paste
    // request while the previous one's restore is still pending.
    paste_busy: Rc<Cell<bool>>,
    logger: Arc<Logger>,
}

impl RemoteInput {
    /// `on_modifiers` receives the seat's depressed-modifier mask on every
    /// change (see overlay_visibility.rs).
    pub async fn setup(
        logger: Arc<Logger>,
        on_modifiers: impl Fn(u32) + 'static,
    ) -> ashpd::Result<Self> {
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
        let depressed_mods = Rc::new(Cell::new(0));
        {
            let keyboard = keyboard.clone();
            let depressed_mods = depressed_mods.clone();
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
                        EiEvent::KeyboardModifiers(mods) => {
                            depressed_mods.set(mods.depressed);
                            on_modifiers(mods.depressed);
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
                        logger.error(&format!(
                            "[remote_input] failed to listen for clipboard selection transfers: {err}"
                        ));
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
                        logger.error(&format!("[remote_input] clipboard selection-transfer failed: {err}"));
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
                        logger.error(&format!(
                            "[remote_input] failed to listen for clipboard ownership changes: {err}"
                        ));
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
            depressed_mods,
            paste_busy: Rc::new(Cell::new(false)),
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
        let timeout = std::time::Duration::from_millis(CLIPBOARD_READ_TIMEOUT_MS);
        crate::with_timeout(timeout, self.read_clipboard_once())
            .await
            .unwrap_or_else(|| {
                Err(ashpd::Error::IO(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "clipboard selection-read timed out (portal never replied)",
                )))
            })
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
    pub async fn copy_item_text(
        &self,
        restore: bool,
        hotkey: &str,
        keep_mod_keys: bool,
    ) -> ashpd::Result<Option<String>> {
        // Press *before* the snapshot/clear, unlike real HostClipboard.ts.
        // Tried the upstream order (snapshot first) live: the pre-read then
        // hits whatever owned the clipboard before (often not offering text)
        // and trips the portal's not-offered-format hang (see
        // read_clipboard_once_bounded), wedging later calls. Pressing first
        // means PoE already owns the selection by the time we read.
        self.release_shortcut_keys(hotkey, keep_mod_keys);
        self.press_copy(keep_mod_keys);

        let previous = match self.read_clipboard_if_offered().await {
            Ok(text) => text,
            Err(err) => {
                self.logger.error(&format!("[remote_input] clipboard pre-read (for staleness check) failed: {err}"));
                None
            }
        };

        if let Err(err) = self.write_clipboard_text("").await {
            self.logger.error(&format!("[remote_input] clipboard pre-clear failed: {err}"));
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
                    self.logger.error(&format!("[remote_input] clipboard poll read failed: {err}"));
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
                self.logger.error(&format!("[remote_input] clipboard restore failed: {err}"));
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
                SetSelectionOptions::default().set_mime_types(&[CLIPBOARD_TEXT_MIME]),
            )
            .await
    }

    pub async fn type_in_chat(&self, text: &str, send: bool, hotkey: &str, restore: bool) {
        let (clip, taps) = chat_sequence(text, send);
        self.paste_with_taps(&clip, &taps, hotkey, restore).await;
    }

    pub async fn stash_search(&self, text: &str, hotkey: &str, restore: bool) {
        self.paste_with_taps(text, STASH_SEARCH_TAPS, hotkey, restore).await;
    }

    /// Port of HostClipboard.restoreShortly around a key-tap sequence:
    /// put `clip` on the clipboard, tap `taps`, and restore the previous
    /// clipboard RESTORE_AFTER_MS later if `restore` (restoreClipboard).
    async fn paste_with_taps(&self, clip: &str, taps: &[Tap], hotkey: &str, restore: bool) {
        if self.paste_busy.replace(true) {
            println!("[remote_input] paste dropped: previous one still restoring");
            return;
        }
        // Reset even if the caller's timeout drops this future mid-way.
        struct Busy(Rc<Cell<bool>>);
        impl Drop for Busy {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _busy = Busy(self.paste_busy.clone());

        let saved = if restore {
            self.read_clipboard_if_offered().await.ok().flatten()
        } else {
            None
        };
        if let Err(err) = self.write_clipboard_text(clip).await {
            self.logger.error(&format!("[remote_input] paste: clipboard write failed: {err}"));
            return;
        }

        use ei::keyboard::KeyState::{Press, Released};
        self.release_shortcut_keys(hotkey, false);
        for &(ctrl, key) in taps {
            let Some(code) = key_to_evdev(key) else { continue };
            let mut seq = Vec::with_capacity(4);
            if ctrl {
                seq.push((KEY_LEFTCTRL, Press));
            }
            seq.extend([(code, Press), (code, Released)]);
            if ctrl {
                seq.push((KEY_LEFTCTRL, Released));
            }
            self.press_keys(&seq);
        }

        gtk4::glib::timeout_future(std::time::Duration::from_millis(RESTORE_AFTER_MS)).await;
        if restore
            && let Err(err) = self.write_clipboard_text(saved.as_deref().unwrap_or("")).await
        {
            self.logger.error(&format!("[remote_input] paste: clipboard restore failed: {err}"));
        }
    }

    pub fn depressed_mods(&self) -> u32 {
        self.depressed_mods.get()
    }

    /// Ctrl+C, but like pressKeysToCopyItemText, don't toggle Ctrl if the
    /// user is already holding it (keepModKeys hotkeys leave it held):
    /// injecting a Ctrl release would clear the seat's Ctrl state while the
    /// physical key is still down, breaking hold-to-pin.
    /// Only trusted with `keep_mod_keys`: otherwise release_shortcut_keys
    /// just injected a Ctrl release that the mask doesn't reflect yet.
    fn press_copy(&self, keep_mod_keys: bool) {
        use ei::keyboard::KeyState::{Press, Released};
        if keep_mod_keys && self.depressed_mods() & MOD_CTRL != 0 {
            self.press_keys(&[(KEY_C, Press), (KEY_C, Released)]);
        } else {
            self.press_keys(&[
                (KEY_LEFTCTRL, Press),
                (KEY_C, Press),
                (KEY_C, Released),
                (KEY_LEFTCTRL, Released),
            ]);
        }
    }

    /// Port of Shortcuts.ts's key-up toggle before acting on a hotkey: the
    /// user is often still physically holding the shortcut (e.g. the `D` of
    /// Ctrl+D) when we inject Ctrl+C, and PoE then sees Ctrl+D+C and
    /// sometimes ignores it (observed live as "clipboard poll timed out").
    /// ponytail: uses the renderer's configured `shortcut`; if the user
    /// rebound the hotkey in KDE System Settings these may be the wrong keys
    /// (stray key-ups are harmless). Upgrade path: track the portal's
    /// bound `trigger_description` instead.
    /// With `keep_mod_keys` only the non-modifier key is released, matching
    /// Shortcuts.ts.
    fn release_shortcut_keys(&self, shortcut: &str, keep_mod_keys: bool) {
        let keys: Vec<_> = shortcut
            .rsplit(" + ")
            .filter(|key| !(keep_mod_keys && is_mod_key(key)))
            .filter_map(key_to_evdev)
            .map(|code| (code, ei::keyboard::KeyState::Released))
            .collect();
        self.press_keys(&keys);
    }

    fn press_keys(&self, keys: &[(u32, ei::keyboard::KeyState)]) {
        let handle = self.keyboard.borrow();
        let Some(handle) = handle.as_ref() else {
            self.logger.error("[remote_input] keyboard device not ready yet");
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
    fn chat_sequence_matches_text_box_ts() {
        let (clip, taps) = chat_sequence("/hideout", true);
        assert_eq!(clip, "/hideout");
        assert_eq!(
            taps,
            vec![
                (false, "Enter"),
                (true, "V"),
                (false, "Enter"),
                (false, "Enter"),
                (false, "ArrowUp"),
                (false, "ArrowUp"),
                (false, "Escape"),
            ]
        );

        // Not auto-cleared by PoE -> select-all first so V replaces it.
        let (clip, taps) = chat_sequence("ty", false);
        assert_eq!(clip, "ty");
        assert_eq!(taps, vec![(false, "Enter"), (true, "A"), (true, "V")]);

        let (clip, taps) = chat_sequence("@last ty for trade", false);
        assert_eq!(clip, "ty for trade");
        assert_eq!(taps, vec![(true, "Enter"), (true, "V")]);

        let (clip, taps) = chat_sequence("/invite @last", false);
        assert_eq!(clip, "/invite ");
        assert_eq!(
            taps,
            vec![(true, "Enter"), (false, "Home"), (false, "Home"), (false, "Delete"), (true, "V")]
        );

        for (_, key) in chat_sequence("x", true).1.iter().chain(STASH_SEARCH_TAPS) {
            assert!(key_to_evdev(key).is_some(), "{key}");
        }
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
