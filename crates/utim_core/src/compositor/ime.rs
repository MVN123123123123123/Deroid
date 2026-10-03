//! Integrated Virtual Keyboard (IME - Gboard Style).
//! Conforms to Wayland text-input-v3 and zwp_input_method_v2 protocols.
//! Provides smooth viewport push animation, QWERTY/symbols/numeric layouts,
//! commit_string, delete_surrounding_text.

use crate::graphics::drm_kms::{SpringConfig, SpringSimulation};

/// Which page of the on-screen keyboard is up.
///
/// **The reference has no keyboard to transcribe, and this is why.** A launcher
/// does not own an IME: `ExtendedEditText.showSoftInputInternal` hands the field
/// to the platform `InputMethodManager` (`ExtendedEditText.java:128-136`) and
/// `ActivityContext.isHardwareKeyboard` branches on `Configuration.KEYBOARD_QWERTY`
/// rather than on any key set (`ActivityContext.java:391-395`). The manifest
/// keeps `keyboardHidden` in `configChanges` (`AndroidManifest.xml:60`) because
/// the launcher expects a *hardware* keyboard and never hosts a software one.
/// `src/com/android/launcher3/keyboard/` holds `FocusedItemDecorator`,
/// `FocusIndicatorHelper`, `ItemFocusIndicatorHelper` and
/// `KeyboardDragAndDropView` -- focus and drag-and-drop for a physical keyboard,
/// not an on-screen one -- and there is no `packages/inputmethods` in the tree.
///
/// So the `?123` / `ABC` / `123` labels below are **Gboard's**, not the
/// reference's, and are documented as such. What *is* from the reference is the
/// thing this enum has to be for it to be worth having: the key rows are read
/// from [`crate::graphics::layout::keyboard_rows`], so the variant a tap selects
/// and the characters the renderer paints are one table, not two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardLayout {
    #[default]
    Qwerty,
    Symbols,
    Numeric,
}

impl KeyboardLayout {
    /// The page the visible toggle key leads to.
    ///
    /// The single edge in the cycle: both non-alpha pages go back to the
    /// alphabet. A three-way cycle would be defensible, but it would mean the
    /// toggle key's label has to name a *page* while its behaviour names a
    /// *destination*, and the drawn label is the string
    /// [`VirtualKeyboard::handle_key_tap`] dispatches -- one function,
    /// [`crate::graphics::layout::keyboard_toggle_label`], for both.
    ///
    /// Gboard's own bottom-left key is `?123` from the alphabet, `ABC` from the
    /// symbol page, and `123` from a numeric-only field. The third of those is
    /// reached by the *field* asking for it (`handle_key_tap("123")`), not by
    /// the toggle, because a page reached by tapping needs a way back and a
    /// `123 -> Symbols` toggle has none worth having.
    #[inline]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Qwerty => Self::Symbols,
            Self::Symbols | Self::Numeric => Self::Qwerty,
        }
    }

    /// Whether letters should be drawn shifted.
    ///
    /// Only the alphabet page has letters, so a page change drops the shift with
    /// it. Without this, a user who typed `?123` while shift was on would come
    /// back to `QWERTY` and the first letter they typed would be upper case for
    /// a shift they cannot see.
    #[inline]
    pub const fn has_letters(self) -> bool {
        matches!(self, Self::Qwerty)
    }

    /// Whether the shift key is drawn at all.
    ///
    /// Separate from [`Self::has_letters`] because the third row is *where* shift
    /// lives, and on the symbol page that row is better spent on the punctuation
    /// that page exists for. Gboard keeps shift on every page; this drops it so
    /// the symbol rows can be as wide as the digit row, which is the difference
    /// between a symbols page you can aim at and one you cannot.
    #[inline]
    pub const fn shows_shift(self) -> bool {
        self.has_letters()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImeAction {
    None,
    /// Single-character commit: zero-allocation fast path for the common
    /// ASCII case.
    CommitChar(char),
    CommitString(String),
    DeleteSurroundingText {
        before_length: u32,
        after_length: u32,
    },
    SendKey(u32), // Linux evdev keycode e.g. KEY_ENTER (28)
    /// The user asked for the keyboard to go away.
    ///
    /// Added with the layout toggle. `Keyboard::hit` has always been able to
    /// return [`crate::graphics::layout::Key::Hide`] and the renderer has always
    /// painted a key for it, but the closest thing to an action was
    /// `ImeAction::None` -- which is indistinguishable from a key that was drawn,
    /// hit and did nothing. A shell that dispatched `None` for the Hide key
    /// swallowed the tap.
    HideKeyboard,
}

/// Gboard-Style On-Screen Virtual Keyboard
pub struct VirtualKeyboard {
    pub keyboard_height: f32, // e.g. 320.0 px
    pub layout: KeyboardLayout,
    pub is_shift_active: bool,
    pub is_caps_lock: bool,
    pub is_active: bool,
    /// 0.0 = Hidden, 1.0 = Fully visible. The canonical analytical
    /// [`SpringSimulation`]: this crate has exactly one spring model, and the
    /// IME uses it rather than a private integrator.
    pub slide_spring: SpringSimulation,
}

impl Default for VirtualKeyboard {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualKeyboard {
    pub fn new() -> Self {
        let keyboard_height = 320.0;
        Self {
            keyboard_height,
            layout: KeyboardLayout::Qwerty,
            is_shift_active: false,
            is_caps_lock: false,
            is_active: false,
            slide_spring: SpringSimulation::new(0.0, 0.0, SpringConfig::ime_slide()),
        }
    }

    /// Wayland text-input-v3 focus entered: activate IME
    pub fn activate(&mut self) {
        self.is_active = true;
        self.slide_spring.target = 1.0;
    }

    /// Wayland text-input-v3 focus left: deactivate IME
    pub fn deactivate(&mut self) {
        self.is_active = false;
        self.slide_spring.target = 0.0;
    }

    pub fn toggle(&mut self) {
        if self.is_active {
            self.deactivate();
        } else {
            self.activate();
        }
    }

    /// Calculates smooth viewport push translation for active app window
    /// so the text cursor remains visible above the keyboard
    pub fn window_viewport_push_y(&self) -> f32 {
        let progress = self.slide_spring.value.clamp(0.0, 1.0);
        progress * self.keyboard_height
    }

    /// Dispatch a key straight from [`crate::graphics::layout::Keyboard::hit`].
    ///
    /// **Allocation-free**, which is the point of it existing alongside
    /// [`Self::handle_key_tap`]: `Keyboard::hit` returns a [`Key`] carrying the
    /// character it was drawn with, so the shell can hand that straight over.
    /// Routing it through `handle_key_tap` instead means `key.to_string()` per
    /// tap, and a key tap is the hot path of a text field.
    ///
    /// `Key::Layout` is the one arm that makes the layout field reachable: it is
    /// the only key on the sheet that can produce a `KeyboardLayout` change, and
    /// before it existed the `?123` / `ABC` / `123` arms below had no caller
    /// that could reach them.
    pub fn handle_key(&mut self, key: crate::graphics::layout::Key) -> ImeAction {
        use crate::graphics::layout::Key;
        match key {
            Key::Shift => {
                self.is_shift_active = !self.is_shift_active;
                ImeAction::None
            }
            Key::Layout => {
                self.layout = self.layout.toggled();
                // Leaving a page drops the shift: on the symbol and numeric
                // pages there are no letters for it to apply to, and leaving it
                // on means the first letter typed after `ABC` is upper case for a
                // shift nobody can see.
                self.is_shift_active = false;
                ImeAction::None
            }
            Key::Backspace => ImeAction::DeleteSurroundingText {
                before_length: 1,
                after_length: 0,
            },
            Key::Enter => ImeAction::SendKey(28), // KEY_ENTER
            Key::Space => ImeAction::CommitChar(' '),
            Key::Hide => ImeAction::HideKeyboard,
            Key::Char(ch) => self.commit_char(ch),
        }
    }

    /// Key tap handler, by name.
    ///
    /// The three layout arms are matched against
    /// [`crate::graphics::layout::keyboard_toggle_label`] rather than against
    /// literals, so the string the renderer paints on the toggle key and the
    /// string this dispatches are the same value. They were literals here and a
    /// function in `layout.rs`, which is exactly the shape of bug where a drawn
    /// `?123` is handled as `ABC`.
    pub fn handle_key_tap(&mut self, key: &str) -> ImeAction {
        match key {
            "SHIFT" => {
                self.is_shift_active = !self.is_shift_active;
                ImeAction::None
            }
            // The toggle, matched by the label of every page it can be reached
            // from. `keyboard_toggle_label` is total over the three pages and
            // returns `"?123"` or `"ABC"`, so this arm is reachable and total.
            l if l == crate::graphics::layout::keyboard_toggle_label(KeyboardLayout::Qwerty)
                || l == crate::graphics::layout::keyboard_toggle_label(KeyboardLayout::Symbols)
                || l == crate::graphics::layout::keyboard_toggle_label(KeyboardLayout::Numeric) =>
            {
                self.layout = self.layout.toggled();
                self.is_shift_active = false;
                ImeAction::None
            }
            // The numeric page is reached by the *field*, not by the toggle: a
            // phone-number field has no letters to type, and a toggle that
            // arrived here would have nowhere useful to go back to.
            "123" => {
                self.layout = KeyboardLayout::Numeric;
                self.is_shift_active = false;
                ImeAction::None
            }
            "BACKSPACE" => ImeAction::DeleteSurroundingText {
                before_length: 1,
                after_length: 0,
            },
            "ENTER" => ImeAction::SendKey(28), // KEY_ENTER
            "SPACE" => ImeAction::CommitChar(' '),
            "HIDE" => ImeAction::HideKeyboard,
            _ => {
                // Single-char keys without double-scanning the string.
                let mut it = key.chars();
                match (it.next(), it.next()) {
                    (Some(ch), None) => self.commit_char(ch),
                    _ => ImeAction::None,
                }
            }
        }
    }

    /// Case-fold and commit one character.
    ///
    /// Split out of [`Self::handle_key_tap`] so [`Self::handle_key`] shares the
    /// folding rules instead of re-deriving them: two copies of "shift turns
    /// off after one character" is two places for the auto-revert to be dropped.
    fn commit_char(&mut self, ch: char) -> ImeAction {
        let shifted = self.is_shift_active || self.is_caps_lock;
        // ASCII fast path: no allocation. Non-ASCII case
        // folding is the rare path and keeps the String form.
        let out = if ch.is_ascii_alphabetic() {
            if shifted {
                ch.to_ascii_uppercase()
            } else {
                ch.to_ascii_lowercase()
            }
        } else if ch.is_ascii() {
            ch
        } else {
            let s = if shifted {
                ch.to_uppercase().to_string()
            } else {
                ch.to_lowercase().to_string()
            };
            if self.is_shift_active && !self.is_caps_lock {
                self.is_shift_active = false;
            }
            return ImeAction::CommitString(s);
        };

        // Single shift turns off after typing
        if self.is_shift_active && !self.is_caps_lock {
            self.is_shift_active = false;
        }

        ImeAction::CommitChar(out)
    }

    /// Set the page directly, for a field that only wants one of them.
    ///
    /// The shell's call on open: a telephone field hands over
    /// [`KeyboardLayout::Numeric`] and never has to simulate three taps to get
    /// there. Drops the shift for the same reason [`Self::handle_key`] does.
    #[inline]
    pub fn set_layout(&mut self, layout: KeyboardLayout) {
        self.layout = layout;
        self.is_shift_active = false;
    }

    pub fn update(&mut self, dt: f32) {
        // `step_clamped`, not `step`: the IME advances from whatever clock the
        // compositor is handed, and a non-finite delta must not be able to park
        // NaN in `slide_spring.value`, which the viewport push reads.
        self.slide_spring.step_clamped(dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_virtual_keyboard_animation_and_viewport_push() {
        let mut ime = VirtualKeyboard::new();
        assert_eq!(ime.window_viewport_push_y(), 0.0);

        ime.activate();
        for _ in 0..60 {
            ime.update(0.016);
        }

        assert!(ime.is_active);
        assert!((ime.window_viewport_push_y() - 320.0).abs() < 1.0);

        ime.deactivate();
        for _ in 0..60 {
            ime.update(0.016);
        }
        assert!(!ime.is_active);
        assert!(ime.window_viewport_push_y() < 1.0);
    }

    #[test]
    fn test_ime_key_actions_and_shift() {
        let mut ime = VirtualKeyboard::new();

        // Lowercase typing
        let act1 = ime.handle_key_tap("a");
        assert_eq!(act1, ImeAction::CommitChar('a'));

        // Shift then type
        ime.handle_key_tap("SHIFT");
        assert!(ime.is_shift_active);
        let act2 = ime.handle_key_tap("b");
        assert_eq!(act2, ImeAction::CommitChar('B'));
        assert!(!ime.is_shift_active); // Auto-reverted

        // Space and Backspace
        let act_space = ime.handle_key_tap("SPACE");
        assert_eq!(act_space, ImeAction::CommitChar(' '));

        let act_del = ime.handle_key_tap("BACKSPACE");
        assert_eq!(
            act_del,
            ImeAction::DeleteSurroundingText {
                before_length: 1,
                after_length: 0
            }
        );
    }

    use crate::graphics::layout::{keyboard_rows, keyboard_toggle_label, Key, Keyboard};

    /// Every page's toggle goes somewhere real, and the alphabet round-trips.
    ///
    /// `toggled` is deliberately **not** an involution and this pins the shape it
    /// actually has: `Qwerty <-> Symbols` is a two-cycle, and `Numeric` exits to
    /// `Qwerty` rather than continuing round. An involution would need
    /// `Numeric <-> ?`, and there is no fourth page to pair it with -- which is
    /// also why `Numeric` is entered by a *field* ([`VirtualKeyboard::set_layout`])
    /// rather than by the toggle.
    ///
    /// The property that actually matters, and the one a user can feel, is that
    /// no page toggles to itself: a toggle that paints a key and selects the page
    /// it is already showing is a key that visibly works and changes nothing.
    #[test]
    fn the_layout_toggle_is_an_edge_and_always_lands_somewhere_real() {
        use KeyboardLayout as L;
        for from in [L::Qwerty, L::Symbols, L::Numeric] {
            assert_ne!(from.toggled(), from, "{from:?} toggled to itself");
        }
        assert_eq!(L::Qwerty.toggled(), L::Symbols);
        assert_eq!(
            L::Symbols.toggled(),
            L::Qwerty,
            "the alphabet did not come back"
        );
        assert_eq!(L::Numeric.toggled(), L::Qwerty);
        // The toggle never *reaches* the numeric page. That is the design and it is
        // asserted rather than left implicit: a numeric page that a key could
        // reach would need a fourth destination to come back from, and there is
        // not one. So `Numeric` is a field-driven page
        // ([`VirtualKeyboard::set_layout`]) and the visible toggle is not the way
        // in -- which is why the toggle's label on that page reads `ABC`.
        for from in [L::Qwerty, L::Symbols] {
            assert_ne!(
                from.toggled(),
                L::Numeric,
                "{from:?} toggles into the numeric page"
            );
        }
        assert_ne!(L::Numeric.toggled(), L::Numeric);
        // And the alphabet pair really is a two-cycle, so a user who keeps
        // tapping lands back where they started and cannot get stranded.
        assert_eq!(L::Qwerty.toggled().toggled(), L::Qwerty);
        assert_eq!(L::Symbols.toggled().toggled(), L::Symbols);
    }

    /// Every page's toggle label is a string `handle_key_tap` acts on, and acting
    /// on it moves off the page it was painted on.
    ///
    /// This is the drift guard. The renderer paints
    /// `keyboard_toggle_label(layout)` and the shell feeds that string back, so
    /// the two must agree; before the refactor `handle_key_tap` matched literals
    /// and the renderer had a different set, and nothing would have said so.
    ///
    /// Painted *off* the page it was painted on: a toggle that selected the page
    /// it is already showing would be a key that visibly works and changes
    /// nothing, which is the project's stated worst outcome.
    #[test]
    fn every_painted_toggle_label_dispatches_to_a_different_page() {
        use KeyboardLayout as L;
        for from in [L::Qwerty, L::Symbols, L::Numeric] {
            let label = keyboard_toggle_label(from);
            let mut ime = VirtualKeyboard::new();
            ime.set_layout(from);
            assert_eq!(ime.layout, from, "set_layout did not take");
            assert_eq!(ime.handle_key_tap(label), ImeAction::None);
            assert_ne!(
                ime.layout, from,
                "the {from:?} toggle label {label:?} did nothing"
            );
        }
    }

    /// `handle_key` is the allocation-free twin of `handle_key_tap`, and it agrees
    /// with it.
    ///
    /// `handle_key` exists so the shell can pass a `Key::Char` straight from
    /// `Keyboard::hit` instead of formatting it into a `String` per tap. That
    /// is only safe if it computes the same `ImeAction`, so every arm is compared
    /// against its string twin rather than only spot-checked.
    #[test]
    fn handle_key_matches_handle_key_tap_on_every_arm() {
        use KeyboardLayout as L;
        for layout in [L::Qwerty, L::Symbols, L::Numeric] {
            let rows = keyboard_rows(layout);
            // A character key from each row of the page.
            let mut chars = vec![
                Key::Char(crate::graphics::layout::KB_DIGITS[4]),
                Key::Char(rows.row2[1]),
                Key::Char(rows.row3[0]),
            ];
            if layout.has_letters() {
                chars.push(Key::Char('q'));
            }
            for k in chars {
                let mut a = VirtualKeyboard::new();
                a.set_layout(layout);
                let mut b = VirtualKeyboard::new();
                b.set_layout(layout);
                let via_key = a.handle_key(k);
                let via_str = b.handle_key_tap(&k_char(k));
                assert_eq!(via_key, via_str, "{layout:?} {k:?}");
                assert_eq!(a.layout, b.layout, "{layout:?} {k:?}: the pages diverged");
                assert_eq!(a.is_shift_active, b.is_shift_active, "{layout:?} {k:?}");
            }
            for (k, name) in [
                (Key::Backspace, "BACKSPACE"),
                (Key::Enter, "ENTER"),
                (Key::Space, "SPACE"),
                (Key::Hide, "HIDE"),
            ] {
                let mut a = VirtualKeyboard::new();
                a.set_layout(layout);
                let mut b = VirtualKeyboard::new();
                b.set_layout(layout);
                assert_eq!(a.handle_key(k), b.handle_key_tap(name), "{layout:?} {name}");
            }
        }
    }

    /// The toggle key moves the page, and the *painted* label is what says so.
    ///
    /// Driven from the geometry, not from a string constant: the touch lands on
    /// `Keyboard::row4_layout` and the resulting `ime.layout` is the page whose
    /// table the renderer will read next frame. This is the end-to-end path the
    /// brief asks for -- a tap on the drawn key changes what is drawn -- minus the
    /// pixels, which `screenshot.rs` asserts separately.
    #[test]
    fn a_touch_on_the_drawn_toggle_key_changes_the_page() {
        use KeyboardLayout as L;
        for from in [L::Qwerty, L::Symbols, L::Numeric] {
            let kb = Keyboard::new_for(1080.0, 2400.0, from);
            let key = kb
                .hit(kb.row4_layout.center_x(), kb.row4_layout.center_y())
                .expect("the painted toggle key is not hit-testable");
            assert_eq!(key, Key::Layout, "{from:?}");
            let mut ime = VirtualKeyboard::new();
            ime.set_layout(from);
            assert_eq!(ime.handle_key(key), ImeAction::None);
            assert_eq!(ime.layout, from.toggled());
            // And the next page's row 2 is not this page's.
            assert_ne!(
                keyboard_rows(ime.layout).row2,
                keyboard_rows(from).row2,
                "{from:?}: the page changed but its row 2 did not"
            );
        }
    }

    /// Leaving the alphabet drops a shift nobody can see.
    ///
    /// On the symbol and numeric pages there are no letters for shift to apply
    /// to, and it is not drawn -- so keeping it would mean the first letter typed
    /// after `ABC` comes out upper case.
    #[test]
    fn a_page_change_drops_an_invisible_shift() {
        use KeyboardLayout as L;
        let mut ime = VirtualKeyboard::new();
        ime.handle_key_tap("SHIFT");
        assert!(ime.is_shift_active);
        ime.handle_key(Key::Layout);
        assert_eq!(ime.layout, L::Symbols);
        assert!(
            !ime.is_shift_active,
            "shift survived a page change and will apply to the first letter typed back"
        );

        // A numeric field, likewise.
        let mut ime = VirtualKeyboard::new();
        ime.handle_key_tap("SHIFT");
        ime.set_layout(L::Numeric);
        assert!(!ime.is_shift_active);
    }

    /// Caps lock survives a page change; single shift does not.
    ///
    /// They are one field apart in the struct and behave identically in the
    /// commit path (`shifted = is_shift_active || is_caps_lock`), so a change
    /// that resets one and not the other is easy to write and hard to notice.
    #[test]
    fn caps_lock_survives_a_page_change_and_single_shift_does_not() {
        let mut ime = VirtualKeyboard::new();
        ime.is_caps_lock = true;
        ime.is_shift_active = true;
        ime.handle_key(Key::Layout);
        assert!(ime.is_caps_lock, "caps lock was dropped by a page change");
        assert!(!ime.is_shift_active);
        assert_eq!(
            ime.handle_key(Key::Char('a')),
            ImeAction::CommitChar('A'),
            "caps lock did not survive into the commit path either"
        );
    }

    /// Only the alphabet page has letters, and only it draws shift.
    ///
    /// Both are load-bearing for the *render*: a shift key drawn on a page whose
    /// characters are all punctuation is a key that visibly does nothing, and
    /// `Keyboard::shows_shift` is the single answer the renderer and the shell
    /// must agree on.
    #[test]
    fn only_the_alphabet_page_has_letters_or_a_shift_key() {
        use KeyboardLayout as L;
        assert!(L::Qwerty.has_letters());
        assert!(L::Qwerty.shows_shift());
        for l in [L::Symbols, L::Numeric] {
            assert!(!l.has_letters(), "{l:?} claims letters");
            assert!(!l.shows_shift(), "{l:?} draws a shift key");
            let rows = keyboard_rows(l);
            assert!(
                !rows
                    .row2
                    .iter()
                    .chain(rows.row3)
                    .any(|c| c.is_ascii_alphabetic()),
                "{l:?} still has letters on it"
            );
        }
        // And the alphabet page really does.
        let rows = keyboard_rows(L::Qwerty);
        assert!(rows.row2.iter().any(|c| c.is_ascii_alphabetic()));
    }

    /// The Hide key has an action of its own.
    ///
    /// It used to fold into `ImeAction::None`, which is indistinguishable from a
    /// key that was tapped and did nothing -- so the one control on the sheet a
    /// user presses to get the sheet out of the way was silently swallowed.
    #[test]
    fn the_hide_key_is_not_indistinguishable_from_no_action() {
        let mut ime = VirtualKeyboard::new();
        assert_eq!(ime.handle_key(Key::Hide), ImeAction::HideKeyboard);
        assert_ne!(
            ime.handle_key(Key::Hide),
            ImeAction::None,
            "Hide still returns the same value as a key that does nothing"
        );
        assert_eq!(ime.handle_key_tap("HIDE"), ImeAction::HideKeyboard);
        assert_eq!(ime.handle_key(Key::Shift), ImeAction::None);
    }

    /// The keyboard's resting page is the alphabet, and the enum says so.
    ///
    /// `Default` on the variant, not just on [`VirtualKeyboard`]: the state field
    /// is a `DrmInteractiveState::keyboard_layout` that has to agree with this,
    /// and one `Default` is the only way to keep them in step.
    #[test]
    fn the_resting_keyboard_is_the_alphabet() {
        assert_eq!(KeyboardLayout::default(), KeyboardLayout::Qwerty);
        assert_eq!(VirtualKeyboard::new().layout, KeyboardLayout::default());
    }

    fn k_char(k: Key) -> String {
        match k {
            Key::Char(c) => c.to_string(),
            other => panic!("not a character key: {other:?}"),
        }
    }
}
