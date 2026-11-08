//! Port of `main/src/windowing/OverlayVisibility.ts`: holding Alt on its own
//! hides the overlay (after 85ms if interactable, 275ms if not) so the player
//! can glance at the game; anything else shows it again.
//!
//! Driven by the EIS keyboard's `KeyboardModifiers` events, which KWin sends
//! for the *seat's* modifier state (the user's real keyboard, not just our
//! injected keys — observed live: the Alt of an Alt+Tab shows up there).
//! ponytail: upstream also un-hides on any non-modifier keydown or mouse
//! move while Alt is held; we only see modifier transitions, so the overlay
//! stays hidden until Alt is released or another modifier joins it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib;
use serde_json::json;

use crate::Overlay;
use crate::remote_input::{MOD_ALT as ALT, MOD_CTRL as CTRL, MOD_SHIFT as SHIFT};

fn is_alt_only(depressed: u32) -> bool {
    depressed & (SHIFT | CTRL | ALT) == ALT
}

pub struct OverlayVisibility {
    overlay: Rc<Overlay>,
    visible: Cell<bool>,
    timer: RefCell<Option<glib::SourceId>>,
}

impl OverlayVisibility {
    pub fn new(overlay: Rc<Overlay>) -> Rc<Self> {
        Rc::new(Self {
            overlay,
            visible: Cell::new(true),
            timer: RefCell::new(None),
        })
    }

    pub fn on_modifiers(self: &Rc<Self>, depressed: u32) {
        if is_alt_only(depressed) {
            self.make_invisible();
        } else {
            self.make_visible();
        }
    }

    fn make_visible(&self) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        } else if !self.visible.get() {
            self.set_visible(true);
        }
    }

    fn make_invisible(self: &Rc<Self>) {
        if !self.visible.get() || self.timer.borrow().is_some() {
            return;
        }
        let delay = if self.overlay.click_through.get() { 275 } else { 85 };
        let this = self.clone();
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
            this.timer.replace(None);
            this.set_visible(false);
        });
        self.timer.replace(Some(id));
    }

    fn set_visible(&self, visible: bool) {
        self.visible.set(visible);
        self.overlay
            .events
            .broadcast("MAIN->OVERLAY::visibility", json!({ "isVisible": visible }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_lone_alt_hides() {
        assert!(is_alt_only(ALT));
        assert!(is_alt_only(ALT | 2)); // Caps Lock bit doesn't count
        assert!(!is_alt_only(0));
        assert!(!is_alt_only(ALT | SHIFT));
        assert!(!is_alt_only(ALT | CTRL));
        assert!(!is_alt_only(CTRL));
    }
}
