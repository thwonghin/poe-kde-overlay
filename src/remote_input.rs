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

// Evdev keycodes (linux/input-event-codes.h), injected over libei
// (ConnectToEIS). The portal's plain NotifyKeyboardKeycode has no effect on
// this KDE version even with the session granted; EIS is the path KWin's
// remote-input backend (and KDE Connect) actually uses.
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

// HostClipboard.ts's POLL_DELAY/POLL_LIMIT: after Ctrl+C, poll every 48ms
// for the game's copy to land; give up after ~500ms.
const CLIPBOARD_POLL_DELAY_MS: u64 = 48;
const CLIPBOARD_POLL_LIMIT_MS: u64 = 500;

// Bounds one portal SelectionRead (see read_clipboard_once_bounded).
const CLIPBOARD_READ_TIMEOUT_MS: u64 = 200;

const CLIPBOARD_TEXT_MIME: &str = "text/plain;charset=utf-8";

// HostClipboard.ts's LANGUAGE_DETECTOR, both renderers' combined (kept in
// sync by the `item_text_signatures_match_both_vendors` test): the
// first line of a real PoE item clipboard dump always starts with one of
// these (localized "Item Class: "), so a prefix match is enough to tell a
// genuine item paste apart from whatever else happened to be on the
// clipboard. PoE2's uncut skill gems have no Item Class line and start at
// "Rarity: " instead (EE2's uncutSkillGemLine); harmless for PoE1.
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
    "アイテムクラス: ",       // ja (poe2)
    // poe2 uncut skill gems (th and cmn-Hans use the English line)
    "Rarity: ",
    "Редкость: ",
    "Rareté: ",
    "Seltenheit: ",
    "Raridade: ",
    "Rareza: ",
    "아이템 희귀도: ",
    "稀有度: ",
    "レアリティ: ",
];

/// The copy keys as (evdev key, pressed?), like pressKeysToCopyItemText:
/// modifiers the user already holds (`held`, an XKB depressed mask) aren't
/// toggled, since injecting their release would clear the seat's state
/// while the physical key is still down, breaking hold-to-pin.
fn copy_sequence(show_mods_key: Option<&str>, held: u32) -> Vec<(u32, bool)> {
    // mergeTwoHotkeys("Ctrl + C", showModsKey) minus C, in hotkeyToString's
    // order: Ctrl, Shift, Alt, then any plain key.
    let extra: Vec<&str> = show_mods_key.map_or(Vec::new(), |k| k.split(" + ").collect());
    let mut keys: Vec<&str> = ["Ctrl", "Shift", "Alt"]
        .into_iter()
        .filter(|m| *m == "Ctrl" || extra.contains(m))
        .collect();
    for key in extra {
        if !is_mod_key(key) && key != "C" && !keys.contains(&key) {
            keys.push(key);
        }
    }
    let held_mask = |key: &str| match key {
        "Ctrl" => MOD_CTRL,
        "Shift" => MOD_SHIFT,
        "Alt" => MOD_ALT,
        _ => 0,
    };
    let mods: Vec<u32> = keys
        .into_iter()
        .filter(|key| held & held_mask(key) == 0)
        .filter_map(key_to_evdev)
        .collect();
    let mut seq: Vec<(u32, bool)> = mods.iter().map(|&k| (k, true)).collect();
    seq.extend([(KEY_C, true), (KEY_C, false)]);
    seq.extend(mods.iter().rev().map(|&k| (k, false)));
    seq
}

fn looks_like_item_text(text: &str) -> bool {
    ITEM_TEXT_SIGNATURES.iter().any(|sig| text.starts_with(sig))
}

/// One tick of `copy_item_text`'s clipboard poll.
#[derive(Debug, PartialEq)]
enum PollStep {
    Found,
    Wait,
    /// Press Ctrl+C again (at most once per copy).
    Recopy,
    /// `changed`: some app took the clipboard during the poll.
    TimedOut { changed: bool },
}

#[derive(Default)]
struct ClipboardPoll {
    changed: bool,
    recopied: bool,
}

impl ClipboardPoll {
    fn step(&mut self, pinged: bool, has_item_text: bool, elapsed_ms: u64) -> PollStep {
        self.changed |= pinged;
        if has_item_text {
            return PollStep::Found;
        }
        // Checked before the recopy: a Ctrl+C sent after giving up would
        // only reach the game.
        if elapsed_ms >= CLIPBOARD_POLL_LIMIT_MS {
            return PollStep::TimedOut { changed: self.changed };
        }
        // Another app took the clipboard, yet no item text: most likely
        // PoE copied *before* our clear, which then overwrote it (the ping
        // can be processed after the clear either way). Copy once more;
        // harmless if that wasn't it.
        if pinged && !self.recopied {
            self.recopied = true;
            return PollStep::Recopy;
        }
        PollStep::Wait
    }
}

/// What `restore` writes back: the pre-copy clipboard, unless it looked
/// like item text itself (stale or fresh can't be told apart).
fn restore_value(previous: Option<&str>) -> &str {
    match previous {
        Some(text) if !looks_like_item_text(text) => text,
        _ => "",
    }
}

fn restore_token_path() -> std::path::PathBuf {
    crate::xdg::data_dir().join("poe-kde-overlay/remote_desktop_restore_token")
}

struct KeyboardHandle {
    device: ei::Device,
    keyboard: ei::Keyboard,
}

pub struct RemoteInput {
    context: ei::Context,
    connection: event::Connection,
    keyboard: Rc<RefCell<Option<KeyboardHandle>>>,
    // Reference point for frame() timestamps (monotonic µs). Real values,
    // not 0: EIS may order or drop identically-timestamped events.
    start: std::time::Instant,
    // Portal clipboard tied to this RemoteDesktop session: readable
    // regardless of local focus, unlike gdk::Clipboard (Wayland gates that
    // by keyboard focus, which the overlay never holds while PoE does).
    clipboard: Clipboard,
    // Kept alive, not otherwise used post-setup: dropping the D-Bus portal
    // session could tear down the EIS grant it authorized, even though the
    // raw EIS socket (`context`) is a separate connection.
    #[allow(dead_code)]
    session: Session<RemoteDesktop>,
    // What we last claimed clipboard ownership of (write_clipboard_text);
    // served to SelectionTransfer requests by the listener in `setup`.
    pending_clipboard_write: Rc<RefCell<Option<String>>>,
    // Whether the current clipboard owner offers CLIPBOARD_TEXT_MIME, from
    // SelectionOwnerChanged. Reading a type the owner doesn't offer hits the
    // portal hang (read_clipboard_once_bounded), and our own WebView
    // sometimes owns the clipboard with only a WebKit-internal type. Starts
    // `true`: current state can't be queried, only changes observed.
    clipboard_offers_text: Rc<Cell<bool>>,
    // Pinged when another app (PoE, after our Ctrl+C) takes the clipboard
    // with text on it, so copy_item_text's poll wakes immediately instead
    // of on its next fixed tick.
    text_copied: async_channel::Receiver<()>,
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
            .handshake_async_io("poe-kde-overlay", ei::handshake::ContextType::Sender)
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
                    // Not modifier changes: those fire on every Ctrl/Alt/Shift
                    // press anywhere on the desktop.
                    if !matches!(event, EiEvent::KeyboardModifiers(_)) {
                        debug!("[remote_input] eis event: {event:?}");
                    }
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

        // The portal can't just "set the clipboard": we claim ownership via
        // SetSelection (write_clipboard_text) and then serve each
        // SelectionTransfer request while we own it. Own Clipboard handle;
        // each stream item carries its own Session.
        let pending_clipboard_write: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        {
            let pending_clipboard_write = pending_clipboard_write.clone();
            let logger = logger.clone();
            let transfer_clipboard = Clipboard::new().await?;
            gtk4::glib::spawn_future_local(async move {
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

        // Keeps `clipboard_offers_text` in sync.
        let clipboard_offers_text: Rc<Cell<bool>> = Rc::new(Cell::new(true));
        let (text_copied_tx, text_copied) = async_channel::bounded(1);
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
                    if offers_text && change.session_is_owner() != Some(true) {
                        // Full = a wake-up is already pending; one is enough.
                        let _ = text_copied_tx.try_send(());
                    }
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
            text_copied,
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

    /// xdg-desktop-portal-kde never replies to a SelectionRead for a type
    /// the current owner doesn't offer (journal: "requesting not offered
    /// format", then "Marshalling failed: Invalid file descriptor"), so an
    /// unbounded read hangs forever — and with it the hotkey loop.
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

    /// Main defense against that hang; the timeout is the backstop for an
    /// ownership change racing this check.
    async fn read_clipboard_if_offered(&self) -> ashpd::Result<Option<String>> {
        if !self.clipboard_offers_text.get() {
            return Ok(None);
        }
        self.read_clipboard_once_bounded().await
    }

    /// Port of HostClipboard.ts's readItemText(): press Ctrl+C, clear the
    /// clipboard, then poll until item text shows up. The clear is
    /// unconditional — otherwise item text left from a previous check is
    /// accepted on the first tick, showing the wrong item. With `restore`,
    /// the previous content is written back afterwards, unless it looked
    /// like item text itself (stale or fresh can't be told apart).
    ///
    /// ponytail: upstream's `__APT_FORCE_EMPTY_<timestamp>` marker (Klipper
    /// vetoing Electron's empty writes) isn't ported; a plain "" clears fine
    /// through the portal so far.
    pub async fn copy_item_text(
        &self,
        restore: bool,
        hotkey: &str,
        keep_mod_keys: bool,
        show_mods_key: Option<&str>,
    ) -> ashpd::Result<Option<String>> {
        // Press *before* the snapshot/clear, unlike HostClipboard.ts: a
        // snapshot first reads whatever owned the clipboard before (often
        // not offering text) and trips the portal hang. Pressing first means
        // PoE usually owns the selection by the time we read.
        //
        // Drop a wake-up left over from an earlier copy; only one caused by
        // this Ctrl+C should end the first wait early.
        while self.text_copied.try_recv().is_ok() {}
        self.release_shortcut_keys(hotkey, keep_mod_keys);
        self.press_copy(keep_mod_keys, show_mods_key);

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

        // Wall-clock, not a tick count: a tick can cost up to
        // CLIPBOARD_READ_TIMEOUT_MS when a read hangs.
        let start = std::time::Instant::now();
        let mut poll = ClipboardPoll::default();
        let result = loop {
            // Usually ends early: PoE taking the clipboard pings
            // `text_copied`. The fixed delay is the fallback if the portal
            // doesn't signal (upstream polls on this delay alone).
            let delay = std::time::Duration::from_millis(CLIPBOARD_POLL_DELAY_MS);
            let pinged = crate::with_timeout(delay, self.text_copied.recv())
                .await
                .is_some_and(|r| r.is_ok());

            let text = match self.read_clipboard_if_offered().await {
                Ok(text) => text,
                Err(err) => {
                    self.logger.error(&format!("[remote_input] clipboard poll read failed: {err}"));
                    None
                }
            };
            let has_item_text = text.as_deref().is_some_and(looks_like_item_text);
            match poll.step(pinged, has_item_text, start.elapsed().as_millis() as u64) {
                PollStep::Found => break text,
                PollStep::Wait => {}
                PollStep::Recopy => {
                    debug!("[remote_input] clipboard changed without item text; copying again");
                    self.press_copy(keep_mod_keys, show_mods_key);
                }
                PollStep::TimedOut { changed } => {
                    self.logger.error(if changed {
                        "[remote_input] clipboard poll timed out: the clipboard changed but never held item text"
                    } else {
                        "[remote_input] clipboard poll timed out: the clipboard never changed (Ctrl+C not seen by PoE?)"
                    });
                    break None;
                }
            }
        };

        if restore
            && let Err(err) = self.write_clipboard_text(restore_value(previous.as_deref())).await
        {
            self.logger.error(&format!("[remote_input] clipboard restore failed: {err}"));
        }

        Ok(result)
    }

    // If restoring the clipboard ever shadows the user's later copies, check
    // whether SetSelection ownership "sticks" instead of being reclaimed by
    // the next app that copies.
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

    /// Ctrl(+showModsKey)+C (game_config.rs) via `copy_sequence`.
    /// The held mask is only trusted with `keep_mod_keys`: otherwise
    /// release_shortcut_keys just injected releases it doesn't reflect yet.
    fn press_copy(&self, keep_mod_keys: bool, show_mods_key: Option<&str>) {
        use ei::keyboard::KeyState::{Press, Released};
        let held = if keep_mod_keys { self.depressed_mods() } else { 0 };
        let seq: Vec<_> = copy_sequence(show_mods_key, held)
            .into_iter()
            .map(|(key, down)| (key, if down { Press } else { Released }))
            .collect();
        self.press_keys(&seq);
    }

    /// Port of Shortcuts.ts's key-up toggle before acting on a hotkey: the
    /// user is often still physically holding the shortcut (e.g. the `D` of
    /// Ctrl+D) when we inject Ctrl+C, and PoE then sees Ctrl+D+C and
    /// sometimes ignores it (observed live as "clipboard poll timed out").
    /// `shortcut` is what KDE actually has bound when that could be parsed
    /// (shortcuts.rs's `Bindings::hotkey`); stray key-ups are harmless.
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

    /// Upstream adding (or renaming) a language's first line fails here
    /// instead of silently timing out the clipboard poll for that language,
    /// and an entry neither vendor still has fails too.
    #[test]
    fn item_text_signatures_match_both_vendors() {
        let mut upstream = std::collections::BTreeSet::new();
        for source in crate::game::vendor_sources("main/src/shortcuts/HostClipboard.ts") {
            let detector = &source[source.find("LANGUAGE_DETECTOR =").expect("LANGUAGE_DETECTOR moved")..];
            for key in ["firstLine", "uncutSkillGemLine"] {
                upstream.extend(crate::game::ts_string_values(detector, key));
            }
        }
        let ours: std::collections::BTreeSet<String> =
            ITEM_TEXT_SIGNATURES.iter().map(|s| s.to_string()).collect();
        assert_eq!(ours, upstream, "ITEM_TEXT_SIGNATURES out of sync with upstream LANGUAGE_DETECTOR");
    }

    #[test]
    fn copy_sequence_adds_alt_and_skips_held_mods() {
        let (ctrl, alt, c) = (KEY_LEFTCTRL, key_to_evdev("Alt").unwrap(), KEY_C);
        let (shift, z) = (key_to_evdev("Shift").unwrap(), key_to_evdev("Z").unwrap());
        assert_eq!(copy_sequence(None, 0), [(ctrl, true), (c, true), (c, false), (ctrl, false)]);
        assert_eq!(
            copy_sequence(Some("Alt"), 0),
            [(ctrl, true), (alt, true), (c, true), (c, false), (alt, false), (ctrl, false)]
        );
        assert_eq!(copy_sequence(Some("Alt"), MOD_CTRL), [(alt, true), (c, true), (c, false), (alt, false)]);
        assert_eq!(copy_sequence(Some("Alt"), MOD_CTRL | MOD_ALT), [(c, true), (c, false)]);
        // A rebound key, plain or with a modifier; Ctrl isn't pressed twice.
        assert_eq!(
            copy_sequence(Some("Shift + Z"), 0),
            [(ctrl, true), (shift, true), (z, true), (c, true), (c, false), (z, false), (shift, false), (ctrl, false)]
        );
        assert_eq!(copy_sequence(Some("Ctrl"), 0), copy_sequence(None, 0));
    }

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

    #[test]
    fn clipboard_poll_recopies_once_and_reports_why_it_timed_out() {
        let mut poll = ClipboardPoll::default();
        assert_eq!(poll.step(false, false, 48), PollStep::Wait);
        // PoE copied before our clear: the ping brings no item text.
        assert_eq!(poll.step(true, false, 60), PollStep::Recopy);
        assert_eq!(poll.step(true, false, 100), PollStep::Wait, "only one recopy");
        assert_eq!(poll.step(true, true, 110), PollStep::Found);

        // Never pinged: times out saying the clipboard never changed.
        let mut silent = ClipboardPoll::default();
        assert_eq!(silent.step(false, false, CLIPBOARD_POLL_LIMIT_MS), PollStep::TimedOut { changed: false });
        // Pinged earlier, still nothing: says it changed. No recopy on the
        // tick that gives up.
        let mut noisy = ClipboardPoll::default();
        assert_eq!(noisy.step(false, false, 48), PollStep::Wait);
        assert_eq!(noisy.step(true, false, CLIPBOARD_POLL_LIMIT_MS), PollStep::TimedOut { changed: true });
        // Item text wins even past the limit.
        assert_eq!(ClipboardPoll::default().step(false, true, 9999), PollStep::Found);
    }

    #[test]
    fn restore_skips_item_text() {
        assert_eq!(restore_value(Some("my notes")), "my notes");
        assert_eq!(restore_value(Some("Item Class: Body Armours\nRarity: Rare")), "");
        // poe2 uncut skill gem: no Item Class line.
        assert!(looks_like_item_text("Rarity: Currency\nUncut Skill Gem"));
        assert_eq!(restore_value(None), "");
    }
}
