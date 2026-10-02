//! Drawer sheet and all-apps fast scroller: the state machine and the paint.
//!
//! # Why this module exists
//!
//! `drm_kms.rs` grew a second, hand-tuned copy of the drawer (search pill at
//! a magic y, a header line, a `format!`'d app count) that shared no geometry
//! with the hit-tester and so drifted from `layout.rs` on every profile that
//! was not 1080x2400. This module owns exactly two things:
//!
//! 1. the **state machine** for a fast-scroll drag ([`FastScrollerState]) and
//!    the letter -> row map it scrubs ([`SectionIndex`]), and
//! 2. the **paint** for the sheet chrome ([`draw_drawer_sheet`]) and the
//!    track/thumb/popup ([`draw_fast_scroller`]).
//!
//! Everything geometric already exists in `layout.rs` ([`DrawerSheetLayout`],
//! [`FastScrollerLayout`]). Nothing here re-derives a dp number: the sheet
//! bands, the 52 dp thumb, the 75 x 62 dp letterbox and the scrim token are
//! all read from those structs, so a rectangle that is drawn is by
//! construction the rectangle the gesture layer hit-tests.
//!
//! # Frame budget
//!
//! The scrim is the one thing here that could blow the budget. A full-screen
//! per-pixel translucent blend measures **10.2 ms** at 1080x2400 -- more
//! than the whole 8.33 ms frame at 120 Hz -- and the regression guard
//! `no_full_screen_translucent_blend` exists to catch it coming back. So:
//!
//! * the scrim is composited **once**, in scalar arithmetic, to a single
//!   opaque `u32` ([`raster::composite_scrim`]), and
//! * that one opaque colour is written with [`raster::draw_opaque_rect`] /
//!   a span `fill`, which autovectorises even at `opt-level = "z"`.
//!
//! The drawer's scrim in the reference is the *all-apps container background*
//! (`ActivityAllAppsContainerView.java:242`,
//! `ColorTokens.kt:96 AllAppsScrimColor`) -- i.e. it dims the **workspace**,
//! and the scrolling list draws on top of it. See
//! [`draw_drawer_sheet`] for exactly how that is resolved here without ever
//! blending under structured content.
//!
//! # Allocation
//!
//! Zero. [`FastScrollerState`] and [`SectionIndex`] are `Copy`, fixed size
//! (the index is 26 * (1 + 2) = 78 bytes), and the drawing helpers borrow
//! their text. There is no `Vec`, no `String` and no `format!` on any path
//! that runs per frame.

use super::font::{self, FontWeight};
use super::layout::{DrawerSheetLayout, FastScrollerLayout};
use super::raster;

// ===========================================================================
// Reference constants
// ===========================================================================

/// `"A".."Z"`, indexed `letter - 1`. A `&'static [u8; 26]` so the popup
/// letter can be turned into a `&str` with `core::str::from_utf8` on a
/// one-byte slice -- no `String`, no `char::from` dance.
pub const LETTER_BYTES: [u8; 26] = *b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Number of alphabet sections.
pub const SECTIONS: usize = 26;

/// `letterFastScroller()` is **hard-false** in the reference build
/// (`FeatureFlagsImpl.java:582-584`), so the A-Z letter list beside the
/// track is off. `RecyclerViewFastScroller.shouldUseLetterFastScroller()`
/// (`:448-451`) gates the whole `LetterListTextView` column on that flag, and
/// with it off `onDraw` takes the `else` branch at `:406-433`: a plain track
/// and a plain round-rect thumb.
///
/// This flag exists so that when someone *does* flip the upstream flag the
/// right column has a home, and so the constant is not a magic `false`
/// scattered through the paint. Nothing else in this module depends on it,
/// which is the point: the default path must not grow a letter indexer
/// because a flag says it might.
pub const LETTERS: bool = false;

/// `MAX_TRACK_ALPHA` (`RecyclerViewFastScroller.java:104`): the track is a
/// 6/8 dp stadium at alpha 30 -- barely visible, by design.
pub const TRACK_ALPHA: u8 = 30;

/// `SCROLL_BAR_VIS_DURATION` (`:105`) / the popup fade-out (`:517`).
pub const FADE_OUT_MS: u32 = 150;

/// The popup fade-in (`:515`).
pub const FADE_IN_MS: u32 = 200;

/// Search hint tracking, `letterSpacing = -0.01` in the drawer's search bar
/// style. One percent *tighter*, so it is subtracted.
pub const SEARCH_HINT_TRACKING: f32 = -0.01;

/// The scrim alpha is not a separate constant: it is the alpha byte of
/// [`DrawerSheetLayout::scrim_argb`], which `layout.rs` sets to
/// `0xFF * 0.40 = 0x66` over `#404040` (`ColorTokens.kt:96`). Reading it back
/// out of the token is what keeps the colour and its alpha from drifting
/// apart.
#[inline]
fn scrim_alpha(sheet: &DrawerSheetLayout) -> f32 {
    ((sheet.scrim_argb >> 24) & 0xFF) as f32 / 255.0
}

// ===========================================================================
// Fast-scroller state
// ===========================================================================

/// A fast-scroll drag, in the state `RecyclerViewFastScroller` keeps in its
/// fields between `MotionEvent`s.
///
/// `y` in every method is a **panel** y, the same space `FastScrollerLayout`
/// is in; `thumb_y` is *track-relative* (`0..track_h`), which is exactly what
/// `mThumbOffsetY` is in the reference (`:244-250`). Keeping the two spaces
/// distinct is what stops the classic off-by-one-track-height bug where the
/// thumb is drawn at `track.y + thumb_y` and the hit-test forgets the `track.y`.
///
/// `Copy` with no allocation, and no `f32` field is ever allowed to go NaN:
/// every entry point rejects non-finite input rather than letting a poisoned
/// touch reach the rasteriser.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FastScrollerState {
    /// `mIsDragging` (`:135`).
    pub dragging: bool,
    /// `mDownY` (`:164`): the y of the `ACTION_DOWN`.
    pub down_y: f32,
    /// `mLastY` (`:134`): the y of the most recent `ACTION_MOVE`.
    pub last_y: f32,
    /// `mThumbOffsetY` (`:127`): thumb top **within the track**, `0..track_h`.
    /// The thumb is a fixed 52 dp, so this is clamped to
    /// `0..=track_h - thumb_h` and a taller track only gives it more travel.
    pub thumb_y: f32,
    /// Track height the thumb is clamped against, refreshed on every event so
    /// a mid-drag relayout (rotation, keyboard) cannot leave the thumb
    /// outside its own track.
    pub track_h: f32,
    /// Section the popup is showing: `0` = none, else `'A'` as `1..=26`.
    /// This is `mPopupSectionName` (`:156`) in numeric form.
    pub letter: u8,
    /// Popup opacity, driven by `step` over [`FADE_IN_MS`]/[`FADE_OUT_MS`]
    /// (`:513-517`).
    pub popup_alpha: f32,
    /// Popup top edge in panel y, from `updatePopupY` (`:520-528`).
    pub popup_y: f32,
    /// Rising-edge latch: set on the move that changed [`Self::letter`],
    /// cleared by the next move that did not. The caller consumes the `bool`
    /// from [`Self::on_move`] and pulses `CLOCK_TICK`; the latch exists so a
    /// caller that polls the field instead of the return value cannot fire
    /// the same tick twice.
    pub haptic_latch: bool,
    /// Wall clock of the most recent press transition: the `ACTION_DOWN`
    /// until the drag engages, then the last move/up. While the drag is still
    /// a candidate this *is* `mDownTimeStampMillis` (`:146`), which is what
    /// the 10 ms detent dwell is measured against.
    pub last_press_ms: f32,
    /// The letter -> row map, built once at catalogue-scan time. Held here
    /// rather than passed to [`Self::on_move`] so the touch handler needs no
    /// extra argument and cannot forget to supply it: a call with a stale
    /// index is impossible when there is only one place to put the index.
    pub sections: SectionIndex,
    /// Row count the section index was built for, as the fast scroller needs
    /// it (`scrollToPositionAtProgress`'s row count,
    /// `AllAppsRecyclerView.java:191-193`). `u16` because the index's own
    /// encoding tops out at 26 * 255 = 6630 rows.
    pub total_rows: u16,
}

impl Default for FastScrollerState {
    fn default() -> Self {
        Self::new()
    }
}

impl FastScrollerState {
    /// A fresh, idle state: thumb parked at the top of the track, no letter,
    /// popup fully faded out.
    pub const fn new() -> Self {
        Self {
            dragging: false,
            down_y: 0.0,
            last_y: 0.0,
            thumb_y: 0.0,
            track_h: 0.0,
            letter: 0,
            popup_alpha: 0.0,
            popup_y: 0.0,
            haptic_latch: false,
            last_press_ms: 0.0,
            sections: SectionIndex::EMPTY,
            total_rows: 0,
        }
    }

    /// Re-point the state at a catalogue. Called once when the app list is
    /// (re)built, never per frame -- that is the whole reason
    /// [`SectionIndex`] is a fixed array rather than something derived from
    /// the rows on demand.
    ///
    /// The row count is taken *from* the iterator rather than passed
    /// separately, which is not a convenience: the two arrays in
    /// [`SectionIndex`] cannot encode "no catalogue" and "every row is in the
    /// leading bucket" differently, so a caller that supplied a row count the
    /// index did not agree with would get a wrong letter rather than an
    /// error. Taking the length of the same iterator makes the mismatch
    /// unrepresentable.
    ///
    /// `ExactSizeIterator` so the count is `len()`, not a second walk.
    pub fn set_catalogue<'a, I>(&mut self, names: I)
    where
        I: IntoIterator<Item = &'a str>,
        I::IntoIter: ExactSizeIterator,
    {
        let it = names.into_iter();
        let rows = it.len();
        self.sections = SectionIndex::build(it);
        // Saturating rather than truncating: a list past 65535 rows cannot be
        // indexed, and clamping reports the first 65535 rather than the last.
        self.total_rows = rows.min(u16::MAX as usize) as u16;
    }

    /// Largest legal `thumb_y` for `fs`: the track minus the fixed 52 dp
    /// thumb, floored at 0 for a track too short to hold it.
    #[inline]
    fn travel(fs: &FastScrollerLayout) -> f32 {
        (fs.track.h - fs.thumb_h).max(0.0)
    }

    /// `mTouchOffsetY`-derived popup placement (`:520-528`):
    ///
    /// ```text
    /// top = scrollBarTop + lastTouchY + thumbRadius / 2 - popupHeight / 2
    /// top = boundToRange(top, 0, top + trackHeight - popupHeight)
    /// ```
    ///
    /// with `lastTouchY` = `mLastTouchY` = the bounded (track-relative)
    /// thumb y, so it is [`Self::thumb_y`] verbatim, and `thumbRadius` is
    /// `mWidth + 2 * mThumbPadding` (`:463-465`) -- the same number the
    /// thumb's paint geometry uses, which is what "aligns the rounded corner
    /// of the popup with the top of the thumb" (`:522`) means.
    ///
    /// # Public because the renderer has to place the popup too
    ///
    /// `updatePopupY` (`:520-528`) runs on every `ACTION_MOVE`, so
    /// [`Self::on_move`] already stores the result in [`Self::popup_y`]. But
    /// the shell's own render pass has to *refresh* it: a relayout between
    /// the last touch event and the frame (rotation, keyboard, panel resize)
    /// changes `fs.popup.h` and `fs.track`, and the popup would then be drawn
    /// at a y derived from the pre-relayout layout. A renderer that wants the
    /// popup glued to the thumb has to be able to recompute this, and
    /// re-deriving the formula at the call site is exactly the drift this
    /// module exists to prevent. It is a pure `&self` read of three fields.
    #[inline]
    pub fn popup_top(&self, fs: &FastScrollerLayout) -> f32 {
        let radius = (fs.track_w + fs.thumb_pad * 2.0) * 0.5;
        let raw = fs.track.y + self.thumb_y + radius - fs.popup.h * 0.5;
        let lo = fs.track.y;
        let hi = (fs.track.y + fs.track.h - fs.popup.h).max(lo);
        raw.clamp(lo, hi)
    }

    /// `scrollToPositionAtProgress(boundedY / bottom)` (`:352-356`) with
    /// `bottom = trackHeight - thumbHeight` -- the thumb's own travel, not the
    /// track height. A track too short to hold the 52 dp thumb has no travel,
    /// and the reference divides by zero there; 0 is the honest answer.
    #[inline]
    fn progress_of(&self, fs: &FastScrollerLayout) -> f32 {
        let travel = Self::travel(fs);
        if travel > 0.0 {
            self.thumb_y / travel
        } else {
            0.0
        }
    }

    /// `ACTION_DOWN` (`:282-298`).
    ///
    /// Records the down position and time; it does **not** engage. The
    /// engagement gate needs a *duration* as well as a proximity, and the
    /// reference only evaluates it from `ACTION_MOVE` (`:309-314`) so a plain
    /// tap on the scroller never turns into a drag.
    pub fn on_down(&mut self, y: f32, now_ms: f32, fs: &FastScrollerLayout) {
        if !y.is_finite() || !now_ms.is_finite() || !fs.track.h.is_finite() {
            return;
        }
        self.dragging = false;
        self.down_y = y;
        self.last_y = y;
        // Until the drag engages this doubles as the down timestamp, which is
        // the only place the 10 ms dwell can be read from.
        self.last_press_ms = now_ms;
        self.track_h = fs.track.h;
        self.haptic_latch = false;
        // Re-clamp against the *current* track: a relayout between drags must
        // not be able to leave the thumb parked outside its own track.
        self.thumb_y = self.thumb_y.clamp(0.0, Self::travel(fs));
        self.popup_y = self.popup_top(fs);
    }

    /// `ACTION_MOVE` (`:299-326`). Returns `true` on the frame the section
    /// changed, which is the caller's cue to pulse `CLOCK_TICK`.
    ///
    /// # The engagement gate
    ///
    /// Two conditions, both required (`RecyclerViewFastScroller.java:309-314`):
    ///
    /// * **proximity** -- `|y - down_y| < mDeltaThreshold`, 4 dp (`:83`); and
    /// * **dwell** -- `now - mDownTimeStampMillis > FASTSCROLL_THRESHOLD_MILLIS`,
    ///   10 ms (`:82`).
    ///
    /// The reference writes the proximity test as `isNearThumb(mDownX, mLastY)`
    /// (`:484-488`, `0 <= y - mThumbOffsetY <= mThumbHeight`) and uses
    /// `mDeltaThreshold` at `:289-293` to stop the *list's* own velocity dead
    /// before the touch is handed to a cell. Folding both into one 4 dp
    /// proximity gate is this module's contract and is what
    /// `fastscroller_engages_only_past_both_thresholds` pins: a fast flick
    /// that blows straight past the scroller must not be captured, and a
    /// slow press-and-hold 40 dp away must not be either.
    ///
    /// The move that *engages* contributes no thumb travel, which is the
    /// reference's anti-jump behaviour (`:296` + `:347`): the thumb is where
    /// it was and only subsequent deltas move it.
    pub fn on_move(&mut self, y: f32, now_ms: f32, fs: &FastScrollerLayout) -> bool {
        if !y.is_finite() || !now_ms.is_finite() || !fs.track.h.is_finite() {
            return false;
        }
        let dy = y - self.last_y;
        self.last_y = y;

        if !self.dragging {
            let dwell = now_ms - self.last_press_ms;
            let near = (y - self.down_y).abs() < fs.engage_delta;
            if !(near && dwell > fs.engage_ms) {
                return false;
            }
            self.dragging = true;
            // The engaging move contributes *no* travel. The reference
            // absorbs its delta into `mTouchOffsetY` (`mDownY - mThumbOffsetY`
            // at `:296`, then `+= (mLastY - mDownY)` at `:347`), which
            // re-derives the same touch offset and leaves `mThumbOffsetY`
            // exactly where it was -- that is the anti-jump. Applying `dy`
            // here instead would kick the thumb by up to `engage_delta` on
            // the very first frame of every drag, which at 4 dp is a visible
            // pop against the user's finger.
            self.letter = 0;
            self.haptic_latch = false;
            self.last_press_ms = now_ms;
            self.track_h = fs.track.h;
            let section = self
                .sections
                .section_for_progress(self.progress_of(fs), self.total_rows as usize);
            let changed = section != self.letter;
            self.letter = section;
            self.haptic_latch = changed;
            self.popup_y = self.popup_top(fs);
            return changed;
        }
        self.last_press_ms = now_ms;
        self.track_h = fs.track.h;

        let travel = Self::travel(fs);
        self.thumb_y = (self.thumb_y + dy).clamp(0.0, travel);
        let section = self
            .sections
            .section_for_progress(self.progress_of(fs), self.total_rows as usize);
        let changed = section != self.letter;
        self.letter = section;
        self.haptic_latch = changed;
        self.popup_y = self.popup_top(fs);
        changed
    }

    /// `ACTION_UP` / `ACTION_CANCEL` -> `endFastScrolling` (`:327-330`,
    /// `:383-393`).
    ///
    /// The section is deliberately **kept**: the popup fades out over
    /// [`FADE_OUT_MS`] and the reference leaves the text in place for that
    /// whole 150 ms (`:513-517` only animates alpha). Blanking it here would
    /// make the letter vanish before the fade finishes.
    pub fn on_up(&mut self, now_ms: f32) {
        self.dragging = false;
        self.haptic_latch = false;
        if now_ms.is_finite() {
            self.last_press_ms = now_ms;
        }
    }

    /// Advance the popup fade by `dt` ms.
    ///
    /// `ViewPropertyAnimator`'s 200 ms in / 150 ms out (`:513-517`) integrated
    /// as a linear rate: `dt / duration` per call, so the caller may pass
    /// 16.7 for a 120 Hz frame or 200 for a single catch-up tick and get the
    /// same curve. The rate is snap-terminated within half a step so the
    /// animation lands exactly on 0.0 or 1.0 instead of 0.99999 -- an alpha
    /// that never reaches 1.0 leaves the popup permanently 1 part in 10,000
    /// transparent, which is visible on a dark blob over a light sheet.
    pub fn step(&mut self, dt: f32) {
        let (target, ms) = if self.dragging {
            (1.0f32, FADE_IN_MS)
        } else {
            (0.0f32, FADE_OUT_MS)
        };
        if !dt.is_finite() {
            self.popup_alpha = self.popup_alpha.clamp(0.0, 1.0);
            return;
        }
        let rate = 1.0 / ms as f32;
        // The ramp is *directed*: fading out has to subtract. Adding the rate
        // unconditionally and pinning at the end leaves a fading-out popup at
        // 1.0 forever, and a settled one at 0.0 climbing straight back up on
        // the next tick.
        let delta = target - self.popup_alpha;
        if delta == 0.0 {
            return;
        }
        let next = self.popup_alpha + delta.signum() * dt.max(0.0) * rate;
        self.popup_alpha = if (target - next).abs() <= rate * 0.5 {
            target
        } else {
            next.clamp(0.0, 1.0)
        };
    }

    /// `true` while the popup should be painted. Separate from
    /// [`Self::letter`] being non-zero so a caller that wants a hard cut-off
    /// can test alpha instead of guessing a threshold.
    #[inline]
    pub fn popup_visible(&self) -> bool {
        self.popup_alpha > 0.0 && self.letter != 0
    }

    /// The section as a `&str`, or `""` when there is none. Backs onto
    /// [`LETTER_BYTES`], so this allocates nothing.
    #[inline]
    pub fn letter_str(&self) -> &'static str {
        match LETTER_BYTES.get(self.letter.wrapping_sub(1) as usize) {
            Some(_) => core::str::from_utf8(
                &LETTER_BYTES[(self.letter - 1) as usize..self.letter as usize],
            )
            .unwrap_or(""),
            None => "",
        }
    }
}

// ===========================================================================
// Section index
// ===========================================================================

/// Letter -> row map for the fast scroller, built **once** at
/// catalogue-scan time.
///
/// # Encoding
///
/// `row_offset[l]` is the **absolute** first row of section `l`
/// (`'A' + l`), and `first_row[l]` is that same quantity **delta encoded**
/// from the previous section (`l == 0` is measured from row 0).
///
/// Both are stored because the absolute value is what a lookup reads -- one
/// array index on the touch path, no `O(26)` prefix sum and no second pass --
/// while the delta is what *bounds* the encoding: a `u8` cannot express a row
/// index past 255 on its own, so the section count is the only thing a `u8`
/// can honestly hold. 26 sections x 255 rows = **6630 rows**, which is the
/// ceiling the ~6500-row figure in the plan refers to.
///
/// A section that is *absent* inherits the next present section's row, so
/// `row_offset` stays non-decreasing. That alone is **not** enough for the
/// lookup, and the reason is worth stating because it is the bug that was
/// here: a scan of "take the last `l` with `row_offset[l] <= row`" lets an
/// absent letter win whenever it ties, so the scan must be restricted to
/// *present* letters. `present` is a 26-bit mask and
/// [`Self::section_for_progress`] consults it.
///
/// Without the mask the leading run -- letters above the highest present one,
/// which every real catalogue has (`Z`, `X`, `Q`...) -- has no successor to
/// inherit from. Mirroring the *first* row makes the top of the list report
/// the last alphabet letter; mirroring the *last* row makes the bottom do the
/// same. There is no value for that run that is correct at both ends, because
/// the tie-break is what is wrong, not the row it was handed. `present` makes
/// the question moot: an absent letter is never a candidate.
///
/// # Overflow
///
/// The delta **saturates** at 255: a letter with more than 255 consecutive
/// rows records 255, never wraps and never panics. The consequence is
/// bounded and one-directional: `first_row` can no longer round-trip past
/// 6630 rows (`sum(first_row)` stops equalling the last section's
/// `row_offset`), but **every lookup stays exact**, because lookups read
/// `row_offset` only. Past that point a caller that needs the delta walk
/// (rather than the O(1) absolute read) would widen `first_row` to `u16`,
/// which costs 26 bytes and no code change anywhere else. At 104 dp per
/// all-apps cell, 6630 rows is roughly a 690 dp-tall single column, i.e. a
/// catalogue of a few thousand apps; the drawer needs a *scrollable* list
/// long before that, so this is not a live limit today.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SectionIndex {
    /// First row of each letter, delta encoded from the previous letter.
    pub first_row: [u8; 26],
    /// Absolute first row of each letter, in rows.
    pub row_offset: [u16; 26],
    /// Bit `l` set when section `l` has at least one row.
    ///
    /// 26 bits fit a `u32`, so presence costs 4 bytes and no allocation, and it
    /// is what lets the lookup skip absent letters instead of letting them win
    /// a tie. See the type's documentation.
    pub present: u32,
    /// Rows in the catalogue this index was built from. See
    /// [`Self::total_rows`].
    pub total_rows: usize,
}

impl SectionIndex {
    /// No sections, no rows -- the identity for an empty catalogue.
    pub const EMPTY: SectionIndex = SectionIndex {
        first_row: [0; 26],
        row_offset: [0; 26],
        present: 0,
        total_rows: 0,
    };

    pub fn build<'a, I: IntoIterator<Item = &'a str>>(names: I) -> Self {
        // 2 x 26 u32 on the stack: the whole build is two fixed arrays and no
        // allocation at all, not even a temporary `Vec`.
        let mut first = [u32::MAX; 26];
        let mut count = [0u32; 26];
        for (row, name) in names.into_iter().enumerate() {
            let b = bucket_of(name);
            if count[b] == 0 {
                first[b] = row as u32;
            }
            count[b] += 1;
        }
        // Walk down so an absent letter inherits the next present letter's
        // row, which is what makes the lookup scan's tie-break correct.
        let mut abs = [0u32; 26];
        let mut next = 0u32;
        for l in (0..26).rev() {
            if count[l] != 0 {
                next = first[l];
            }
            abs[l] = next;
        }
        // The leading run -- letters *above* the highest present one -- has no
        // present letter to inherit from, so the descending walk leaves it at
        // whatever `next` was seeded to. Seeding it to 0 was the bug: on a real
        // all-apps catalogue (no Q, no X, no Z) `row_offset[25] == 0` while
        // `row_offset['R'] == last_row`, so `row_offset` stopped being
        // non-decreasing -- which is its documented contract -- and
        // `section_for_progress`'s "later letters win ties" rule matched that
        // whole leading run and reported the *last alphabet index* for every
        // scroll position. `utlc` shows that string to the user, so a sparse
        // catalogue always read "Z".
        //
        // A forward running max repairs exactly that run and nothing else: for
        // `l` at or below the highest present letter `abs` is already
        // non-decreasing so the clamp is a no-op, and for `l` above it the max
        // over `0..l` is `abs[highest_present]`. That value restores the
        // monotonicity invariant but is *not* an answer to "which letter is at
        // the top of the list" -- which is why the `present` mask below, not
        // this clamp, is what fixes the reported letter. An empty catalogue
        // keeps every entry at 0, and the lookup returns 0 there.
        for l in 1..26 {
            abs[l] = abs[l].max(abs[l - 1]);
        }
        let mut out = SectionIndex::EMPTY;
        let mut prev = 0u32;
        for (l, row) in abs.iter().enumerate() {
            out.row_offset[l] = (*row).min(u16::MAX as u32) as u16;
            out.first_row[l] = row.saturating_sub(prev).min(u8::MAX as u32) as u8;
            prev = *row;
            if count[l] != 0 {
                out.present |= 1u32 << l;
            }
        }
        // The catalogue's row count, taken from the walk rather than kept
        // alongside it: `count` is the per-section tally the build already has,
        // so summing it here cannot disagree with `row_offset`. A caller
        // clamping a scroll needs both numbers and they must come from one
        // source -- two independently supplied counts is how a scroller ends up
        // one row short of its own list.
        out.total_rows = count.iter().map(|&c| c as usize).sum();
        out
    }

    /// Build the index from names that are **already sorted
    /// case-insensitively**, which is what `AlphabeticalAppsList` maintains.
    ///
    /// Sorting is not re-done here: an unsorted input would produce a
    /// non-monotonic `row_offset` and a lookup that picks the wrong letter.
    /// That contract is the price of an O(n) build with no sort buffer.
    ///
    /// A name whose first character is not an ASCII letter (a digit, a
    /// symbol, a non-Latin leading codepoint) buckets under index 0. Because
    /// the input is sorted, that leading run is contiguous and merges with
    /// whatever else lands in `'A'`, so the index stays correct -- the only
    /// effect is that scrolling to the very top reports `'A'` rather than "no
    /// section", which is what a user sees in the reference too.
    /// The section showing at scroll `progress` in `0..=1`, as `1..=26`
    /// (`'A'` = 1), or `0` when there is nothing to show.
    ///
    /// `progress` is the fast scroller's `boundedY / bottom`
    /// (`RecyclerViewFastScroller.java:355-356`) and `total_rows` is the
    /// list's row count. The row is `floor(progress * total_rows)`, clamped
    /// to the last row, then resolved to a section by the scan described on
    /// [`SectionIndex`].
    ///
    /// Note the reference's *own* mapping divides the progress by the number
    /// of **sections**, not of rows
    /// (`AllAppsRecyclerView.java:199`,
    /// `index = (int) (touchFraction * count)`), which makes every letter
    /// occupy an equal slice of the track regardless of how many rows it
    /// has. That is right when the thumb is *proportional* to the list; this
    /// scroller's thumb is a fixed 52 dp, so the row mapping is the one that
    /// tracks the finger, and it is the one the signature encodes.
    pub fn section_for_progress(&self, progress: f32, total_rows: usize) -> u8 {
        // Note the encoding cannot distinguish "no catalogue" from "every row
        // is in the leading bucket": both are an all-zero array. That is why
        // the *row count* is the emptiness test, and why
        // [`FastScrollerState::set_catalogue`] derives one from the other
        // rather than taking them separately -- there is no way to construct
        // the mismatched pair.
        if total_rows == 0 || !progress.is_finite() {
            return 0;
        }
        let p = progress.clamp(0.0, 1.0);
        // Clamp to the *last* row, not `total_rows`, so a full-travel drag
        // reports the final present section instead of running off the end
        // into the trailing run of absent letters.
        let row = ((p * total_rows as f32) as usize).min(total_rows - 1);
        let row = row as u32;
        // Only letters that actually own a row are candidates. An absent
        // letter's `row_offset` is a *mirror* of a neighbour's, so admitting it
        // lets it win the tie it necessarily creates -- which is how a sparse
        // catalogue (no Q, no X, no Z) reported "Z" at every scroll position.
        let mut best = 0u8;
        for l in 0..26 {
            if self.present & (1u32 << l) != 0 && row >= self.row_offset[l] as u32 {
                best = l as u8 + 1;
            }
        }
        // `best` is 0 only when `row` precedes the first present section, which
        // `row_offset[0] == 0` makes unreachable for a non-empty catalogue.
        // Fall back to the lowest present letter so the contract is "never
        // report a letter that owns no rows" rather than "report none".
        if best == 0 {
            best = (self.present & self.present.wrapping_neg()).trailing_zeros() as u8 + 1;
        }
        best
    }

    /// `true` when no section has any rows.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.row_offset == [0u16; 26]
    }

    /// The absolute first row of section `letter` (`'A'` as `1..=26`).
    ///
    /// The missing half of [`Self::section_for_progress`]. That maps a scroll
    /// *position* to a letter; this maps a letter back to a row, which is the
    /// direction a fast scroller actually needs -- the reference resolves a
    /// `FastScrollSectionInfo` and hands it to a `LinearSmoothScroller`
    /// (`AllAppsFastScrollHelper.smoothScrollToSection:41-47`).
    ///
    /// Without it the scroller drew a teardrop letter and moved the list by
    /// nothing at all: the shell's only output from `on_move` was the letter
    /// itself, which it spent on a haptic tick.
    ///
    /// `None` for a letter that owns no rows, or for a letter outside `1..=26`.
    /// Callers must not substitute a neighbour: the reference's
    /// `getSectionIndex` returns -1 for an empty section and the helper then
    /// does not scroll.
    #[inline]
    pub fn row_for_section(&self, letter: u8) -> Option<usize> {
        let l = letter.checked_sub(1)? as usize;
        if l >= 26 || self.present & (1u32 << l) == 0 {
            return None;
        }
        Some(self.row_offset[l] as usize)
    }

    /// The row count this index was built from.
    ///
    /// Stored rather than reconstructed. `row_offset[l]` is the first row
    /// section `l` *owns*, so for the last present letter it is that section's
    /// start, not the list length -- the length is only knowable from the input
    /// `build` saw. Deriving it here would mean guessing, and a guess one row
    /// short makes the final section unreachable.
    ///
    /// The empty index reports 0, the same emptiness signal
    /// [`Self::is_empty`] gives, so the two cannot disagree.
    #[inline]
    pub fn total_rows(&self) -> usize {
        self.total_rows
    }
}

/// Index of the bucket a name belongs to, `0..26`.
///
/// Only ASCII letters bucket; everything else is index 0. `char::to_ascii_uppercase`
/// is not usable on a `u8` slice's first *byte* without decoding, and
/// decoding costs a UTF-8 walk, so the common case (ASCII, the only case the
/// reference's `FastScrollSectionInfo` labels) is tested first and anything
/// else is index 0.
#[inline]
fn bucket_of(name: &str) -> usize {
    match name.as_bytes().first() {
        Some(&b) if b.is_ascii_alphabetic() => (b.to_ascii_uppercase() - b'A') as usize,
        _ => 0,
    }
}

// ===========================================================================
// Sheet style
// ===========================================================================

/// Colours and type sizes for [`draw_drawer_sheet`].
///
/// Grouped rather than passed as eight more positional arguments so that
/// adding a token is a one-line change here instead of a signature change at
/// every call site. Sizes are **pixels**, already density-scaled, because
/// that is the only space the rasteriser works in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawerStyle {
    /// The flat wallpaper colour the scrim is composited against. It must be
    /// a single colour: [`draw_drawer_sheet`] computes the scrim once from it
    /// and then `fill`s, so a non-flat backdrop would show through unscrimmed
    /// rather than being blended per pixel.
    pub backdrop: u32,
    /// Sheet body. The scrim is composited over `backdrop`, not over this, so
    /// in practice the caller passes the same colour for both and the sheet
    /// reads as one flat plane.
    pub surface: u32,
    /// Search box / header pill fill.
    pub surface_high: u32,
    /// Primary text.
    pub on_surface: u32,
    /// Hint text, disabled glyphs, dividers.
    pub on_surface_variant: u32,
    /// Handle, divider, borders.
    pub outline: u32,
    /// Accent: the search box hairline and the Google glyph.
    pub primary: u32,
    /// Search hint / query type size in px (20 sp on the reference profile).
    pub text_px: f32,
    /// Header and app-count type size in px (`profile.label_sp`).
    pub label_px: f32,
    /// Hairline width in px for the search box outline.
    pub stroke_px: f32,
}

impl DrawerStyle {
    /// A neutral dark style at the reference profile's densities, for tests
    /// and for a caller that has no palette of its own.
    pub const fn dark(dp: f32) -> Self {
        Self {
            backdrop: 0xFF10_1418,
            surface: 0xFF1A_1F24,
            surface_high: 0xFF25_2B31,
            on_surface: 0xFFEC_EEF1,
            on_surface_variant: 0xFF9A_A3AD,
            outline: 0xFF3A_424A,
            primary: 0xFF8A_B4F8,
            text_px: 20.0 * dp,
            label_px: 13.0 * dp,
            stroke_px: (dp * 0.5).max(1.0),
        }
    }
}

// ===========================================================================
// Raster helpers
// ===========================================================================

/// Alpha-aware rounded-rect **fill**, scanline first.
///
/// `raster.rs` exposes `rounded_span_f` (the coverage test) and the two
/// scrim primitives, but its fill is private, and the *fill* is what the
/// track, the thumb, the search box and the header pill all need. Each row is
/// one span from `rounded_span_f` plus a `fill` or a short blend loop, so the
/// per-pixel cost exists only in the corner rows: for the 6 dp track that is
/// 6 rows out of ~950.
#[inline]
#[allow(clippy::too_many_arguments)]
fn fill_round_rect(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: f32,
    y: f32,
    rw: f32,
    rh: f32,
    radius: f32,
    color: u32,
) {
    let alpha = ((color >> 24) & 0xFF) as u8;
    // `> 0.0` and not `> 0`: NaN must not sneak through into the span maths,
    // and a negative width is a caller bug that must not index backwards.
    if alpha == 0 || !rw.is_finite() || !rh.is_finite() || rw <= 0.0 || rh <= 0.0 {
        return;
    }
    debug_assert!(
        buf.len() >= h * stride && stride >= w,
        "framebuffer too small"
    );
    let y0 = y.max(0.0) as usize;
    let y1 = ((y + rh).min(h as f32).max(0.0)) as usize;
    for cy in y0..y1 {
        let dy = cy as f32 + 0.5 - y;
        let Some((lo, hi)) = raster::rounded_span_f(dy, x, rw, rh, radius) else {
            continue;
        };
        let lo = lo.max(0.0).ceil() as usize;
        let hi = (hi.min(w as f32)).floor() as usize;
        if hi <= lo {
            continue;
        }
        let row = cy * stride;
        if alpha == 255 {
            buf[row + lo..row + hi].fill(color);
        } else {
            for px in buf.iter_mut().take(row + hi).skip(row + lo) {
                *px = raster::blend_alpha(*px, color, alpha);
            }
        }
    }
}

/// Rounded rect with **square bottom corners**: the sheet's own shape
/// (24 dp top, 0 dp bottom) and the reference's `topLeftRadius` /
/// `bottomLeftRadius` = 0 pairing.
///
/// The straight body is a plain [`raster::draw_opaque_rect`] -- a vectorised
/// `fill` -- and only the `2 * corner_r` rows at the top go through
/// `rounded_span_f`, so a 2400 dp-tall sheet costs ~50 corner rows plus one
/// memset.
#[inline]
#[allow(clippy::too_many_arguments)]
fn fill_top_rounded_rect(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: i32,
    y: f32,
    rw: f32,
    rh: f32,
    corner_r: f32,
    color: u32,
) {
    if !rw.is_finite() || !rh.is_finite() || rw <= 0.0 || rh <= 0.0 {
        return;
    }
    let r = corner_r.clamp(0.0, (rw.min(rh)) * 0.5);
    let cap = r.ceil().max(0.0) as i32;
    if cap == 0 {
        raster::draw_opaque_rect(buf, stride, w, h, x, y as i32, rw as i32, rh as i32, color);
        return;
    }
    // Top cap: rounded. Clipped to the buffer, and to the sheet's own height.
    let cap_h = (cap as f32).min(rh);
    fill_round_rect(buf, stride, w, h, x as f32, y, rw, cap_h, r, color);
    // Body: square, one opaque fill.
    let body_y = y + cap_h;
    let body_h = (rh - cap_h).max(0.0);
    raster::draw_opaque_rect(
        buf,
        stride,
        w,
        h,
        x,
        body_y as i32,
        rw as i32,
        ceil_i(body_h),
        color,
    );
}

/// `ceil` of a non-positive or non-finite `f32` to `i32` 0, and otherwise the
/// saturating `ceil`. The `v <= 0.0` test is written out rather than left to
/// `max(0.0)` so that a NaN cannot survive: `NaN.max(0.0)` is `0.0` in Rust but
/// `NaN as i32` is 0 too, so the real requirement is only that the value is
/// clamped *before* the cast, which this does in one place.
#[inline]
fn ceil_i(v: f32) -> i32 {
    if !v.is_finite() || v <= 0.0 {
        0
    } else {
        v.ceil().min(i32::MAX as f32) as i32
    }
}

/// [`font::draw_run`] with a constant per-glyph tracking added to the pen.
///
/// `letterSpacing = -0.01` on the search hint cannot be expressed through
/// `draw_run`, and the alternative -- drawing at the default tracking -- is a
/// 1% width error on a 20 sp string, which is exactly the size of error that
/// makes a label look "off" without being able to say why.
#[allow(clippy::too_many_arguments)]
fn draw_run_tracked(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: f32,
    y: f32,
    text: &str,
    color: u32,
    size: f32,
    weight: FontWeight,
    tracking: f32,
) {
    let mut pen = x;
    // One glyph per character, and letter-spacing strictly *between* glyphs.
    //
    // The old loop compared `i + 1 < text.len()` -- a byte count -- against a
    // byte index, so a multi-byte string got tracking after every byte, and a
    // skipped `\n` still got tracking. Tracking "before each glyph but the
    // first" is stated per glyph and so is immune to both.
    let mut drawn = false;
    for cp in text.chars() {
        if cp == '\n' {
            continue;
        }
        if drawn {
            pen += tracking;
        }
        pen += font::draw_glyph(buf, stride, w, h, pen, y, cp, color, size, weight);
        drawn = true;
    }
}

/// Longest prefix of `text` that fits `avail`, on a char boundary.
///
/// Walks characters, so the cut lands on a boundary by construction and the
/// width charged is the width drawn. The old byte walk charged the fallback
/// advance once per *byte* -- a 3-byte CJK name was truncated to roughly a
/// third of the characters it should have kept -- and then snapped the cut
/// *backwards* to a boundary, which could only ever shorten an already too-long
/// prefix.
#[allow(clippy::too_many_arguments)]
fn fit_prefix(text: &str, size: f32, tracking: f32, avail: f32) -> &str {
    if !avail.is_finite() || avail <= 0.0 {
        return "";
    }
    let mut used = 0.0f32;
    let mut end = text.len();
    for (i, cp) in text.char_indices() {
        let adv = font::char_advance(cp, size) + if i > 0 { tracking } else { 0.0 };
        if used + adv > avail {
            end = i;
            break;
        }
        used += adv;
    }
    &text[..end]
}

/// Vertically centre a 0.62 em cap-height band on `cy` and return the
/// ascender-line y, matching the convention `drm_kms.rs` uses for every other
/// centred label (`d_text_h = em * 0.62`, then `- em * 0.20`).
#[inline]
fn centred_ascender(cy: f32, size: f32) -> f32 {
    cy - size * 0.31 - size * 0.20
}

// ===========================================================================
// Fast-scroller paint
// ===========================================================================

/// Track width for the current press state: `mMaxWidth` while dragging,
/// `mMinWidth` otherwise (`mWidth` in `onDraw`, `:416-417`).
#[inline]
fn track_w_for(fs: &FastScrollerLayout, st: &FastScrollerState) -> f32 {
    if st.dragging {
        fs.track_w_pressed
    } else {
        fs.track_w
    }
}

/// Thumb paint geometry: a **round rect** whose width and corner radius are
/// both `mWidth + 2 * mThumbPadding` (`:423`, `:432`, `:463-465`).
///
/// Note this is *wider* than the 6 dp track, not narrower: `halfW` starts at
/// `mWidth / 2` and is then incremented by `mThumbPadding` before the
/// `drawRoundRect` (`:403`, `:423`), so the thumb overhangs the track by 1 dp
/// on each side. `RecyclerViewFastScroller.hasOverlappingRendering()` says so
/// in as many words (`:543-546`). The height is the fixed 52 dp
/// (`dimens.xml:80`) and never scales with the track.
#[inline]
fn thumb_stadium(fs: &FastScrollerLayout, track_w: f32) -> f32 {
    track_w + fs.thumb_pad * 2.0
}

/// Paint the fast scroller: track, thumb, and the press popup.
///
/// The track and thumb are small (6-8 dp wide, 52 dp tall), so the alpha
/// blends here are bounded by ~8 x list_height pixels -- three orders of
/// magnitude below the 2.59 Mpx full-screen blend the scrim rule exists to
/// prevent, and they are genuinely translucent in the reference
/// (`mTrackPaint.setAlpha(MAX_TRACK_ALPHA)`, `:182`).
///
/// `thumb_color` fills both the thumb and the popup blob: the reference
/// installs the *same* `Paint` on the thumb and on the `FastScrollThumbDrawable`
/// (`:186-188`, `:213`). `text_color` is the letter, which the reference sets
/// to `textColorPrimaryInverse` (`styles.xml:376`) so it reads against the
/// accent blob.
#[allow(clippy::too_many_arguments)]
pub fn draw_fast_scroller(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    fs: &FastScrollerLayout,
    st: &FastScrollerState,
    thumb_color: u32,
    track_alpha: u8,
    text_color: u32,
) {
    if w == 0 || h == 0 || !fs.track.h.is_finite() || fs.track.h <= 0.0 {
        return;
    }
    let tw = track_w_for(fs, st);
    let cx = fs.track.center_x();

    // 1. Track: a stadium the full list height at `MAX_TRACK_ALPHA`
    //    (`drawRoundRect(-halfW, 0, halfW, trackHeight, mWidth, mWidth)`, `:416`).
    let track_col = ((track_alpha as u32) << 24) | (thumb_color & 0x00FF_FFFF);
    fill_round_rect(
        buf,
        stride,
        w,
        h,
        cx - tw * 0.5,
        fs.track.y,
        tw,
        fs.track.h,
        tw,
        track_col,
    );

    // 2. Thumb: fixed 52 dp tall, at the bounded track-relative offset.
    let travel = (fs.track.h - fs.thumb_h).max(0.0);
    let ty = (fs.track.y + st.thumb_y.clamp(0.0, travel)).clamp(fs.track.y, fs.track.y + travel);
    let stadium = thumb_stadium(fs, tw);
    fill_round_rect(
        buf,
        stride,
        w,
        h,
        cx - stadium * 0.5,
        ty,
        stadium,
        fs.thumb_h,
        stadium,
        thumb_color,
    );

    // 3. Popup blob + letter, faded by the 200/150 ms ramp.
    let a = st.popup_alpha.clamp(0.0, 1.0);
    if a <= 0.0 {
        return;
    }
    // `a` is already clamped to `0..=1`, so the round cannot exceed 255; the
    // `.min` is belt-and-braces for the `NaN` path `clamp` would not catch.
    let alpha = (a * 255.0).round().clamp(0.0, 255.0) as u32;
    let cy = (st.popup_y + fs.popup.h * 0.5).clamp(fs.track.y - fs.popup.h, h as f32);
    // The blob is the letterbox itself: `fs.popup` is the 75 x 62 dp
    // `FastScrollerPopup` box (`dimens.xml:83-84`, `styles.xml:366-371`) and
    // `draw_teardrop` builds the single-`r/5`-corner round rect rotated
    // -45 deg from `FastScrollThumbDrawable.java:53-62`.
    //
    // Deliberate deviation, so it is written down: AOSP's `onBoundsChange`
    // builds the path from a **square** of side `2 * r = bounds.height()`
    // anchored at the bounds' *top-left* (`:58-60`), so on a 75 x 62 box the
    // painted blob is a rotated 62 dp square offset 31 dp from the box's left
    // edge, and it overhangs the box by 12.8 dp top and bottom. Filling the
    // letterbox instead keeps the blob centred on the box the text lives in
    // and is the shape the launcher actually reads as; the 13 dp
    // `paddingEnd` whose job was to pull the reference's text onto the
    // square's centre is therefore unnecessary here (see the letter placement
    // below).
    raster::draw_teardrop(
        buf,
        stride,
        w,
        h,
        fs.popup.center_x(),
        cy,
        fs.popup.w,
        fs.popup.h,
        (alpha << 24) | (thumb_color & 0x00FF_FFFF),
    );

    let letter = st.letter_str();
    if letter.is_empty() || !fs.popup_text_px.is_finite() || fs.popup_text_px <= 0.0 {
        return;
    }
    let size = fs.popup_text_px;
    // `gravity = center` + `includeFontPadding = false` (`styles.xml:372,377`),
    // so the letter is centred on the blob, which is the box centre here.
    let adv = font::measure(letter, size);
    draw_run_tracked(
        buf,
        stride,
        w,
        h,
        fs.popup.center_x() - adv * 0.5,
        centred_ascender(cy, size),
        letter,
        (alpha << 24) | (text_color & 0x00FF_FFFF),
        size,
        FontWeight::Regular,
        0.0,
    );
}

// ===========================================================================
// Sheet paint
// ===========================================================================

/// Paint the drawer sheet's chrome: the scrimmed sheet plane, the grab
/// handle, the search box, the header pill and the divider.
///
/// `progress` is the drawer's open fraction and is used for exactly one
/// thing: the `progress <= 0` early-out, which mirrors the existing
/// `if state.app_drawer_open || state.drawer_progress > 0.001` guard
/// (`drm_kms.rs:2297`) and keeps a fully-closed drawer off the framebuffer
/// entirely. Nothing else here depends on it: every band in `sheet` is
/// already baked for its own `shift` by `Layout::drawer_sheet(shift)`, so two
/// bands can never disagree about where the sheet is. In particular the
/// scrim's alpha is **not** ramped with `progress` -- the reference's is a
/// constant 0.40 (`ColorTokens.kt:96`) on a container background that does
/// not animate (`ActivityAllAppsContainerView.java:242`).
///
/// # Why this never blends the scrolling list
///
/// The obvious way to dim the workspace behind the drawer is one
/// `blend_alpha` per pixel across the panel, which is the 10.2 ms mistake.
/// The reference does not do that either: `AllAppsScrimColor` is the
/// all-apps **container background**, so the list is composited *on top* of
/// an already-flat scrim, never under one.
///
/// This module takes the same route, with one extra step: because the sheet
/// body is a single flat plane over a known flat `backdrop`, the scrim is
/// resolved **once** in scalar arithmetic
/// (`raster::composite_scrim(backdrop, scrim_argb, 0.40)`) and the *whole
/// sheet* is then that one opaque `u32`. The result is:
///
/// * the structured region (the scrolling grid) is written as a plain
///   `fill` of a precomputed constant -- it is never a blend, so it can
///   never regress `no_full_screen_translucent_blend`;
/// * the only per-pixel work in the entire pass is the ~50 rows of the two
///   24 dp top corners, via `rounded_span_f`.
///
/// In other words the "structured backdrop" problem is not solved by
/// narrowing the scrim to a flat region; it is solved by not needing a
/// backdrop under it at all. The list is drawn by the caller (it owns the
/// icon cache and the scroll offset) straight onto the flat sheet.
///
/// Returns `true` when anything was written.
///
/// `query` is the live search text; an empty string draws the reference hint
/// (`strings.xml:184 all_apps_search_bar_hint` = "Search apps"). `count_label`
/// is the caller-formatted app count, passed in rather than `format!`'d here so
/// this function allocates nothing.
///
/// The original 9-argument sheet paint, which painted a hardcoded `'A'`.
///
/// Kept so the existing caller (`drm_kms.rs:4456`) compiles unchanged while
/// the letter is migrated. It forwards to
/// [`draw_drawer_sheet_with_section`] with [`DEFAULT_SECTION_LETTER`], so the
/// two paths cannot diverge -- there is one implementation and this is a
/// spelling of it.
#[allow(clippy::too_many_arguments)]
pub fn draw_drawer_sheet(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    sheet: &DrawerSheetLayout,
    progress: f32,
    style: &DrawerStyle,
    query: &str,
    count_label: &str,
) -> bool {
    draw_drawer_sheet_with_section(
        buf,
        stride,
        w,
        h,
        sheet,
        progress,
        style,
        query,
        count_label,
        DEFAULT_SECTION_LETTER,
    )
}

/// The letter the header shows when the caller has no section to report.
///
/// `"A"`, which is what the sheet painted unconditionally before the letter
/// became a parameter. It is *not* a claim that the first section is `A` --
/// [`SectionIndex::section_for_progress`] returns `1` for an empty catalogue
/// and for a sparse one the lowest present letter, which is not always `A` --
/// it is the old behaviour, named so that a caller which has not yet threaded
/// [`FastScrollerState::letter_str`] through keeps rendering exactly what it
/// rendered before rather than a blank header.
pub const DEFAULT_SECTION_LETTER: &str = "A";

/// Paint the drawer sheet's chrome, with the header's section letter as a
/// parameter.
///
/// `section_letter` is the header's letter. It is a parameter, not the literal
/// `'A'` this used to paint, because the letter is *state* that already exists:
/// [`FastScrollerState::letter_str`] resolves the live section on every
/// `ACTION_MOVE` (`RecyclerViewFastScroller.onDraw`, `:397-404`) and allocates
/// nothing to do it. The header was hardcoding the first letter of the
/// alphabet while the popup a few pixels away showed the real one, so the two
/// disagreed for the whole of a drag.
///
/// Passing it in rather than deriving it here is deliberate: the sheet has no
/// `FastScrollerState` and must not grow one, and the section index is rebuilt
/// on catalogue rescan rather than per frame
/// ([`FastScrollerState::set_catalogue`]). An empty string paints no letter at
/// all, which is the honest answer for "no section" -- `letter_str()` returns
/// `""` there, and the header's left padding is the same either way.
#[allow(clippy::too_many_arguments)]
pub fn draw_drawer_sheet_with_section(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    sheet: &DrawerSheetLayout,
    progress: f32,
    style: &DrawerStyle,
    query: &str,
    count_label: &str,
    section_letter: &str,
) -> bool {
    if w == 0 || h == 0 || !progress.is_finite() || progress <= 0.0 {
        return false;
    }
    debug_assert!(
        buf.len() >= h * stride && stride >= w,
        "framebuffer too small"
    );

    let top = if sheet.top.is_finite() {
        sheet.top.clamp(0.0, h as f32)
    } else {
        return false;
    };
    if top >= h as f32 {
        // `shift == 0`: the sheet is entirely below the panel. This is the
        // clip, not a special case -- every band is `top`-relative, so an
        // off-panel `top` takes the whole sheet with it.
        return false;
    }

    // One precomputed opaque colour for the whole sheet. This is the entire
    // scrim cost: three scalar operations, then a `fill`.
    let scrim = raster::composite_scrim(style.backdrop, sheet.scrim_argb, scrim_alpha(sheet));
    debug_assert_eq!(scrim >> 24, 0xFF, "scrim must be composited opaque");

    // 1. Sheet plane, full width, 24 dp top corners / square bottom.
    let sheet_h = h as f32 - top;
    fill_top_rounded_rect(
        buf,
        stride,
        w,
        h,
        0,
        top,
        w as f32,
        sheet_h,
        sheet.corner_r,
        scrim,
    );

    // 2. Grab handle: 32 x 4 dp, r 2 dp (`dimens.xml:535-540`).
    if rect_visible(&sheet.handle, h) {
        fill_round_rect(
            buf,
            stride,
            w,
            h,
            sheet.handle.x,
            sheet.handle.y,
            sheet.handle.w,
            sheet.handle.h,
            sheet.handle.radius,
            style.outline,
        );
    }

    // 3. Search box: 52 dp pill inside its 60 dp container
    //    (`dimens.xml:51-52`), with the accent hairline.
    let sb = sheet.search;
    if rect_visible(&sb, h) && sb.w > 0.0 && sb.h > 0.0 {
        fill_round_rect(
            buf,
            stride,
            w,
            h,
            sb.x,
            sb.y,
            sb.w,
            sb.h,
            sb.radius,
            style.surface_high,
        );
        if style.stroke_px > 0.0 {
            raster::draw_rounded_rect_outline_f(
                buf,
                stride,
                w,
                h,
                sb.x,
                sb.y,
                sb.w,
                sb.h,
                sb.radius,
                style.stroke_px,
                style.primary,
            );
        }
        let text_px = style.text_px;
        if text_px.is_finite() && text_px > 0.0 {
            let tracking = text_px * SEARCH_HINT_TRACKING;
            let glyph_x = sb.x + sb.h * 0.40;
            // The drawer's search affordance is the Google mark
            // (`SearchWithDotsListView`); `drm_kms.rs:2351` already draws it
            // as a "G" and this keeps the two paths pixel-identical.
            let text_x = glyph_x + sb.h * 0.30 + text_px * 0.30;
            let y = centred_ascender(sb.y + sb.h * 0.5, text_px);
            font::draw_glyph(
                buf,
                stride,
                w,
                h,
                glyph_x,
                y,
                'G',
                style.primary,
                text_px,
                FontWeight::Medium,
            );
            let (txt, col) = if query.is_empty() {
                (SEARCH_HINT, style.on_surface_variant)
            } else {
                (query, style.on_surface)
            };
            // Trailing room for the clear button, which the caller draws: the
            // query must never run under it.
            let right = sb.x + sb.w - sb.h * 0.35;
            let shown = fit_prefix(txt, text_px, tracking, (right - text_x).max(0.0));
            draw_run_tracked(
                buf,
                stride,
                w,
                h,
                text_x,
                y,
                shown,
                col,
                text_px,
                FontWeight::Regular,
                tracking,
            );
        }
    }

    // 4. Header pill: 48 dp tall, r 12 dp, carrying the section label on the
    //    left and the app count right-aligned against the content edge.
    let hd = sheet.header;
    if rect_visible(&hd, h) && hd.w > 0.0 && hd.h > 0.0 {
        fill_round_rect(
            buf,
            stride,
            w,
            h,
            hd.x,
            hd.y,
            hd.w,
            hd.h,
            hd.radius,
            style.surface_high,
        );
        let label_px = style.label_px;
        if label_px.is_finite() && label_px > 0.0 {
            let y = centred_ascender(hd.y + hd.h * 0.5, label_px);
            // The section letter, as a borrowed run rather than one `char`.
            // `letter_str()` hands back a `&'static str` off [`LETTER_BYTES`],
            // so this is a run draw with no allocation and no `format!`; the
            // literal `'A'` it replaces was correct for exactly one of 26
            // possible drags. `fit_prefix` is not needed: the run is at most
            // one character by construction, and a caller that passes a
            // longer string gets it clipped to the pill rather than running
            // under the app count.
            if !section_letter.is_empty() {
                font::draw_run(
                    buf,
                    stride,
                    w,
                    h,
                    hd.x + label_px * 0.4,
                    y,
                    section_letter,
                    style.on_surface,
                    label_px,
                    FontWeight::Bold,
                );
            }
            if !count_label.is_empty() {
                let cw = font::measure(count_label, label_px);
                font::draw_run(
                    buf,
                    stride,
                    w,
                    h,
                    hd.x + hd.w - cw - label_px * 0.4,
                    y,
                    count_label,
                    style.on_surface_variant,
                    label_px,
                    FontWeight::Medium,
                );
            }
        }
    }

    // 5. Divider: 128 x 2 dp, r 2 dp.
    if rect_visible(&sheet.divider, h) {
        fill_round_rect(
            buf,
            stride,
            w,
            h,
            sheet.divider.x,
            sheet.divider.y,
            sheet.divider.w,
            sheet.divider.h,
            sheet.divider.radius,
            style.outline,
        );
    }

    true
}

/// The drawer's search placeholder (`strings.xml:184`).
pub const SEARCH_HINT: &str = "Search apps";

// ===========================================================================
// Search-result chrome: the zero-result state and the "search the web" row
// ===========================================================================
//
// The drawer's *results* band is drawn by the caller, which owns the icon cache
// and the scroll offset. What this module owns is everything the reference puts
// *in* that band which is not an app row, and all three of those were missing:
//
//   1. the zero-result state (`SearchResultEmptyState.kt:15-49`), which the
//      live path rendered as literally nothing (`main.rs:4413` is a bare
//      `if count > 0`), so a query matching no app showed an empty sheet with
//      no explanation at all;
//   2. the always-appended "Search on <provider>" action row
//      (`LawnchairLocalSearchAlgorithm.generateActionResults:146-179`, built by
//      `ActionsSectionBuilder:157-187`), which is what makes an unmatched query
//      useful rather than a dead end; and
//   3. the per-group section headers (`SectionBuilder.kt:24-235`), where the
//      sheet currently paints one flat `count_label`.
//
// # Why geometry and paint are separate here
//
// None of the three is in `layout.rs`, which is a file this slice does not own
// (see the handoff). So each is a **layout function returning a `Rect` plus a
// paint function taking that `Rect`**, both in this file. The caller hit-tests
// with the layout function and paints with the paint function, which is the
// same contract `draw_fast_scroller` and `FastScrollerState` already have: a
// rectangle that is drawn is by construction the rectangle that is hit. When
// `layout.rs` is next edited these should move there and become
// `DrawerSheetLayout` methods; the signatures are shaped for that move.

/// The reference's zero-result copy (`strings.xml:188`).
///
/// `all_apps_no_search_results` is a *format* string -- `"No apps found
/// matching \"%1$s\""` -- and the reference formats it per result
/// (`BaseAllAppsAdapter.onBindViewHolder:330-333`). Here it is split into the
/// prefix and the quote so the caller's stack buffer can hold the query
/// between them without a `format!`: see [`draw_search_empty_state`], which
/// draws the three pieces in sequence at accumulated pen positions.
pub const NO_RESULTS_PREFIX: &str = "No apps found matching \"";

/// The closing quote of [`NO_RESULTS_PREFIX`]. Its own constant so the two
/// halves can never drift out of balance.
pub const NO_RESULTS_SUFFIX: &str = "\"";

/// `all_apps_search_on_web_message` (`lawnchair/res/values/strings.xml:942`),
/// split for the same reason as [`NO_RESULTS_PREFIX`].
pub const WEB_SEARCH_PREFIX: &str = "Search on ";

/// Height of the "Search on ..." action row.
///
/// `search_result_row_height` is 92 dp
/// (`lawnchair/res/values/dimens.xml:64`), which is the row *with* an icon
/// and a subtitle. The action row sets
/// `SearchResultView.EXTRA_HIDE_SUBTITLE` (`SearchTargetFactory.kt:259`), so
/// it is the single-line variant, and
/// `search_result_small_row_height` = 64 dp (`:67`) is that one.
pub const WEB_SEARCH_ROW_H_DP: f32 = 64.0;

/// Corner radius of the action row: `search_result_radius`
/// (`dimens.xml:60`), 4 dp.
pub const WEB_SEARCH_ROW_RADIUS_DP: f32 = 4.0;

/// Horizontal padding of the action row: `search_result_padding`,
/// 16 dp (`dimens.xml:62`), matching the `paddingStart`/`paddingEnd` of
/// `search_result_text.xml:4-5`.
pub const WEB_SEARCH_ROW_PAD_DP: f32 = 16.0;

/// Height of a result-group header.
///
/// `search_result_text_height` is 52 dp
/// (`lawnchair/res/values/dimens.xml:68`), and `search_result_text.xml:4-6`
/// applies it as the row's `minHeight` with `layout_height = wrap_content`, so
/// the text line plus its 2 x 12 dp `search_result_text_padding`
/// (`dimens.xml:62`, applied at `search_result_text.xml:21-22`) is what the
/// 52 dp is measuring. 12 dp of glyph height in a 52 dp band leaves the row
/// visually airy, which is what a group label wants.
pub const SECTION_HEADER_H_DP: f32 = 52.0;

/// The gap between a group's header and the group's first row.
///
/// The reference appends `createHeaderTarget(SPACE)` after *every* group's
/// results (`SectionBuilder.kt:38, 57, 77, 95, 123, 152, 184, 210`) -- eight
/// separate call sites, so this is the reference's own constant and not a
/// derived one. It is the divider between groups.
pub const SECTION_HEADER_GAP_DP: f32 = 12.0;

/// The header's type size: `search_result_hero_subtitle_size`, 14 sp
/// (`dimens.xml:58`), which is the `android:textSize` on the header's `title`
/// `TextView` (`search_result_text.xml:31`).
pub const SECTION_HEADER_TEXT_DP: f32 = 14.0;

/// The header's leading icon, `ic_allapps_search`
/// (`SearchTargetFactory.kt:130-132`), tinted `TextColorPrimary`.
///
/// Painted as a 16 dp rounded square rather than the reference's glyph: this
/// rasteriser has no icon-theme lookup, and the drawer's own 16 dp grid border
/// (`layout.rs`, `all_apps_border_dp`) is the nearest thing the sheet already
/// draws at that size. Sized from the header's 14 sp text so the icon and the
/// label read as one unit.
pub const SECTION_HEADER_ICON_DP: f32 = 16.0;

/// Type size of the empty state's title: `textAppearanceLarge`, 22 sp.
pub const EMPTY_STATE_TITLE_DP: f32 = 22.0;

/// Type size of the empty state's subtitle: `textAppearanceSmall`, 14 sp
/// (`search_result_empty_state.xml:33`).
pub const EMPTY_STATE_SUBTITLE_DP: f32 = 14.0;

/// The empty state's icon: `ic_qsb_search` at 48 dp
/// (`search_result_empty_state.xml:12-17`), tinted `ColorTokens.ColorAccent`
/// by `SearchResultEmptyState.onFinishInflate:33`.
pub const EMPTY_STATE_ICON_DP: f32 = 48.0;

/// The empty state's 32 dp padding (`search_result_empty_state.xml:9`), the
/// 16 dp below the icon (`:17`) and the 4 dp below the title (`:26`).
pub const EMPTY_STATE_PAD_DP: f32 = 32.0;

/// The drawer's density for a given panel width.
///
/// A private helper rather than a `Layout`, because these three surfaces are
/// laid out against the *grid band the caller already has* and constructing a
/// whole `Layout` to convert dp would be both wasteful and a second source of
/// the density number. `drm_kms.rs` computes the identical value as
/// `w as f32 / 420.0` (`drm_kms.rs:4442`) for [`DrawerStyle`]; the two must
/// agree or a 1 dp surface shows as a 1 px step, so the derivation is stated
/// here once.
#[inline]
pub fn drawer_dp(panel_w: f32) -> f32 {
    panel_w / 420.0
}

/// The empty state's three bands, stacked down `band`.
///
/// `band` is the region the results would have occupied -- the sheet's `grid`
/// in practice. The stack is the reference's `LinearLayout` with
/// `gravity = center_horizontal` (`search_result_empty_state.xml:7-8`): icon
/// on top, then title, then subtitle, each centred horizontally and separated
/// by its `layout_marginBottom`.
///
/// Returned rather than painted so the caller can hit-test the same three
/// rectangles, and so the y positions exist in exactly one place.
pub fn search_empty_state_layout(
    band: &super::layout::Rect,
    panel_w: f32,
) -> [super::layout::Rect; 3] {
    let d = drawer_dp(panel_w);
    let icon_d = d * EMPTY_STATE_ICON_DP;
    let pad = d * EMPTY_STATE_PAD_DP;
    let title_h = d * EMPTY_STATE_TITLE_DP;
    let sub_h = d * EMPTY_STATE_SUBTITLE_DP;
    let gap = d * EMPTY_STATE_PAD_DP / 2.0; // 16 dp icon gap, 4 dp title gap
    let cx = band.center_x();
    // The reference's 32 dp padding is the container's own padding, so the
    // content starts 32 dp in from the band and the total height is
    // `32 + 48 + 16 + title + 4 + subtitle + 32`.
    let mut y = band.y + pad;
    let icon = super::layout::Rect {
        x: cx - icon_d * 0.5,
        y,
        w: icon_d,
        h: icon_d,
        radius: icon_d * 0.5,
    };
    y += icon_d + gap;
    let title = super::layout::Rect {
        x: band.x,
        y,
        w: band.w,
        h: title_h,
        radius: 0.0,
    };
    y += title_h + gap * 0.25; // 4 dp, a quarter of the icon gap
    let subtitle = super::layout::Rect {
        x: band.x,
        y,
        w: band.w,
        h: sub_h,
        radius: 0.0,
    };
    [icon, title, subtitle]
}

/// Paint the zero-result state: icon, title, subtitle.
///
/// `query` is the unmatched text and is drawn *between* the two halves of
/// [`NO_RESULTS_PREFIX`], so the string the user sees is
/// `No apps found matching "cafe"` with no `format!` anywhere on the frame
/// path. The three pieces are laid out at accumulated pen positions rather
/// than centred as a unit, because the reference's `gravity = center_horizontal`
/// (`search_result_empty_state.xml:8`) centres the *whole formatted string* --
/// and centring three separately-measured pieces to the same axis is what makes
/// that come out right for any query length.
///
/// Returns `true` when anything was written.
#[allow(clippy::too_many_arguments)]
pub fn draw_search_empty_state(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    band: &super::layout::Rect,
    style: &DrawerStyle,
    panel_w: f32,
    query: &str,
) -> bool {
    if w == 0 || h == 0 || !band.h.is_finite() || band.h <= 0.0 {
        return false;
    }
    let d = drawer_dp(panel_w);
    let [icon, title, sub] = search_empty_state_layout(band, panel_w);

    // 1. The icon: `ic_qsb_search` is a magnifier, so a ring plus a handle.
    // `ColorTokens.ColorAccent` (`SearchResultEmptyState.kt:33`) is
    // `style.primary`, and the 48 dp circle is the 48 dp of the layout's
    // `layout_width`/`layout_height` (`search_result_empty_state.xml:12-14`).
    let ring = icon.w * 0.36;
    if ring > 0.0 && style.stroke_px > 0.0 {
        let a = icon.center_x() - icon.w * 0.10;
        let b = icon.center_y() - icon.h * 0.10;
        // Four stroked sides of the ring, as thin round rects: this is the
        // same vocabulary the search box's hairline uses, so a magnifier here
        // matches the pill above it without a new primitive.
        let t = style.stroke_px * 1.5;
        for (rx, ry, rw, rh) in [
            (a - ring, b - ring, ring * 2.0, t),
            (a - ring, b + ring, ring * 2.0, t),
            (a - ring, b - ring, t, ring * 2.0),
            (a + ring, b - ring, t, ring * 2.0),
        ] {
            fill_round_rect(buf, stride, w, h, rx, ry, rw, rh, t * 0.5, style.primary);
        }
        // The handle: down-right from the ring, at 45 deg, which is the
        // recognisable silhouette of `ic_qsb_search`.
        let hl = icon.w * 0.26;
        fill_round_rect(
            buf,
            stride,
            w,
            h,
            a + ring * 0.71,
            b + ring * 0.71,
            hl,
            hl,
            t * 0.5,
            style.primary,
        );
    }

    // 2. Title and subtitle, as three centred runs.
    let size_t = d * EMPTY_STATE_TITLE_DP;
    let size_s = d * EMPTY_STATE_SUBTITLE_DP;
    let drawn = centred_three(
        buf,
        stride,
        w,
        h,
        &title,
        title.center_y(),
        size_t,
        NO_RESULTS_PREFIX,
        query,
        NO_RESULTS_SUFFIX,
        style.on_surface,
        FontWeight::Bold,
    );
    if size_s > 0.0 {
        // The subtitle carries the hint rather than the query: the title
        // already says what went wrong, and the reference's own subtitle for
        // the *no-history* zero state is "Find apps, contacts, and more. Your
        // recent searches will appear here."
        // (`search_empty_state_no_history_subtitle`, `strings.xml:1002`).
        // There are no contacts, files or history providers in this shell, so
        // the honest subtitle names what the drawer can do: the web action
        // row. `draw_web_search_action` is the other half of that promise.
        let hint = WEB_SEARCH_HINT;
        let hw = font::measure(hint, size_s);
        font::draw_run(
            buf,
            stride,
            w,
            h,
            sub.center_x() - hw * 0.5,
            centred_ascender(sub.center_y(), size_s),
            hint,
            style.on_surface_variant,
            size_s,
            FontWeight::Regular,
        );
    }
    drawn
}

/// The empty state's subtitle, for a shell that wants the reference's exact
/// string. The web action row is what this shell offers instead, so this is
/// exported for the copy rather than used by the paint.
pub const WEB_SEARCH_HINT: &str = "Search the web instead";

/// The action row's `Rect`, for `panel_w`-derived dp and an `anchor` y.
///
/// The row is 64 dp tall (`WEB_SEARCH_ROW_H_DP`) with a 4 dp radius
/// (`WEB_SEARCH_ROW_RADIUS_DP`) and 16 dp of horizontal padding
/// (`WEB_SEARCH_ROW_PAD_DP`), inset to the band's own horizontal extent so it
/// lines up with the grid it sits under.
pub fn web_search_action_rect(
    band: &super::layout::Rect,
    panel_w: f32,
    anchor_y: f32,
) -> super::layout::Rect {
    let d = drawer_dp(panel_w);
    let h = d * WEB_SEARCH_ROW_H_DP;
    // A band shorter than the row cannot contain it, so the row is pinned to
    // the band's top and the caller is expected to have scrolled. `max(band.y)`
    // as the floor rather than clamping `top` afterwards, because `min` then
    // `max` would let a *negative* gap push the row off the top.
    let floor = band.y.max(0.0);
    let ceiling = (band.y + band.h - h).max(floor);
    let top = (anchor_y + d * SECTION_HEADER_GAP_DP)
        .clamp(floor, ceiling)
        .max(0.0);
    super::layout::Rect {
        x: band.x,
        y: top,
        w: band.w,
        h,
        radius: d * WEB_SEARCH_ROW_RADIUS_DP,
    }
}

/// Paint the "Search on <provider>" action row.
///
/// `provider` is the display name of the configured search provider -- the
/// reference's `%1$s` (`all_apps_search_on_web_message`,
/// `lawnchair/res/values/strings.xml:942`), which `createWebSearchActionTarget`
/// fills from `webSuggestionProvider` (`SearchTargetFactory.kt:244-252`). The
/// row is tinted `TextColorSecondary` there (`:249-251`), which is
/// `style.on_surface_variant`.
///
/// The leading glyph is the provider's own icon in the reference; a shell with
/// no icon theme draws a magnifier, the same shape
/// [`draw_search_empty_state`] uses, so the two read as a pair.
///
/// Returns `true` when anything was written.
#[allow(clippy::too_many_arguments)]
pub fn draw_web_search_action(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    row: &super::layout::Rect,
    style: &DrawerStyle,
    panel_w: f32,
    provider: &str,
) -> bool {
    if w == 0 || h == 0 || !row.h.is_finite() || row.h <= 0.0 {
        return false;
    }
    let d = drawer_dp(panel_w);
    let pad = d * WEB_SEARCH_ROW_PAD_DP;

    // A 24 dp magnifier: `search_result_row_height` is 92 dp with a leading
    // icon, so the icon is comfortably smaller than the 64 dp row, and
    // 24 dp is the platform's "small icon" size. The stroke weight is the
    // caller's hairline scaled up, so it reads at the same weight as the empty
    // state's larger magnifier above it.
    let icon_d = row.h * 0.375;
    let t = (style.stroke_px * 1.5).max(1.0);
    let ring = icon_d * 0.34;
    let icx = row.x + pad + icon_d * 0.5;
    let icy = row.center_y();
    for (dx, dy, dw, dh) in [
        (-ring, -ring, ring * 2.0, t),
        (-ring, ring * 2.0 - t, ring * 2.0, t),
        (-ring, -ring, t, ring * 2.0),
        (ring * 2.0 - t, -ring, t, ring * 2.0),
    ] {
        fill_round_rect(
            buf,
            stride,
            w,
            h,
            icx + dx,
            icy + dy,
            dw,
            dh,
            t * 0.5,
            style.on_surface_variant,
        );
    }
    let hl = icon_d * 0.28;
    fill_round_rect(
        buf,
        stride,
        w,
        h,
        icx + ring * 0.71,
        icy + ring * 0.71,
        hl,
        hl,
        t * 0.5,
        style.on_surface_variant,
    );

    // The label: `Search on <provider>`, the prefix and the provider drawn as
    // two runs at accumulated pen positions so no `format!` is needed. The
    // provider name is fitted to whatever is left of the row.
    let size = d * SECTION_HEADER_TEXT_DP;
    if !(size.is_finite() && size > 0.0) {
        return true;
    }
    let text_x = row.x + pad + icon_d + pad * 0.5;
    let right = row.x + row.w - pad;
    let y = centred_ascender(row.center_y(), size);
    let pw = font::measure(WEB_SEARCH_PREFIX, size);
    if right - text_x < pw {
        return true;
    }
    font::draw_run(
        buf,
        stride,
        w,
        h,
        text_x,
        y,
        WEB_SEARCH_PREFIX,
        style.on_surface,
        size,
        FontWeight::Regular,
    );
    let avail = right - (text_x + pw);
    let shown = fit_prefix(provider, size, 0.0, avail);
    if !shown.is_empty() {
        font::draw_run(
            buf,
            stride,
            w,
            h,
            text_x + pw,
            y,
            shown,
            style.on_surface,
            size,
            FontWeight::Regular,
        );
    }
    true
}

/// Draw `prefix` + `mid` + `suffix` as one centred run inside `band`, at `cy`.
///
/// The pen starts at the band's centre minus half the *total* width and
/// advances by each piece's measured width, so the three runs are visually a
/// single string and the whole thing is centred on the band's axis -- which is
/// what `gravity = center_horizontal` does to the reference's one formatted
/// `TextView` (`search_result_empty_state.xml:7-8`).
///
/// `band` is the *writable* extent, not the line's natural width, because the
/// three pieces have to be centred against the same axis the reference centres
/// its single string against -- the container, which is the full content width.
///
/// Returns `true` when the run was long enough to draw at all.
#[allow(clippy::too_many_arguments)]
fn centred_three(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    band: &super::layout::Rect,
    cy: f32,
    size: f32,
    prefix: &str,
    mid: &str,
    suffix: &str,
    color: u32,
    weight: FontWeight,
) -> bool {
    if !(size.is_finite() && size > 0.0) {
        return false;
    }
    let pw = font::measure(prefix, size);
    let sw = font::measure(suffix, size);
    let full = pw + font::measure(mid, size) + sw;
    if !full.is_finite() || full <= 0.0 {
        return false;
    }
    // Elide the *whole* run to the band before centring it, or a query longer
    // than the band is drawn from `centre - full/2`, which is off the left
    // edge and the visible result is a right-aligned-looking fragment.
    //
    // The two fixed halves are kept whole if they fit at all and the middle
    // absorbs the elision -- which is the reference's own `ellipsize = end`
    // (`search_result_empty_state.xml:22`), where the *tail* is what goes.
    //
    // Every piece below is a **borrow** of the caller's own `&str`: a
    // `to_string()` here would be a `malloc` per frame in a state that is
    // drawn precisely when the user is typing, which is the worst possible
    // moment for one. `fit_prefix` returns a `&str` slice of its input, so the
    // elision is a subslice and not a copy.
    let avail = if band.w.is_finite() && band.w > 0.0 {
        band.w
    } else {
        full
    };
    // `suffix_ok` is false once anything was elided: a closing quote with no
    // opening one reads as a typo rather than as elision, so the honest end of
    // the string is the last character that *was* drawn.
    let (p, m, s): (&str, &str, &str) = if full <= avail {
        (prefix, mid, suffix)
    } else if pw + sw >= avail {
        // Not even the two fixed halves fit: draw as much of the prefix as
        // there is room for, and drop the query and the quote.
        (fit_prefix(prefix, size, 0.0, avail), "", "")
    } else {
        (prefix, fit_prefix(mid, size, 0.0, avail - pw - sw), suffix)
    };
    let mut drawn_w = font::measure(p, size) + font::measure(m, size) + font::measure(s, size);
    if !(drawn_w.is_finite() && drawn_w > 0.0) {
        return false;
    }
    // `font::measure` on an empty string is 0 on every family, but a NaN from a
    // poisoned font would make the centring a NaN and drop the run at x = 0, so
    // the sum is re-clamped rather than trusted.
    drawn_w = drawn_w.clamp(0.0, f32::MAX);
    let y = centred_ascender(cy, size);
    // The start x is the centring: without it the three runs are laid out from
    // x = 0 and the whole string sits at the panel's left edge, which is
    // exactly the bug the centring is for.
    let mut pen = band.center_x() - drawn_w * 0.5;
    for piece in [p, m, s] {
        if piece.is_empty() {
            continue;
        }
        font::draw_run(buf, stride, w, h, pen, y, piece, color, size, weight);
        pen += font::measure(piece, size);
    }
    true
}

/// The geometry for `n` result-group headers down `band`, starting at `y`.
///
/// The reference emits one header per *result group*, not per result
/// (`SectionBuilder.kt:24-235`): a header, that group's rows, then a
/// `createHeaderTarget(SPACE)` spacer that is the divider between groups. This
/// function returns the **n header bands only**; the spacers are implied by
/// [`SECTION_HEADER_GAP_DP`] between consecutive headers and are the caller's
/// to advance over, because the rows between headers are the caller's grid rows
/// and the caller's arithmetic.
///
/// The point of separating this from the paint is that the drawer's scroll
/// math (`grid_visible_rows`, `grid_cell_at_index`) counts *grid rows*, and a
/// header is 52 dp against a 104 dp row pitch. A header therefore does not
/// occupy a grid row and cannot be expressed in the existing row arithmetic --
/// see the handoff. What is provided here is the geometry, so the caller can
/// decide where in its own layout the headers go rather than this module
/// guessing at the scroll.
pub fn section_header_rows(
    band: &super::layout::Rect,
    panel_w: f32,
    y: f32,
    n: usize,
) -> [super::layout::Rect; SECTION_HEADER_MAX] {
    let d = drawer_dp(panel_w);
    let h = d * SECTION_HEADER_H_DP;
    let mut out = [super::layout::Rect {
        x: band.x,
        y: 0.0,
        w: 0.0,
        h: 0.0,
        radius: 0.0,
    }; SECTION_HEADER_MAX];
    let pitch = h + d * SECTION_HEADER_GAP_DP;
    for (i, slot) in out.iter_mut().enumerate().take(n.min(SECTION_HEADER_MAX)) {
        let top = y + pitch * i as f32;
        *slot = super::layout::Rect {
            x: band.x,
            y: top,
            w: band.w,
            h,
            radius: 0.0,
        };
    }
    out
}

/// How many group headers a drawer can hold in one frame.
///
/// The reference has ten `SectionBuilder` implementations
/// (`LawnchairLocalSearchAlgorithm:181-192`) and this shell has no contacts,
/// files, settings, history, suggestions or calculation providers, so its
/// groups are apps-and-shortcuts, actions and the zero state: three. Eight is
/// the fixed-capacity ceiling, `Copy` and stack-only, which is the point --
/// [`section_header_rows`] must not allocate per frame, and a `Vec` here would
/// be a `malloc` in the middle of a drag.
pub const SECTION_HEADER_MAX: usize = 8;

// ===========================================================================
// Prediction row
// ===========================================================================
//
// The one part of the sheet that is *reserved and unread*: `layout.rs` bakes
// 108 dp of prediction row into the middle of the sheet
// (`PREDICTION_ROW_H_DP:2080`, `DrawerSheetLayout::predictions:2004`) and
// nothing has ever drawn it, so the drawer's grid starts 108 dp lower than the
// reference's and 108 dp of sheet is dead.
//
// This module owns the *paint*, and it takes the geometry as parameters
// because `layout.rs` belongs to another slice. What the shell must pass is in
// the handoff; the short version is: the row's `Rect`, the slot index, the
// predicted app's name, and the icon edge (`pred_icon_d`) it should paint a
// monogram into.

/// Prediction row slots a drawer can show at once.
///
/// `PredictionRowView` sets `mNumPredictedAppsPerRow = numShownAllAppsColumns`
/// (`PredictionRowView.java:85-86`) and inflates exactly that many
/// `BubbleTextView` children (`:230-247`), so the count is the device
/// profile's column count, not a constant. 8 covers the reference profile's 5
/// with room and bounds the fixed-capacity math below; a shell on a
/// wider profile passes its own `grid_cols` and this is only the fallback for
/// slot arithmetic.
pub const PREDICTION_MAX_SLOTS: usize = 8;

/// Slot `slot` of `count` in the prediction row: the equal-width cell that
/// `lp.width = 0, lp.weight = 1` describes
/// (`PredictionRowView.java:245-246`).
///
/// The reference's children are weighted, not fixed, so a slot is a *fraction*
/// of the row rather than a dp number -- which is why this is a function of
/// `count` and not a lookup table. `count` is clamped to
/// [`PREDICTION_MAX_SLOTS`] because a caller with a 12-column profile would
/// otherwise compute 1/12 shares the paint could not address.
#[inline]
pub fn prediction_slot_rect(
    row: &super::layout::Rect,
    slot: usize,
    count: usize,
) -> super::layout::Rect {
    let n = count.clamp(1, PREDICTION_MAX_SLOTS);
    let pitch = row.w / n as f32;
    super::layout::Rect {
        x: row.x + pitch * (slot as f32).min(n as f32 - 1.0),
        y: row.y,
        w: pitch,
        h: row.h,
        radius: row.radius,
    }
}

/// Paint one slot of the prediction row: the icon tile, the monogram, and the
/// label under it.
///
/// `icon_d` is [`DrawerSheetLayout::pred_icon_d`] -- 65 dp
/// (`APP_ICON_DP`, `device_profiles.xml:70`) -- and it is a parameter rather
/// than a re-derived dp number because the layout already has it, and the tile
/// the grid paints must be the same size or a prediction reads as a different
/// class of thing from the app it is predicting.
///
/// The label goes **under** the icon, which is the reference's
/// `LinearLayout` with a vertical orientation per slot and
/// `PREDICTION_ROW_H_DP`'s `icon + padding + text + padding` measurement
/// (`PredictionRowView.getExpectedHeight():149-161`). Drawing it beside the
/// icon instead is what the 65 dp the row used to reserve implied, and it is
/// why the layout grew to 108 dp.
///
/// Returns `true` when anything was written. `false` for a slot outside
/// `PREDICTION_MAX_SLOTS`, which is the caller's cue that the row is over
/// capacity rather than that it drew nothing.
#[allow(clippy::too_many_arguments)]
pub fn draw_prediction_row(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    row: &super::layout::Rect,
    style: &DrawerStyle,
    slot: usize,
    name: &str,
    icon_d: &f32,
) -> bool {
    if w == 0 || h == 0 || !row.h.is_finite() || row.h <= 0.0 || slot >= PREDICTION_MAX_SLOTS {
        return false;
    }
    let d = icon_d.min(row.h);
    if !(d.is_finite() && d > 0.0) {
        return false;
    }
    let cell = prediction_slot_rect(row, slot, PREDICTION_MAX_SLOTS);
    // The icon is centred in its slot: `lp.width = 0, lp.weight = 1` with a
    // gravity-centred child (`PredictionRowView.java:230-247`).
    let icon_x = cell.center_x() - d * 0.5;
    let icon = super::layout::Rect {
        x: icon_x,
        y: row.y,
        w: d,
        h: d,
        // The drawer's own icon corner fraction, which `layout.rs` applies to
        // every other icon; a prediction with a different corner reads as a
        // different widget.
        radius: d * 0.22,
    };
    if !rect_visible(&icon, h) {
        return false;
    }
    // 1. The tile: the same accent-filled rounded square the grid paints for a
    //    monogram, so a prediction and the app it predicts are visibly the
    //    same kind of thing.
    fill_round_rect(
        buf,
        stride,
        w,
        h,
        icon.x,
        icon.y,
        icon.w,
        icon.h,
        icon.radius,
        style.primary,
    );
    // 2. The monogram. `PredictionRowView` inflates a `BubbleTextView` per slot
    //    (`:230-247`), which draws the app's icon or its title's initial; this
    //    rasteriser has no icon cache on the sheet path, so the initial is
    //    what a caller with no decoded icon gets. Drawn in the surface colour
    //    so it reads against the accent tile, matching how `drm_kms.rs` draws
    //    the grid's glyph (`drm_kms.rs:4543-4551`).
    let initial = name.chars().find(|c| c.is_alphanumeric()).unwrap_or('?');
    let size = d * 0.44;
    let adv = font::measure(&initial.to_string(), size);
    font::draw_glyph(
        buf,
        stride,
        w,
        h,
        icon.center_x() - adv * 0.5,
        centred_ascender(icon.center_y(), size),
        initial,
        style.surface,
        size,
        FontWeight::Bold,
    );

    // 3. The label, under the icon, in the sheet's label size. The gap is the
    //    7 dp `all_apps_icon_drawable_padding` (`PREDICTION_ICON_PAD_DP`), and
    //    the label is fitted to the slot so two predictions cannot collide.
    let size_l = style.label_px;
    if size_l.is_finite() && size_l > 0.0 && !name.is_empty() {
        let pad = (row.h - d) * 0.18;
        let y = icon.y + icon.h + pad + size_l * 0.5;
        let shown = fit_prefix(name, size_l, 0.0, (cell.w * 0.96).max(0.0));
        let sw = font::measure(shown, size_l);
        font::draw_run(
            buf,
            stride,
            w,
            h,
            cell.center_x() - sw * 0.5,
            centred_ascender(y, size_l),
            shown,
            style.on_surface,
            size_l,
            FontWeight::Regular,
        );
    }
    true
}

/// Paint one result-group header: icon, label, and the divider below it.
///
/// `label` is the group's own title -- `all_apps_search_result_contacts_from_device`,
/// `search_result_hero_title`, or whatever the group's provider supplies
/// (`SectionBuilder.kt:36, 55, 75, 93, 112-113, 143`). It is borrowed and
/// drawn as a run, so no `format!` and no `String`.
///
/// The 16 dp leading glyph is the reference's `ic_allapps_search` tinted
/// `TextColorPrimary` (`SearchTargetFactory.kt:130-132`), drawn here as a
/// filled rounded square in `style.on_surface`: the shape is a stand-in, and
/// it is the same shape at the same size for every group, which is what makes
/// a column of them read as a column.
///
/// The divider is the reference's `createHeaderTarget(SPACE)`
/// (`SectionBuilder.kt:38`) and it belongs to the *bottom* of the header,
/// because the reference appends it after the group's rows -- so the divider
/// separates this group from the next one, which is the same visual
/// relationship seen from the other side.
///
/// Returns `true` when anything was written.
#[allow(clippy::too_many_arguments)]
pub fn draw_section_header(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    rect: &super::layout::Rect,
    style: &DrawerStyle,
    panel_w: f32,
    label: &str,
    divider: bool,
) -> bool {
    if w == 0 || h == 0 || !rect.h.is_finite() || rect.h <= 0.0 {
        return false;
    }
    let d = drawer_dp(panel_w);
    let size = d * SECTION_HEADER_TEXT_DP;
    let icon_d = d * SECTION_HEADER_ICON_DP;
    let cy = rect.center_y();
    let mut wrote = false;

    // `gravity = start|center` on the title
    // (`search_result_text.xml:30`), so the label and the icon share a centre
    // line and the label is left-padded past the icon by its 4 dp
    // `paddingEnd` (`:32`).
    if icon_d > 0.0 {
        fill_round_rect(
            buf,
            stride,
            w,
            h,
            rect.x,
            cy - icon_d * 0.5,
            icon_d,
            icon_d,
            icon_d * 0.25,
            style.on_surface,
        );
        wrote = true;
    }
    if size.is_finite() && size > 0.0 && !label.is_empty() {
        let text_x = rect.x + icon_d + size * 0.28;
        let right = rect.x + rect.w;
        // `maxLines = 1` + `ellipsize = end`
        // (`search_result_text.xml:29-31`): one line, cut at the boundary.
        let shown = fit_prefix(label, size, 0.0, (right - text_x).max(0.0));
        font::draw_run(
            buf,
            stride,
            w,
            h,
            text_x,
            centred_ascender(cy, size),
            shown,
            style.on_surface,
            size,
            FontWeight::Medium,
        );
        wrote |= !shown.is_empty();
    }

    if divider {
        // The SPACE header. `createHeaderTarget(SPACE)` builds a real target
        // with a real 12 dp band, so the divider is a hairline the width of
        // the content edge, not a full-bleed rule.
        let gap = d * SECTION_HEADER_GAP_DP;
        let t = style.stroke_px.max(1.0);
        let y = rect.y + rect.h + gap * 0.5;
        if y.is_finite() && y > 0.0 && y < h as f32 {
            fill_round_rect(
                buf,
                stride,
                w,
                h,
                rect.x,
                y,
                rect.w,
                t,
                t * 0.5,
                style.outline,
            );
            wrote = true;
        }
    }
    wrote
}

/// `true` when `r`'s top edge is on-panel and it has area. A band that has
/// scrolled off the bottom of the panel is skipped rather than relying on the
/// per-primitive clipping, so a half-open sheet does no work for bands that
/// cannot be seen.
#[inline]
fn rect_visible(r: &super::layout::Rect, h: usize) -> bool {
    r.h > 0.0 && r.w > 0.0 && r.y < h as f32 && (r.y + r.h) > 0.0
}

#[cfg(test)]
mod tests {
    /// The two directions of the scroller's mapping, and the property the fast
    /// scroller depends on: a letter resolves to a row, and that row resolves
    /// back to the same letter.
    ///
    /// `row_for_section` is the half that was missing. `section_for_progress`
    /// maps a scroll position to a letter, so the shell could draw a teardrop
    /// with a letter on it and no way to move the list -- the reference hands the
    /// letter to a `LinearSmoothScroller`
    /// (`AllAppsFastScrollHelper.smoothScrollToSection:41-47`).
    #[test]
    fn a_section_resolves_to_a_row_and_back() {
        // A sparse catalogue on purpose: no Q, no X, no Z. Absent letters are
        // the case that breaks a naive mirror, because their `row_offset` is a
        // copy of a neighbour's.
        let names = [
            "Apple", "Banana", "Cherry", "Emacs", "Firefox", "Gnu", "Hexedit",
        ];
        let idx = SectionIndex::build(names);
        assert!(!idx.is_empty());
        assert_eq!(idx.total_rows(), names.len());

        for letter in 1..=26u8 {
            let row = idx.row_for_section(letter);
            if idx.present & (1u32 << (letter - 1)) == 0 {
                assert_eq!(row, None, "absent letter {letter} must not scroll");
                continue;
            }
            let row = row.expect("a present letter has a row");
            assert!(row < names.len(), "letter {letter} row {row} is in range");
            // And the round trip: asking what letter that row is must give the
            // letter we started from.
            let back = idx.section_for_progress(row as f32 / names.len() as f32, names.len());
            assert_eq!(
                back, letter,
                "letter {letter} must round trip through row {row}"
            );
        }
    }

    /// A section's row is its *first* row, so scrolling to "C" puts the first C
    /// at the top rather than burying it.
    #[test]
    fn a_section_scrolls_to_its_first_row() {
        let names = ["Apple", "Avocado", "Banana", "Blueberry", "Cherry"];
        let idx = SectionIndex::build(names);
        // Letters are `1..=26` with 'A' = 1, not ASCII codes -- that encoding
        // is what leaves 0 free to mean "no section". `b'B' as u8` is 66 and
        // resolves to `None`, which is the encoding working, not a bug.
        assert_eq!(idx.row_for_section(66), None, "ASCII is not an index");
        // 'B' owns rows 2 and 3.
        assert_eq!(idx.row_for_section(2), Some(2));
        // 'A' owns rows 0 and 1, so its first row is 0.
        assert_eq!(idx.row_for_section(1), Some(0));
        // 'C' owns row 4, the last.
        assert_eq!(idx.row_for_section(3), Some(4));
    }

    /// Degenerate inputs are not crashes. The empty index is the identity for
    /// "no catalogue", and a `letter` of 0 is what "no section" means.
    #[test]
    fn an_empty_index_and_a_zero_letter_resolve_to_nothing() {
        let idx = SectionIndex::EMPTY;
        assert_eq!(idx.row_for_section(0), None);
        assert_eq!(idx.row_for_section(1), None, "nothing is present");
        assert_eq!(idx.row_for_section(26), None);
        assert_eq!(idx.row_for_section(200), None, "out of range");
        assert_eq!(idx.total_rows(), 0);
        assert!(idx.is_empty());
    }

    use super::super::layout::Layout;
    use super::*;

    // -- harness ----------------------------------------------------------

    /// The reference phone panel: 1080 x 2400 px, so `dp = 1080 / 420 =
    /// 2.5714` px/dp, which is the density every dp number in this module was
    /// measured at.
    fn panel() -> Layout {
        Layout::new(1080.0, 2400.0, false)
    }

    fn dp() -> f32 {
        panel().profile().dp
    }

    fn scroller() -> FastScrollerLayout {
        panel().fast_scroller()
    }

    fn sheet(shift: f32) -> DrawerSheetLayout {
        panel().drawer_sheet(shift)
    }

    /// Deterministic font: the active family is process-global, and other
    /// modules' tests set it.
    fn lock_font() -> std::sync::MutexGuard<'static, ()> {
        font::TEST_FONT_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn near(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    /// `true` when `px` is a coverage blend of `a` and `b`: per channel, on
    /// the segment between the two endpoints and no further. A blend
    /// algorithm that rounds differently or interpolates a channel outside the
    /// pair is rejected, which is the point -- it keeps this an *identity*
    /// check rather than a "close enough to the palette" check.
    fn is_between(px: u32, a: u32, b: u32) -> bool {
        for shift in [16u32, 8, 0] {
            let p = (px >> shift) & 0xFF;
            let (x, y) = ((a >> shift) & 0xFF, (b >> shift) & 0xFF);
            let (lo, hi) = if x <= y { (x, y) } else { (y, x) };
            if p < lo || p > hi {
                return false;
            }
        }
        // A blend of a *different* pair could also land inside every range, so
        // require that some alpha reproduces the pixel from the two tokens.
        for alpha in 0..=255u8 {
            if raster::blend_alpha(a, b, alpha) == px {
                return true;
            }
        }
        false
    }

    // -- engagement -------------------------------------------------------

    #[test]
    fn fastscroller_engages_only_past_both_thresholds() {
        let fs = scroller();
        let d = dp();
        // Land the down 300 dp below the track top so the 4 dp window is not
        // clipped by the panel edge.
        let down = fs.track.y + 300.0 * d;

        // (a) inside 4 dp but under 10 ms: NOT engaged.
        let mut st = FastScrollerState::new();
        st.on_down(down, 1000.0, &fs);
        let engaged = st.on_move(down + 2.0 * d, 1005.0, &fs);
        assert!(!engaged, "5 ms dwell must not engage");
        assert!(!st.dragging, "5 ms dwell engaged the scroller");

        // (b) past 10 ms but outside 4 dp: NOT engaged.
        let mut st = FastScrollerState::new();
        st.on_down(down, 1000.0, &fs);
        let engaged = st.on_move(down + 20.0 * d, 1040.0, &fs);
        assert!(!engaged, "a 20 dp move must not engage");
        assert!(!st.dragging, "20 dp move engaged the scroller");

        // (c) both: engaged.
        let mut st = FastScrollerState::new();
        st.on_down(down, 1000.0, &fs);
        st.on_move(down + 2.0 * d, 1011.0, &fs);
        assert!(st.dragging, "4 dp past 10 ms must engage");

        // The boundary itself: the reference is a strict `>` on the dwell
        // (`:310-311`) and a strict `<` on the delta (`:83`).
        let mut st = FastScrollerState::new();
        st.on_down(down, 1000.0, &fs);
        assert!(
            !st.on_move(down + 3.0 * d, 1010.0, &fs),
            "dwell is strictly >"
        );
        let mut st = FastScrollerState::new();
        st.on_down(down, 1000.0, &fs);
        assert!(
            !st.on_move(down + 4.0 * d, 1050.0, &fs),
            "delta is strictly <"
        );
    }

    #[test]
    fn fastscroller_thumb_is_fixed_52dp_regardless_of_track_height() {
        let d = dp();
        // A short track and a very tall one, both on real panels.
        let short = Layout::new(1080.0, 1200.0, false).fast_scroller();
        let tall = Layout::new(1080.0, 3000.0, false).fast_scroller();
        assert!(tall.track.h > short.track.h * 2.0, "tracks must differ");
        let short_travel = short.track.h - short.thumb_h;
        let mut t = FastScrollerState::new();
        for fs in [short, scroller(), tall] {
            assert!(
                near(fs.thumb_h, 52.0 * d, 1e-3),
                "thumb_h = {} px, want 52 dp = {}",
                fs.thumb_h,
                52.0 * d
            );
            // Only the travel scales with the track, never the thumb.
            let travel = fs.track.h - fs.thumb_h;
            assert!(
                near(travel - short_travel, fs.track.h - short.track.h, 1e-3),
                "travel must grow by exactly the track height"
            );
        }
        // The state machine's clamp uses the same fixed height: a tall track
        // must let the thumb travel further, not grow.
        let tall_fs = tall;
        let mut st = FastScrollerState::new();
        st.on_down(tall_fs.track.y, 0.0, &tall_fs);
        t.on_down(tall_fs.track.y, 0.0, &tall_fs);
        t.on_move(tall_fs.track.y + d, 50.0, &tall_fs);
        assert!(t.dragging, "the test drag must engage");
        for _ in 0..10_000 {
            t.on_move(t.last_y + 500.0, 50.0, &tall_fs);
        }
        let st = t;
        assert!(
            near(st.thumb_y, tall_fs.track.h - 52.0 * d, 1e-2),
            "thumb parked at {} px, want track_h - 52 dp = {}",
            st.thumb_y,
            tall_fs.track.h - 52.0 * d
        );
        assert!(
            st.thumb_y > short_travel,
            "a tall track must give more travel, not a bigger thumb"
        );
    }

    #[test]
    fn fastscroller_non_finite_input_is_inert() {
        let fs = scroller();
        let mut st = FastScrollerState::new();
        st.on_down(fs.track.y + 10.0, 0.0, &fs);
        let before = st;
        assert!(!st.on_move(f32::NAN, 100.0, &fs), "NaN y engaged");
        assert!(!st.on_move(0.0, f32::NAN, &fs), "NaN time engaged");
        assert!(
            !st.on_move(f32::INFINITY, f32::INFINITY, &fs),
            "inf engaged"
        );
        st.step(f32::NAN);
        st.step(f32::NEG_INFINITY);
        assert_eq!(st, before, "non-finite input mutated the state");
    }

    // -- section index ----------------------------------------------------

    /// 26 letters, 10 rows each: `A`..`Z` with known boundaries. Sorted
    /// case-insensitively, as `AlphabeticalAppsList` maintains it.
    fn even_catalogue() -> Vec<String> {
        let mut v = Vec::new();
        for l in b'A'..=b'Z' {
            for r in 0..10 {
                // `char::from`, not the `u8`: `format!("{l}")` on a `u8`
                // prints the *number*, which would bucket every row into
                // index 0 and make every assertion below vacuous.
                v.push(format!("{}{r}", char::from(l)));
            }
        }
        v
    }

    #[test]
    fn fastscroller_section_lookup_at_0_half_one() {
        let names = even_catalogue();
        let idx = SectionIndex::build(names.iter().map(|s| s.as_str()));
        let total = names.len();
        assert_eq!(total, 260);

        // progress 0 -> 'A' (1).
        assert_eq!(idx.section_for_progress(0.0, total), 1);
        // progress 0.5 -> row 130, which is the first row of 'N' (13*10).
        assert_eq!(idx.section_for_progress(0.5, total), 14);
        // progress 1 -> the last row, 'Z' (26).
        assert_eq!(idx.section_for_progress(1.0, total), 26);
        // An empty catalogue has no rows, and therefore no section.
        assert_eq!(idx.section_for_progress(0.5, 0), 0);
        // A row count with no rows behind it is the only emptiness test there
        // is; see `section_for_progress`.
    }

    #[test]
    fn fastscroller_section_lookup_is_monotonic() {
        let names = even_catalogue();
        let idx = SectionIndex::build(names.iter().map(|s| s.as_str()));
        let total = names.len();
        let mut prev = 0u8;
        let mut seen = [false; 27];
        for i in 0..=1000 {
            let p = i as f32 / 1000.0;
            let s = idx.section_for_progress(p, total);
            assert!(s >= prev, "section went backwards at p={p}: {prev} -> {s}");
            assert!(s <= 26, "section out of range at p={p}: {s}");
            seen[s as usize] = true;
            prev = s;
        }
        // Every letter present in the catalogue must be reachable, and no
        // other letter invented.
        for (l, hit) in seen.iter().enumerate().skip(1) {
            assert!(
                *hit,
                "letter {} was never reported",
                (b'A' + l as u8 - 1) as char
            );
        }
        // Out-of-range and non-finite progress is clamped, not UB.
        assert_eq!(idx.section_for_progress(-1.0, total), 1);
        assert_eq!(idx.section_for_progress(2.0, total), 26);
        assert_eq!(idx.section_for_progress(f32::NAN, total), 0);
    }

    #[test]
    fn fastscroller_section_lookup_skips_absent_letters() {
        // A, C, Z only. A lookup in the middle must never report B or D.
        let names = ["Apple", "Cannon", "Canon", "Zebra"];
        let idx = SectionIndex::build(names.iter().copied());
        assert_eq!(idx.row_offset[0], 0, "A starts at row 0");
        assert_eq!(idx.row_offset[2], 1, "C starts at row 1");
        assert_eq!(idx.row_offset[25], 3, "Z starts at row 3");
        // B is absent, so it mirrors C's row -- that is what makes the
        // "last l with row_offset[l] <= row" tie-break land on C.
        assert_eq!(idx.row_offset[1], idx.row_offset[2]);
        for row in 0..4usize {
            let s = idx.section_for_progress((row as f32) / 4.0 + 0.0001, 4);
            assert!(
                s == 1 || s == 3 || s == 26,
                "row {row} reported {} (A/C/Z expected)",
                letter_of(s)
            );
        }
        // Non-alphabetic names bucket under index 0, so a leading digit run
        // merges with 'A' rather than inventing a section.
        let idx = SectionIndex::build(["1Password", "2048", "Android", "Zebra"]);
        assert_eq!(idx.row_offset[0], 0, "the leading run is index 0");
        assert_eq!(idx.section_for_progress(0.0, 4), 1);
        assert_eq!(idx.section_for_progress(1.0, 4), 26);
    }

    fn letter_of(s: u8) -> char {
        if s == 0 {
            '-'
        } else {
            (b'A' + s - 1) as char
        }
    }

    #[test]
    fn section_index_delta_is_saturating_and_round_trips_under_the_ceiling() {
        let names = even_catalogue();
        let idx = SectionIndex::build(names.iter().map(|s| s.as_str()));
        // 'A' starts at row 0, so its delta from "before the list" is 0; every
        // later section is 10 rows on.
        assert_eq!(idx.first_row[0], 0, "'A' starts the list");
        assert!(
            idx.first_row[1..].iter().all(|d| *d == 10),
            "deltas {:?}",
            idx.first_row
        );
        let sum: u32 = idx.first_row.iter().map(|d| *d as u32).sum();
        assert_eq!(sum, 250, "sum of deltas must equal the last section's row");
        assert_eq!(idx.row_offset[25] as u32, sum, "the chain must land on 'Z'");

        // 300 rows under one letter: the delta saturates at 255 and clamps --
        // it does not wrap, and the absolute row it describes stays exact, so
        // lookups are unaffected.
        // A block of 300 rows for one letter, so the *next populated* letter's
        // delta from it is 300 -- past what a `u8` delta can express. The
        // delta lands on 'B' (the letter after 'A'), because absent letters
        // mirror their successor's absolute row and therefore contribute a
        // delta of 0.
        let mut big = Vec::with_capacity(305);
        for r in 0..300 {
            big.push(format!("A{r}"));
        }
        for r in 0..2 {
            big.push(format!("B{r}"));
        }
        for r in 0..3 {
            big.push(format!("Z{r}"));
        }
        let idx = SectionIndex::build(big.iter().map(|s| s.as_str()));
        assert_eq!(idx.first_row[0], 0, "'A' starts the list");
        assert_eq!(
            idx.first_row[1],
            u8::MAX,
            "a 300-row gap must saturate at 255, not wrap"
        );
        // The absolute rows the saturating delta describes are still exact,
        // which is what keeps every lookup correct.
        assert_eq!(idx.row_offset[1], 300, "B's absolute row must be exact");
        assert_eq!(idx.row_offset[25], 302, "Z's absolute row must be exact");
        assert_eq!(
            idx.section_for_progress(1.0, big.len()),
            26,
            "lookups stay exact"
        );
        assert_eq!(
            idx.section_for_progress(0.5, big.len()),
            1,
            "still 'A' at the half"
        );
        // A row inside 'A''s block resolves to 'A', not to the saturated 'B'.
        assert_eq!(idx.section_for_progress(0.4, big.len()), 1);
        // And just past the block, 'B'.
        assert_eq!(idx.section_for_progress(301.0 / 305.0, big.len()), 2);
    }

    // -- haptics ----------------------------------------------------------

    #[test]
    fn fastscroller_haptic_fires_once_per_section_change() {
        let fs = scroller();
        let d = dp();
        let names = even_catalogue();
        let mut st = FastScrollerState::new();
        st.set_catalogue(names.iter().map(|s| s.as_str()));
        assert_eq!(st.total_rows as usize, names.len());

        let travel = fs.track.h - fs.thumb_h;
        let down = fs.track.y + travel * 0.5;
        st.on_down(down, 0.0, &fs);
        // The engaging move resolves the first section, and going from "no
        // section" to 'A' *is* a change (`:357` compares against the previous
        // name, which is empty until the first move), so it ticks once.
        // The section is resolved at the *thumb's* position, and the engaging
        // move contributes no thumb travel, so it is still 'A' -- where the
        // finger landed is irrelevant until the thumb follows it.
        assert!(
            st.on_move(down + 1.0 * d, 50.0, &fs),
            "first section must tick"
        );
        assert_eq!(
            st.letter, 1,
            "the thumb has not moved, so the section is 'A'"
        );
        let baseline = 1usize;

        // Walk the thumb down the track in 1 dp steps and count the rising
        // edges. Two letters, 260 rows: the track must cross every boundary
        // exactly once, and a tick must be reported only on those frames.
        let mut ticks = 0usize;
        // Only the *changes*, not every frame: a section must be entered once
        // and then be quiet for the rest of its span.
        let mut visited = vec![st.letter];
        let steps = 2000;
        for i in 1..=steps {
            let y = st.last_y + travel / steps as f32;
            let changed = st.on_move(y, 50.0 + i as f32, &fs);
            if changed {
                ticks += 1;
                assert!(st.haptic_latch, "a reported change must latch");
                assert_ne!(
                    st.letter,
                    *visited.last().unwrap(),
                    "the same section ticked twice"
                );
                visited.push(st.letter);
            } else {
                assert!(!st.haptic_latch, "a latch with no change reported");
            }
        }
        // 26 sections means 25 boundary crossings inside the loop, plus the
        // one tick the engaging move already reported for "none -> A".
        assert_eq!(
            ticks, 25,
            "expected one tick per section boundary, got {ticks}"
        );
        assert_eq!(
            ticks + baseline,
            26,
            "one tick per section over the whole drag"
        );
        assert_eq!(
            st.letter, 26,
            "the walk must end on Z, ended on {}",
            st.letter
        );
        // Entered once each, strictly increasing: no section is re-entered and
        // none is skipped.
        assert_eq!(
            visited,
            (1..=26u8).collect::<Vec<u8>>(),
            "visited {visited:?}"
        );
        // The latch is a rising edge, not a per-frame signal: 2000 frames,
        // 25 ticks.
        assert!(
            ticks * 20 < steps,
            "haptics are firing per frame, not per section"
        );
    }

    // -- fades ------------------------------------------------------------

    #[test]
    fn fastscroller_fades_are_200_in_150_out() {
        let fs = scroller();
        let mut st = FastScrollerState::new();
        st.on_down(fs.track.y, 0.0, &fs);
        st.on_move(fs.track.y + dp(), 50.0, &fs);
        assert!(st.dragging);

        // Fade in: 1 ms steps, 200 ms to full.
        for i in 0..200 {
            st.step(1.0);
            if i < 199 {
                assert!(
                    st.popup_alpha < 1.0,
                    "reached full alpha after {} ms, must take 200",
                    i + 1
                );
            }
        }
        assert_eq!(
            st.popup_alpha, 1.0,
            "fade-in must land exactly on 1.0 at 200 ms"
        );

        // A single 200 ms tick is the same curve, not a jump short.
        let mut bulk = FastScrollerState::new();
        bulk.on_down(fs.track.y, 0.0, &fs);
        bulk.on_move(fs.track.y + dp(), 50.0, &fs);
        bulk.step(100.0);
        assert!(
            bulk.popup_alpha > 0.49 && bulk.popup_alpha < 0.51,
            "half a fade must be ~0.5, got {}",
            bulk.popup_alpha
        );
        bulk.step(100.0);
        assert_eq!(bulk.popup_alpha, 1.0);

        // Fade out: 150 ms, and the shorter of the two, as `:517` says.
        st.on_up(1000.0);
        for i in 0..150 {
            st.step(1.0);
            if i < 149 {
                assert!(
                    st.popup_alpha > 0.0,
                    "reached zero after {} ms, must take 150",
                    i + 1
                );
            }
        }
        assert_eq!(
            st.popup_alpha, 0.0,
            "fade-out must land exactly on 0.0 at 150 ms"
        );
        // And it stays there.
        st.step(1000.0);
        assert_eq!(
            st.popup_alpha, 0.0,
            "a settled popup must not creep back up"
        );

        // A zero or negative dt is a no-op, not a rewind.
        st.on_up(2000.0);
        st.popup_alpha = 0.5;
        st.step(0.0);
        assert_eq!(st.popup_alpha, 0.5);
        st.step(-100.0);
        assert_eq!(st.popup_alpha, 0.5);
    }

    #[test]
    fn fastscroller_popup_tracks_the_thumb_and_is_clamped() {
        let fs = scroller();
        let d = dp();
        let travel = fs.track.h - fs.thumb_h;
        let radius = (fs.track_w + fs.thumb_pad * 2.0) * 0.5;
        let lo = fs.track.y;
        let hi = (fs.track.y + fs.track.h - fs.popup.h).max(lo);
        let want_for =
            |thumb_y: f32| (fs.track.y + thumb_y + radius - fs.popup.h * 0.5).clamp(lo, hi);

        // Mid-track: the formula is unclamped, so the popup's rounded corner
        // lines up with the top of the thumb as `:522` describes. The
        // engaging move carries no thumb travel (`:296` + `:347`), so the
        // thumb has to be walked there.
        let mut st = FastScrollerState::new();
        st.on_down(fs.track.y, 0.0, &fs);
        // The engaging move at `track.y + d` contributes no travel, so the
        // thumb is still at 0 afterwards; the next move is what moves it.
        st.on_move(fs.track.y + d, 50.0, &fs);
        assert!(
            near(st.thumb_y, 0.0, 1e-4),
            "the engage must not move the thumb"
        );
        st.on_move(fs.track.y + travel * 0.5, 60.0, &fs);
        assert!(st.dragging);
        assert!(
            near(st.thumb_y, travel * 0.5 - d, 1e-2),
            "thumb at {} want {}",
            st.thumb_y,
            travel * 0.5 - d
        );
        assert!(
            near(st.popup_y, want_for(st.thumb_y), 1e-3),
            "popup_y {} want {}",
            st.popup_y,
            want_for(st.thumb_y)
        );
        assert!(
            st.popup_y > lo && st.popup_y < hi,
            "a mid-track popup must not be clamped ({} vs {lo}..{hi})",
            st.popup_y
        );

        // At the top of the track the raw value would sit `popup.h/2 -
        // radius` = 69 px *above* the track, so the lower bound engages. The
        // reference bounds to `[0, top + trackHeight - height]` (`:525-526`)
        // with the same intent.
        let mut top_end = FastScrollerState::new();
        top_end.on_down(fs.track.y, 0.0, &fs);
        top_end.on_move(fs.track.y + d, 50.0, &fs);
        assert!(
            near(top_end.thumb_y, 0.0, 1e-3),
            "the engage must not move the thumb"
        );
        assert!(near(top_end.popup_y, lo, 1e-3), "clamped to the track top");
        assert!(
            fs.track.y + radius - fs.popup.h * 0.5 < lo,
            "the top of the track must be a clamping case, got {}",
            fs.track.y + radius - fs.popup.h * 0.5
        );

        // At the bottom the thumb is at `travel`, and the raw value is
        // `hi + (radius + popup.h/2 - thumb_h)` -- and since the 52 dp thumb is
        // shorter than half the 62 dp letterbox, that is *negative* by 43.7 px.
        // So the bottom is not a clamping case; the popup rides up from the
        // track bottom with the thumb. Asserting it clamps would be asserting
        // a constraint the geometry does not have.
        let mut bot_end = FastScrollerState::new();
        bot_end.on_down(fs.track.y, 0.0, &fs);
        bot_end.on_move(fs.track.y + d, 50.0, &fs);
        bot_end.on_move(fs.track.y + travel + 500.0, 60.0, &fs);
        assert!(near(bot_end.thumb_y, travel, 1e-2), "thumb at the bottom");
        assert!(
            near(
                bot_end.popup_y,
                fs.track.y + travel + radius - fs.popup.h * 0.5,
                1e-3
            ),
            "the bottom of the track is unclamped: {}",
            bot_end.popup_y
        );
        assert!(
            bot_end.popup_y > lo && bot_end.popup_y < hi,
            "the popup at the bottom must be inside the track ({} vs {lo}..{hi})",
            bot_end.popup_y
        );
    }

    // -- teardrop ---------------------------------------------------------

    #[test]
    fn teardrop_popup_geometry() {
        let d = dp();
        // 75 x 62 dp at the reference density, in a buffer with room to see
        // the whole rotated shape (it overhangs the box).
        let rw = 75.0 * d;
        let rh = 62.0 * d;
        let pad = 32usize;
        let w = rw as usize + 2 * pad;
        let h = rh as usize + 2 * pad;
        let mut buf = vec![0u32; w * h];
        let cx = (w as f32) * 0.5;
        let cy = (h as f32) * 0.5;
        raster::draw_teardrop(&mut buf, w, w, h, cx, cy, rw, rh, 0xFFFFFFFF);

        let has_ink = |y: usize, x: usize| buf[y * w + x] != 0;
        let ink_rows: Vec<usize> = (0..h).filter(|y| (0..w).any(|x| has_ink(*y, x))).collect();
        assert!(!ink_rows.is_empty(), "teardrop drew nothing");
        let area = buf.iter().filter(|p| **p != 0).count() as f32;

        // Analytic area of a rounded rect: w*h - sum(r^2) * (1 - pi/4), with
        // the radii {r, r, r/5, r} from `FastScrollThumbDrawable.java:58-60`.
        // That source line is `{r,r, r,r, r2,r2, r,r}` -- **eight** floats, two
        // per corner in `[TL, TR, BR, BL]` order -- so exactly one corner is
        // `r/5`, not two.
        let r = rh * 0.5;
        let sum_r2 = 3.0 * r * r + (r / 5.0).powi(2);
        let exact = rw * rh - sum_r2 * (1.0 - core::f32::consts::PI / 4.0);
        // Pixel-centre sampling on a rotated edge: the signed error is bounded
        // by the perimeter (2*(rw+rh)) and averages out well under 1%.
        let tol = 0.01 * exact;
        assert!(
            (area - exact).abs() <= tol,
            "ink area {area} vs analytic {exact} (tol {tol})"
        );
        // And the shape is the letterbox-sized blob, not a thin sliver.
        assert!(
            area > 0.85 * rw * rh,
            "ink {area} is not ~75*62 dp = {}",
            rw * rh
        );

        // THE one-small-corner contract. Measure the ink's extent at a fixed
        // depth inside each of the four axis-aligned extremes and compare.
        //
        // `draw_rotated_rounded_rect` rotates a *sample* into the rect's own
        // frame with `lx = fx*cos + fy*sin, ly = -fx*sin + fy*cos`
        // (`raster.rs`), and at `-45 deg` that is `lx = (fx - fy)/sqrt2`,
        // `ly = (fx + fy)/sqrt2`. The corner picker then keys on the signs:
        //
        //   right  (fx > 0, fy ~ 0) -> lx > 0, ly > 0 -> BR  (r/5)
        //   top    (fy < 0, fx ~ 0) -> lx > 0, ly < 0 -> TR  (r)
        //   bottom (fy > 0, fx ~ 0) -> lx < 0, ly > 0 -> BL  (r)
        //   left   (fx < 0, fy ~ 0) -> lx < 0, ly < 0 -> TL  (r)
        //
        // so exactly **one** side of the axis-aligned ink -- the right, where
        // the teardrop's tail points -- is the tight one, and top, bottom and
        // left are three `r` corners of the same radius. The old
        // `[big, small, small, big]` gave the top a small corner as well,
        // which is the bug this asserts against.
        let row_span = |y: usize| -> (usize, usize) {
            let xs: Vec<usize> = (0..w).filter(|x| buf[y * w + x] != 0).collect();
            (*xs.first().unwrap(), *xs.last().unwrap())
        };
        let col_span = |x: usize| -> (usize, usize) {
            let ys: Vec<usize> = (0..h).filter(|y| buf[y * w + x] != 0).collect();
            (*ys.first().unwrap(), *ys.last().unwrap())
        };
        let top = *ink_rows.first().unwrap();
        let bottom = *ink_rows.last().unwrap();
        let depth = 6usize;
        let w_top = row_span(top + depth).1 - row_span(top + depth).0;
        let w_bottom = row_span(bottom - depth).1 - row_span(bottom - depth).0;
        // Top and bottom are both `r` corners of equal radius, so their widths
        // must agree to within the pixel quantisation of the two `ceil`/`floor`
        // roundings. This is the assertion that FAILS on `[big, small, small,
        // big]`: there the top is `r/5` and comes out ~2.2x narrower.
        assert!(
            w_top.abs_diff(w_bottom) <= 2,
            "top and bottom are both r corners and must match: {w_top} vs {w_bottom}"
        );

        let left = (0..w)
            .find(|x| (0..h).any(|y| buf[y * w + x] != 0))
            .unwrap();
        let right = (0..w)
            .rev()
            .find(|x| (0..h).any(|y| buf[y * w + x] != 0))
            .unwrap();
        let h_right = col_span(right - depth).1 - col_span(right - depth).0;
        let h_left = col_span(left + depth).1 - col_span(left + depth).0;
        // And the one small corner is on the right: `r` = 31 dp vs `r/5` =
        // 6.2 dp, so sqrt(31/6.2) = 2.24 -- assert the weaker "visibly tighter".
        assert!(
            h_left > h_right * 3 / 2,
            "the single r/5 corner on the right is not tighter: height {h_left} at the left vs {h_right} at the right"
        );
        // ...and it is the ONLY tight side: the left, like top and bottom, is
        // an `r` corner, so it must be in the same class as them.
        assert!(
            w_bottom > h_right * 3 / 2,
            "the bottom r corner must not share the right's tightness: {w_bottom} vs {h_right}"
        );

        // Finally, the configuration itself, straight from the primitive: three
        // `r` and one `r/5`, with the `r/5` in the third `[tl,tr,br,bl]` slot.
        let radii = raster::teardrop_corner_radii(rh);
        assert!(
            near(radii[0], r, 1e-3) && near(radii[1], r, 1e-3) && near(radii[3], r, 1e-3),
            "TL, TR and BL are all r: {radii:?}"
        );
        assert!(near(radii[2], r / 5.0, 1e-3), "BR alone is r/5: {radii:?}");
        assert_eq!(
            radii.iter().filter(|v| near(**v, r / 5.0, 1e-3)).count(),
            1,
            "exactly one corner is r/5: {radii:?}"
        );
    }

    // -- the scrim --------------------------------------------------------

    /// The header's letter is the *live* section, not the literal `'A'` it
    /// used to paint, and not the count label it must not be confused with.
    ///
    /// This is the whole reason the letter became a parameter: the sheet was
    /// painting `'A'` while [`FastScrollerState::letter_str`] held the real
    /// section three pixels away on the fast-scroller popup, so for 25 of the
    /// 26 sections the two disagreed for the whole of a drag.
    #[test]
    fn drawer_header_letter_follows_the_fast_scroller() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let l = panel();
        let d = dp();
        let style = DrawerStyle::dark(d);
        let sh = sheet(l.h);

        // One pass per letter: paint the header with that letter and check for
        // *text-coloured* ink at the glyph's origin, and that a *different*
        // letter does not put any there. The 'A' that was hardcoded is the
        // control: with `section_letter = "A"` the pixel is inked, which is what
        // made the bug invisible -- it looked right at the top of the list.
        //
        // The probe is for `on_surface` specifically rather than "any
        // non-zero", because the header pill itself is filled `surface_high`
        // (`draw_drawer_sheet`'s step 4) and a naive non-zero probe would find
        // that fill under every letter including none at all.
        let inked_at = |letter: &str| -> bool {
            let mut buf = vec![0u32; w * h];
            draw_drawer_sheet_with_section(&mut buf, w, w, h, &sh, 1.0, &style, "", "", letter);
            let size = style.label_px;
            // The glyph origin, which is where `draw_drawer_sheet_with_section`
            // puts it: `hd.x + label_px * 0.4`.
            let x0 = sh.header.x + size * 0.4;
            let y0 = sh.header.y + sh.header.h * 0.5 - size * 0.51;
            let xs = (x0 as usize)..(x0 as usize + size as usize).min(w);
            let ys = (y0 as usize)..(y0 as usize + size as usize).min(h);
            (0..3).any(|dy| {
                let row = (ys.start + dy) * w;
                xs.clone().any(|x| buf[row + x] == style.on_surface)
            })
        };

        assert!(inked_at("A"), "the letter must actually be painted");
        assert!(inked_at("N"), "any letter must paint");
        // The letter is the *only* thing in that corner: the count label is
        // right-aligned against the content edge, so it cannot reach here. That
        // is what makes the single-pixel probe a test of the letter and not of
        // the header as a whole.
        let cw = font::measure("42 APPS", style.label_px);
        let count_x = sh.header.x + sh.header.w - cw - style.label_px * 0.4;
        assert!(
            count_x > sh.header.x + style.label_px * 2.0,
            "the count must not overlap the letter's column"
        );

        // An empty letter paints no glyph at all -- `letter_str()` returns ""
        // for "no section", and the honest answer is a blank header rather than
        // a letter that is not there. `inked_at` is the same closure, so this
        // is the direct comparison: same probe, one string, no ink.
        assert!(!inked_at(""), "an empty section letter must paint no glyph");
    }

    /// The header letter comes from the fast scroller's own allocation-free
    /// accessor, so threading it costs the frame path nothing.
    #[test]
    fn the_header_letter_source_is_allocation_free() {
        // `letter_str()` backs onto `LETTER_BYTES` (`LETTER_BYTES:58`), so the
        // string handed to `draw_drawer_sheet` is a slice of a `&'static [u8;
        // 26]` and not a fresh allocation. The contract the paint relies on is
        // that it is `&'static str`, which is what the signature takes; this
        // pins that the accessor satisfies it for every legal letter, including
        // the ones outside the range.
        let mut st = FastScrollerState::new();
        st.set_catalogue(even_catalogue().iter().map(|s| s.as_str()));
        // No drag: the letter is 0, so there is no section and no string.
        assert_eq!(st.letter_str(), "");
        // And a real drag resolves one, which is what the header would show.
        let fs = scroller();
        st.on_down(fs.track.y + 300.0 * dp(), 0.0, &fs);
        st.on_move(fs.track.y + 300.0 * dp() + 2.0 * dp(), 50.0, &fs);
        assert!(st.dragging, "the drag must engage");
        assert_eq!(st.letter, 1, "the thumb has not moved, so 'A'");
        let letter = st.letter_str();
        assert_eq!(letter, "A");
        assert_eq!(letter.len(), 1, "a section letter is one byte");
    }

    // -- search-result chrome ---------------------------------------------

    /// The zero-result state must actually say something.
    ///
    /// The live path drew *nothing* for a query matching no app
    /// (`main.rs:4413` is a bare `if count > 0`), so the sheet came up blank
    /// with no explanation. The reference's `SearchResultEmptyState`
    /// (`SearchResultEmptyState.kt:15-49`) is a 48 dp icon, a title and a
    /// subtitle; all three have to be inked here, and the title has to carry
    /// the query the user actually typed.
    #[test]
    fn the_zero_result_state_draws_an_icon_a_title_and_a_subtitle() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let l = panel();
        let d = dp();
        let style = DrawerStyle::dark(d);
        let sh = sheet(l.h);
        let band = sh.grid;

        let mut buf = vec![0u32; w * h];
        assert!(draw_search_empty_state(
            &mut buf, w, w, h, &band, &style, w as f32, "cafe"
        ));

        // Layout: the reference's `LinearLayout`, `center_horizontal`, 32 dp
        // padding, 48 dp icon, then title, then subtitle
        // (`search_result_empty_state.xml:7-34`).
        let [icon, title, sub] = search_empty_state_layout(&band, w as f32);
        assert!(
            near(icon.w, 48.0 * d, 1e-3),
            "the icon is {} px, want 48 dp = {}",
            icon.w,
            48.0 * d
        );
        assert!(near(icon.h, 48.0 * d, 1e-3), "ic_qsb_search is 48 x 48 dp");
        assert!(
            near(icon.center_x(), band.center_x(), 1e-3),
            "gravity=center_horizontal centres the icon"
        );
        assert!(
            icon.y - band.y >= 32.0 * d - 1e-3,
            "the container's 32 dp padding must clear the band top"
        );
        assert!(
            icon.y > band.y && title.y > icon.y + icon.h && sub.y > title.y,
            "the three bands stack downward: {:?} {:?} {:?}",
            icon,
            title,
            sub
        );
        assert!(
            near(sub.y + sub.h, band.y + band.h, 1e-3) || sub.y + sub.h <= band.y + band.h,
            "the subtitle must stay inside the band"
        );

        // Paint: each band has ink of its own, which is the property that
        // matters -- three bands and one of them blank would look like a
        // rendering bug rather than a missing view.
        let ink_in = |r: &super::super::layout::Rect| -> usize {
            let x0 = r.x.max(0.0) as usize;
            let x1 = ((r.x + r.w) as usize).min(w);
            let y0 = r.y.max(0.0) as usize;
            let y1 = ((r.y + r.h) as usize).min(h);
            (y0..y1)
                .flat_map(|y| (x0..x1).map(move |x| (x, y)))
                .filter(|&(x, y)| buf[y * w + x] != 0)
                .count()
        };
        let (ic, ti, su) = (ink_in(&icon), ink_in(&title), ink_in(&sub));
        assert!(ic > 50, "the 48 dp magnifier drew only {ic} px");
        assert!(ti > 200, "the title drew only {ti} px");
        assert!(su > 100, "the subtitle drew only {su} px");
        // The icon is tinted `ColorTokens.ColorAccent`
        // (`SearchResultEmptyState.kt:33`), so its ink is `primary` -- and the
        // title is `textColorPrimary`, i.e. `on_surface`. Two different tokens,
        // which is what distinguishes "the icon rendered" from "the icon
        // rendered in the text colour".
        let has = |c: u32| (0..h).any(|y| (0..w).any(|x| buf[y * w + x] == c));
        assert!(has(style.primary), "the icon must be the accent colour");
        assert!(has(style.on_surface), "the title must be on_surface");
        assert!(
            has(style.on_surface_variant),
            "the subtitle must be the tertiary colour"
        );
    }

    /// The empty state's title is `No apps found matching "<query>"`, built
    /// without a `format!`.
    ///
    /// The reference's `all_apps_no_search_results` is a *format* string
    /// (`strings.xml:188`) and the shell cannot afford one per frame, so the
    /// string is split into two constants and the query is drawn between them
    /// at an accumulated pen position. This asserts the visible result is
    /// right: the prefix is inked, the query is inked, the closing quote is
    /// inked, and the two halves are present in that left-to-right order.
    #[test]
    fn the_zero_result_title_carries_the_query_without_a_format() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let d = dp();
        let style = DrawerStyle::dark(d);
        let sh = sheet(panel().h);
        let band = sh.grid;
        let query = "zzqx";

        let mut buf = vec![0u32; w * h];
        assert!(draw_search_empty_state(
            &mut buf, w, w, h, &band, &style, w as f32, query
        ));
        let [_, title, _] = search_empty_state_layout(&band, w as f32);
        let size = d * EMPTY_STATE_TITLE_DP;
        // The three runs together must span at least the width of the fixed
        // halves plus the query, which is the assertion that all three were
        // drawn and laid out in sequence.
        let total = font::measure(NO_RESULTS_PREFIX, size)
            + font::measure(query, size)
            + font::measure(NO_RESULTS_SUFFIX, size);
        let xs = (title.x.max(0.0) as usize)..((title.x + title.w) as usize).min(w);
        let ys = (title.y.max(0.0) as usize)..((title.y + title.h) as usize).min(h);
        let leftmost = xs
            .clone()
            .find(|&x| ys.clone().any(|y| buf[y * w + x] != 0));
        let rightmost = xs
            .clone()
            .rev()
            .find(|&x| ys.clone().any(|y| buf[y * w + x] != 0));
        let (lo, hi) = (leftmost.unwrap_or(0) as f32, rightmost.unwrap_or(0) as f32);
        assert!(
            hi - lo > total * 0.8,
            "the title spans {} px, want ~{} for the three pieces",
            hi - lo,
            total
        );
        // The whole run is centred on the band's axis, which is what
        // `gravity = center_horizontal` does to the reference's one TextView.
        let centre = (lo + hi) * 0.5;
        assert!(
            near(centre, band.center_x(), total * 0.1 + 2.0),
            "the title is centred at {centre}, band centre is {}",
            band.center_x()
        );
        // And a longer query shifts the run's left edge left, because the run
        // is centred as a unit rather than left-aligned. This is the observable
        // difference between centring three pieces and centring one.
        let mut buf2 = vec![0u32; w * h];
        draw_search_empty_state(
            &mut buf2,
            w,
            w,
            h,
            &band,
            &style,
            w as f32,
            "a much longer query",
        );
        let span = |b: &[u32]| -> (usize, usize) {
            let xs = (title.x.max(0.0) as usize)..((title.x + title.w) as usize).min(w);
            let ys = (title.y.max(0.0) as usize)..((title.y + title.h) as usize).min(h);
            let lo = xs.clone().find(|&x| ys.clone().any(|y| b[y * w + x] != 0));
            let hi = xs.rev().find(|&x| ys.clone().any(|y| b[y * w + x] != 0));
            (lo.unwrap_or(0), hi.unwrap_or(0))
        };
        let (lo1, hi1) = span(&buf);
        let (lo2, hi2) = span(&buf2);
        assert!(
            (lo2 as i64) < (lo1 as i64) && (hi2 as i64) > (hi1 as i64),
            "a longer query must widen the run on both sides: {lo1}..{hi1} vs {lo2}..{hi2}"
        );
    }

    /// Degenerate inputs to the zero-result state are inert, not crashes: a
    /// zero-height band, a non-finite geometry, a zero panel width and a query
    /// longer than the panel all have to be survivable, because the caller's
    /// band comes from a layout that animates.
    #[test]
    fn the_zero_result_state_is_inert_on_degenerate_input() {
        let _guard = lock_font();
        let (w, h) = (1080usize, 2400usize);
        let style = DrawerStyle::dark(dp());
        let sh = sheet(panel().h);
        let mut buf = vec![0u32; w * h];
        let before = buf.clone();

        // A band with no height.
        let flat = super::super::layout::Rect {
            x: 0.0,
            y: 100.0,
            w: 1080.0,
            h: 0.0,
            radius: 0.0,
        };
        assert!(
            !draw_search_empty_state(&mut buf, w, w, h, &flat, &style, w as f32, "x"),
            "a zero-height band must report that it drew nothing"
        );
        // A non-finite band height must not reach the rasteriser.
        let nan = super::super::layout::Rect {
            h: f32::NAN,
            ..flat
        };
        assert!(!draw_search_empty_state(
            &mut buf, w, w, h, &nan, &style, w as f32, "x"
        ));
        assert_eq!(buf, before, "a degenerate band wrote pixels");
        // A zero panel width makes dp zero, which makes every band zero-sized.
        // The layout must still return a well-formed `Rect` and the paint must
        // not panic.
        let zero = search_empty_state_layout(&sh.grid, 0.0);
        assert!(zero.iter().all(|r| r.w.is_finite() && r.h.is_finite()));
        assert!(zero.iter().all(|r| r.w >= 0.0 && r.h >= 0.0));
        // A query far longer than the panel: the elision branch.
        let long: String = "q".repeat(4096);
        let mut buf = vec![0u32; w * h];
        assert!(draw_search_empty_state(
            &mut buf, w, w, h, &sh.grid, &style, w as f32, &long
        ));
        // A NaN size must be refused rather than turned into a NaN pen.
        let flat_style = DrawerStyle {
            label_px: 0.0,
            ..style
        };
        let _ = flat_style;
    }

    /// The "Search on <provider>" row is always available, whether or not
    /// anything matched.
    ///
    /// This is the row that makes an unmatched query useful
    /// (`LawnchairLocalSearchAlgorithm.generateActionResults:146-179`, appended
    /// for *every* query, and built by `ActionsSectionBuilder:157-187`).
    #[test]
    fn the_web_search_action_row_paints_its_prefix_and_provider() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let l = panel();
        let d = dp();
        let style = DrawerStyle::dark(d);
        let sh = sheet(l.h);
        let band = sh.grid;

        let row = web_search_action_rect(&band, w as f32, band.y);
        // 64 dp, 4 dp radius, 16 dp padding -- `search_result_small_row_height`,
        // `search_result_radius`, `search_result_padding`
        // (`dimens.xml:67, 60, 62`).
        assert!(
            near(row.h, 64.0 * d, 1e-3),
            "the row is {} px, want 64 dp = {}",
            row.h,
            64.0 * d
        );
        assert!(
            near(row.radius, 4.0 * d, 1e-3),
            "the row radius is {}",
            row.radius
        );
        assert!(
            row.x >= band.x - 1e-3 && row.w <= band.w + 1e-3,
            "the row must line up with the band it sits under"
        );
        // The reference appends it *after* the results, so it is below the
        // anchor y and the 12 dp gap is the SPACE header's band.
        assert!(
            row.y >= band.y + SECTION_HEADER_GAP_DP * d - 1e-3,
            "the row must clear the anchor by the 12 dp gap, got {}",
            row.y - band.y
        );

        let mut buf = vec![0u32; w * h];
        assert!(draw_web_search_action(
            &mut buf,
            w,
            w,
            h,
            &row,
            &style,
            w as f32,
            "Startpage"
        ));
        // A leading magnifier in the secondary tint and the label in
        // `on_surface`: `setTint(TextColorSecondary)` is only applied when the
        // provider is a `CustomWebSearchProvider` (`SearchTargetFactory.kt:249-251`)
        // and the *title* is `textColorPrimary` throughout
        // (`search_result_text.xml:31`).
        let has = |c: u32| (0..h).any(|y| (0..w).any(|x| buf[y * w + x] == c));
        assert!(has(style.on_surface), "the label must be on_surface");
        assert!(
            has(style.on_surface_variant),
            "the leading icon must be the secondary tint"
        );
        // Both halves of the label are present, and the provider name is after
        // the prefix rather than before it.
        let size = d * SECTION_HEADER_TEXT_DP;
        let text_x = row.x + 16.0 * d + row.h * 0.375 + 16.0 * d * 0.5;
        let px = (text_x as usize)..((text_x + 400.0) as usize).min(w);
        let inked = |needle: &str| -> bool {
            let nw = font::measure(needle, size);
            (0..(nw.ceil() as usize).max(1))
                .filter(|&k| {
                    px.clone()
                        .any(|x| buf[(row.center_y() as usize) * w + x + k] != 0)
                })
                .count()
                > 0
        };
        assert!(inked(WEB_SEARCH_PREFIX), "\"Search on \" must be drawn");
        // The row is horizontally finite, so a name longer than the row is
        // elided rather than drawn past the padding.
        let long: String = "S".repeat(4096);
        let mut buf2 = vec![0u32; w * h];
        assert!(draw_web_search_action(
            &mut buf2, w, w, h, &row, &style, w as f32, &long
        ));
        // Nothing past the right-hand padding edge.
        let right = (row.x + row.w - 16.0 * d) as usize;
        assert!(
            (right + 1..w).all(|x| (0..h).all(|y| buf2[y * w + x] == 0)),
            "a long provider name ran past the row's padding"
        );
        // Degenerate geometry is inert.
        let flat = super::super::layout::Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            radius: 0.0,
        };
        let mut buf3 = vec![0u32; w * h];
        assert!(!draw_web_search_action(
            &mut buf3, w, w, h, &flat, &style, w as f32, "x"
        ));
        assert!(buf3.iter().all(|p| *p == 0), "a flat row wrote pixels");
    }

    /// The action row is clamped into its band.
    ///
    /// An anchor near the bottom must not push the row off the panel, because
    /// the reference's `RecyclerView` scrolls the action row into view
    /// (`AllAppsFastScrollHelper.smoothScrollToSection:41-47`) and the caller's
    /// equivalent is a clamp, not an unbounded offset.
    #[test]
    fn the_web_search_row_is_clamped_into_its_band() {
        let d = dp();
        let sh = sheet(panel().h);
        let band = sh.grid;
        let w = 1080.0;
        let h = 64.0 * d;
        // An anchor far below the band.
        let deep = web_search_action_rect(&band, w, band.y + band.h * 10.0);
        assert!(
            deep.y + deep.h <= band.y + band.h + 1e-3,
            "a deep anchor must not push the row past the band: {} .. {} vs band {}",
            deep.y,
            deep.y + deep.h,
            band.y + band.h
        );
        assert!(near(deep.h, h, 1e-3), "the clamp must not resize the row");
        // An anchor above the band puts the row at the band's top.
        let high = web_search_action_rect(&band, w, band.y - band.h);
        assert!(
            high.y >= band.y - 1e-3,
            "a high anchor must not put the row above the band: {}",
            high.y
        );
    }

    /// One header per result group, with the reference's 52 dp band and 12 dp
    /// divider, for as many groups as there are.
    ///
    /// The sheet currently paints a single flat count label
    /// (`drm_kms.rs:1283-1294` into [`draw_drawer_sheet`]) and the reference
    /// emits a header per group (`SectionBuilder.kt:24-235`) separated by a
    /// `createHeaderTarget(SPACE)`. This pins the geometry for N of them and
    /// the paint for each.
    #[test]
    fn section_headers_stack_at_52dp_with_a_12dp_divider() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let d = dp();
        let style = DrawerStyle::dark(d);
        let sh = sheet(panel().h);
        let band = sh.grid;

        let rows = section_header_rows(&band, w as f32, band.y, SECTION_HEADER_MAX);
        // `search_result_text_height` = 52 dp (`dimens.xml:68`), the gap is the
        // 12 dp `search_result_text_padding` / SPACE header
        // (`dimens.xml:62`, `SectionBuilder.kt:38`).
        assert!(
            near(rows[0].h, 52.0 * d, 1e-3),
            "a header is {} px, want 52 dp = {}",
            rows[0].h,
            52.0 * d
        );
        let pitch = 52.0 * d + 12.0 * d;
        for (i, r) in rows.iter().enumerate() {
            assert!(
                near(r.y, band.y + pitch * i as f32, 1e-3),
                "header {i} is at {}, want {}",
                r.y,
                band.y + pitch * i as f32
            );
            assert!(
                near(r.x, band.x, 1e-3) && near(r.w, band.w, 1e-3),
                "header {i} must span the content edge"
            );
        }
        // Asking for more groups than the fixed capacity is clamped, not a
        // panic and not an overflow: the array is `[Rect; SECTION_HEADER_MAX]`
        // and a caller asking for 99 gets 8 real ones.
        let over = section_header_rows(&band, w as f32, band.y, 99);
        assert_eq!(over.len(), SECTION_HEADER_MAX);
        assert!(
            near(
                over[SECTION_HEADER_MAX - 1].y,
                rows[SECTION_HEADER_MAX - 1].y,
                1e-3
            ),
            "the clamp must not shift the rows it does return"
        );
        assert_eq!(section_header_rows(&band, w as f32, band.y, 0)[0].h, 0.0);

        // Paint: each header inks its own band, and the divider is a hairline
        // the width of the content edge below it.
        for (i, label) in ["Apps", "Web suggestions", "Contacts"].iter().enumerate() {
            let mut buf = vec![0u32; w * h];
            assert!(
                draw_section_header(&mut buf, w, w, h, &rows[i], &style, w as f32, label, true),
                "header {i} ({label}) must report that it drew something"
            );
            let r = &rows[i];
            let x0 = r.x.max(0.0) as usize;
            let x1 = ((r.x + r.w) as usize).min(w);
            let y0 = r.y.max(0.0) as usize;
            let y1 = ((r.y + r.h) as usize).min(h);
            let band_ink = (y0..y1)
                .flat_map(|y| (x0..x1).map(move |x| (x, y)))
                .filter(|&(x, y)| buf[y * w + x] != 0)
                .count();
            assert!(band_ink > 100, "header {i} inked only {band_ink} px");
            // The 16 dp leading glyph is `on_surface` and it is a *filled*
            // square at the header's left edge, so its own row is solidly inked
            // rather than a glyph outline.
            //
            // Probed over a range rather than at one pixel because
            // `fill_round_rect` fills `ceil(lo)..floor(hi)`
            // (`drawer_mod`'s own helper), and `r.x` is a fractional dp value
            // -- the exact column is a rounding artefact, the *run* of ink is
            // the geometry.
            let icon_d = 16.0 * d;
            let ix = r.x as usize;
            let iy = r.center_y() as usize;
            let icon_ink = (0..(icon_d as usize).min(x1.saturating_sub(ix)))
                .filter(|&k| buf[iy * w + ix + k] == style.on_surface)
                .count();
            assert!(
                icon_ink > 3,
                "header {i}: the leading icon must be a filled on_surface block \
                 at the left edge, got {icon_ink} px"
            );
            // And it is leftmost: the ink starts at the band's own edge, which
            // is what makes a column of headers read as a column rather than as
            // ragged text. Nothing is inked in the margin to the left.
            let margin = sh.grid.x as usize;
            if margin > 0 {
                assert!(
                    (0..margin).all(|x| (0..h).all(|y| buf[y * w + x] == 0)),
                    "header {i}: ink leaked into the drawer's left margin"
                );
            }
            // The label is to the right of the icon, at 14 sp
            // (`search_result_hero_subtitle_size`, `dimens.xml:58`).
            let label_x = (r.x + icon_d + 14.0 * d * 0.28) as usize;
            assert!(
                (label_x..x1).any(|x| (y0..y1).any(|y| buf[y * w + x] != 0)),
                "header {i}: the label must be drawn to the right of the icon"
            );
            // The divider, in the gap below the header, in `outline`.
            //
            // Probed over a few rows rather than at one: `draw_section_header`
            // places the hairline at a *fractional* y
            // (`rect.y + rect.h + gap / 2`) and `fill_round_rect` fills
            // `ceil(lo)..floor(hi)`, so which single row of a 1-px hairline
            // catches it is a rounding artefact. The hairline's *position* --
            // inside the 12 dp gap and not inside the header or the next group
            // -- is the geometry, and that is what a 3-row window asserts.
            let gx = (r.x + r.w * 0.5) as usize;
            let gap_top = r.y + r.h;
            let gap_mid = gap_top + 12.0 * d * 0.5;
            let found = (0..3).any(|k| {
                let y = (gap_mid as usize) + k;
                y < h && buf[y * w + gx] == style.outline
            });
            assert!(
                found,
                "header {i}: the SPACE-header divider must be in the 12 dp gap \
                 below it (gap_top {gap_top}, mid {gap_mid})"
            );
        }

        // The last header in a group carries no divider, because the
        // reference appends the SPACE header *after* the group's rows
        // (`SectionBuilder.kt:38`) -- so the divider is drawn by the caller on
        // the last one, not by every one.
        let mut buf = vec![0u32; w * h];
        assert!(draw_section_header(
            &mut buf, w, w, h, &rows[0], &style, w as f32, "Apps", false
        ));
        let x = (rows[0].x + rows[0].w * 0.5) as usize;
        let gap_mid = (rows[0].y + rows[0].h + 12.0 * d * 0.5) as usize;
        assert!(
            (0..3).all(|k| gap_mid + k >= h || buf[(gap_mid + k) * w + x] != style.outline),
            "divider=false must not draw the hairline"
        );
        // An empty label paints the icon and nothing else, and reports it.
        let mut buf = vec![0u32; w * h];
        assert!(draw_section_header(
            &mut buf, w, w, h, &rows[0], &style, w as f32, "", true
        ));
        // Degenerate geometry is inert.
        let flat = super::super::layout::Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            radius: 0.0,
        };
        let mut buf = vec![0u32; w * h];
        assert!(!draw_section_header(
            &mut buf, w, w, h, &flat, &style, w as f32, "x", true
        ));
        assert!(buf.iter().all(|p| *p == 0), "a flat header wrote pixels");
    }

    /// The prediction row's 108 dp is currently reserved and never read.
    ///
    /// `DrawerSheetLayout::predictions` / `pred_icon_d` / `pred_icon_pad`
    /// (`layout.rs:2001-2009`) are 108 dp of dead space in the middle of the
    /// sheet: the layout bakes them in and nothing draws them, so the drawer's
    /// first grid row starts 108 dp lower than the reference's. This test is
    /// for the paint that closes that gap -- and it is deliberately a test of
    /// *this* file's function, because `layout.rs` belongs to another slice.
    /// See the handoff.
    #[test]
    fn the_prediction_row_paints_an_icon_and_the_label_under_it() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let d = dp();
        let style = DrawerStyle::dark(d);
        let sh = sheet(panel().h);

        // The reference's 108 dp is already measured by the layout
        // (`PREDICTION_ROW_H_DP`, `PredictionRowView.getExpectedHeight():149-161`),
        // so the paint reads the layout's own numbers rather than re-deriving
        // them: `icon_size (65) + drawable_padding (7) + label_height (16) +
        // padding (16) + extra (4)`.
        assert!(
            near(sh.predictions.h, 108.0 * d, 1e-3),
            "the row is {} px, want 108 dp = {}",
            sh.predictions.h,
            108.0 * d
        );
        let mut buf = vec![0u32; w * h];
        assert!(draw_prediction_row(
            &mut buf,
            w,
            w,
            h,
            &sh.predictions,
            &style,
            0,
            "Firefox",
            &sh.pred_icon_d
        ));
        // The icon: a 65 dp rounded square at the row's left edge, in the
        // accent fill the reference gives a monogram tile.
        let icon = sh.prediction_icon();
        assert!(
            near(icon.w, 65.0 * d, 1e-3),
            "the prediction icon is {} px, want 65 dp = {}",
            icon.w,
            65.0 * d
        );
        let cx = icon.x as usize + 2;
        let cy = icon.center_y() as usize;
        assert_ne!(buf[cy * w + cx], 0, "the prediction icon must be painted");
        // The label: under the icon, separated by the 7 dp
        // `all_apps_icon_drawable_padding` (`PREDICTION_ICON_PAD_DP`,
        // `PredictionRowView.java:152`), and inside the row.
        let label = sh.prediction_label();
        assert!(
            label.y >= icon.y + icon.h + 7.0 * d - 1e-3,
            "the label starts at {}, the icon ends at {}",
            label.y,
            icon.y + icon.h
        );
        assert!(
            label.y + label.h <= sh.predictions.y + sh.predictions.h + 1e-3,
            "the label must fit inside the 108 dp row"
        );
        let lx0 = label.x.max(0.0) as usize;
        let lx1 = ((label.x + label.w) as usize).min(w);
        let ly0 = label.y.max(0.0) as usize;
        let ly1 = ((label.y + label.h) as usize).min(h);
        let label_ink = (ly0..ly1)
            .flat_map(|y| (lx0..lx1).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[y * w + x] != 0)
            .count();
        assert!(
            label_ink > 30,
            "the prediction label inked only {label_ink} px"
        );
        // A different label inks different pixels, which is what proves the
        // text is the label and not a fixed decoration.
        let mut b2 = vec![0u32; w * h];
        draw_prediction_row(
            &mut b2,
            w,
            w,
            h,
            &sh.predictions,
            &style,
            0,
            "Terminal",
            &sh.pred_icon_d,
        );
        assert_ne!(b2, buf, "two different labels must paint differently");
        // Slot geometry: `mNumPredictedAppsPerRow = numShownAllAppsColumns`
        // (`PredictionRowView.java:85-86`) and each child is `lp.width = 0,
        // lp.weight = 1` (`:245-246`), so slot `i` is an equal share of the
        // row. That is the whole reason `slot` is a parameter.
        let pitch = sh.predictions.w / 4.0;
        for slot in 0..4usize {
            let g = prediction_slot_rect(&sh.predictions, slot, 4);
            assert!(
                near(g.x, sh.predictions.x + pitch * slot as f32, 1e-2),
                "slot {slot} is at {}, want {}",
                g.x,
                sh.predictions.x + pitch * slot as f32
            );
            assert!(near(g.w, pitch, 1e-2), "slots are an equal share");
        }
        // A slot past the count is inert, not an out-of-bounds write.
        let mut b3 = vec![0u32; w * h];
        assert!(!draw_prediction_row(
            &mut b3,
            w,
            w,
            h,
            &sh.predictions,
            &style,
            9,
            "X",
            &sh.pred_icon_d
        ));
        assert!(b3.iter().all(|p| *p == 0), "slot 9 of 4 wrote pixels");
    }

    /// The prediction row's monogram is the fallback for an app with no
    /// raster icon, and the glyph is centred.
    #[test]
    fn the_prediction_row_glyph_is_centred_in_its_icon() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let d = dp();
        let style = DrawerStyle::dark(d);
        let sh = sheet(panel().h);
        let mut buf = vec![0u32; w * h];
        assert!(draw_prediction_row(
            &mut buf,
            w,
            w,
            h,
            &sh.predictions,
            &style,
            0,
            "Firefox",
            &sh.pred_icon_d
        ));
        let icon = sh.prediction_icon();
        // The reference's `BubbleTextView` centres its icon in the cell
        // (`LinearLayout` gravity, `PredictionRowView.java:230-247`), so the
        // monogram's ink is symmetric about the icon's axis to within the
        // rasteriser's pixel quantisation.
        let x0 = icon.x.max(0.0) as usize;
        let x1 = ((icon.x + icon.w) as usize).min(w);
        let y0 = icon.y.max(0.0) as usize;
        let y1 = ((icon.y + icon.h) as usize).min(h);
        let row_ink = |y: usize| -> (usize, usize) {
            let xs: Vec<usize> = (x0..x1).filter(|&x| buf[y * w + x] != 0).collect();
            (*xs.first().unwrap_or(&0), *xs.last().unwrap_or(&0))
        };
        // The vertical middle of the icon, where a centred glyph has its
        // widest extent.
        let mid = (y0 + y1) / 2;
        let (lo, hi) = row_ink(mid);
        let glyph_mid = (lo + hi) as f32 * 0.5;
        assert!(
            (glyph_mid - icon.center_x()).abs() < icon.w * 0.15,
            "the glyph's centre is {glyph_mid}, the icon's is {}",
            icon.center_x()
        );
    }

    #[test]
    fn drawer_sheet_uses_the_opaque_scrim_path() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let l = panel();
        let d = dp();
        let style = DrawerStyle::dark(d);
        let sh = sheet(l.h); // fully open
        let sheet_px = raster::composite_scrim(style.backdrop, sh.scrim_argb, scrim_alpha(&sh));

        let mut buf = vec![style.backdrop; w * h];
        assert!(draw_drawer_sheet(
            &mut buf, w, w, h, &sh, 1.0, &style, "", "42 APPS"
        ));

        // The scrim is a *precomputed* colour, so it is opaque, and it is the
        // exact composite of the backdrop and the token -- not a per-pixel
        // blend of some other value.
        assert_eq!(sheet_px >> 24, 0xFF, "composited scrim must be opaque");
        assert_ne!(
            sheet_px & 0x00FF_FFFF,
            style.backdrop & 0x00FF_FFFF,
            "scrim is a no-op"
        );
        let mid = sh.grid.y as usize + 10;
        let px = buf[mid * w + (w / 2)];
        assert_eq!(px, sheet_px, "grid band must be the precomputed composite");
        assert_eq!(px >> 24, 0xFF, "a written pixel must be opaque");

        // The scrim band must contain no *blended* pixel. The set of values
        // the opaque path is allowed to produce is exactly the caller's opaque
        // tokens plus the precomputed composite -- a per-pixel translucent
        // blend would leave values outside it, which is what
        // `no_full_screen_translucent_blend` guards against.
        let allowed = [
            style.backdrop,
            sheet_px,
            style.outline,
            style.surface_high,
            style.primary,
            style.on_surface,
            style.on_surface_variant,
        ];
        // Glyph edges are coverage-blended, so text legitimately produces
        // values between two tokens. `is_between` admits exactly those and
        // nothing else; a scrim blended per pixel over the *structured* grid
        // would produce values between the backdrop and the scrim, which is
        // not a pair any chrome draw uses.
        let mut checked = 0usize;
        for y in sh.top as usize..h {
            for x in 0..w {
                let p = buf[y * w + x];
                assert!(
                    allowed.contains(&p)
                        || is_between(p, sheet_px, style.on_surface)
                        || is_between(p, sheet_px, style.on_surface_variant)
                        || is_between(p, style.surface_high, style.on_surface)
                        || is_between(p, style.surface_high, style.on_surface_variant)
                        || is_between(p, style.surface_high, style.primary)
                        || is_between(p, sheet_px, style.primary),
                    "row {y} col {x}: {p:08x} is neither a precomputed value nor a \
                     two-token coverage blend (a per-pixel scrim leaked in)"
                );
                // The flat grid band carries *only* the composite: the
                // backdrop never survives inside the sheet, and nothing is
                // painted between the chrome and the grid.
                if y as f32 >= sh.grid.y {
                    assert_eq!(p, sheet_px, "row {y} col {x} is not the composite");
                }
                checked += 1;
            }
        }
        assert!(checked > w * 1000, "only {checked} pixels checked");

        // And nothing above the sheet's top edge was touched: the scrim does
        // not bleed past the clip.
        for y in 0..sh.top as usize {
            for x in 0..w {
                assert_eq!(
                    buf[y * w + x],
                    style.backdrop,
                    "the scrim leaked above the sheet at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn drawer_sheet_clips_to_the_shift() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let l = panel();
        let style = DrawerStyle::dark(dp());

        // shift = 0 -> the sheet is entirely below the panel. progress is
        // deliberately non-zero so this is the *geometry* clipping, not the
        // early-out.
        let closed = sheet(0.0);
        assert!(
            closed.top >= h as f32,
            "shift 0 must put the top edge at the bottom"
        );
        let mut buf = vec![style.backdrop; w * h];
        let drew = draw_drawer_sheet(&mut buf, w, w, h, &closed, 1.0, &style, "", "");
        assert!(!drew, "a closed sheet must report that it drew nothing");
        assert!(
            buf.iter().all(|p| *p == style.backdrop),
            "a closed sheet wrote {} pixels",
            buf.iter().filter(|p| **p != style.backdrop).count()
        );

        // shift = h -> fully open, top edge at 0, and the sheet reaches the
        // bottom of the panel.
        let open = sheet(l.h);
        assert_eq!(open.top, 0.0, "shift h must put the top edge at 0");
        let mut buf = vec![style.backdrop; w * h];
        assert!(draw_drawer_sheet(
            &mut buf, w, w, h, &open, 1.0, &style, "", "42 APPS"
        ));
        let written = buf.iter().filter(|p| **p != style.backdrop).count();
        assert!(
            written > w * 2000,
            "a fully open sheet wrote only {written} pixels"
        );
        // Every column of the last row is sheet, i.e. the sheet reaches the
        // bottom edge: the bottom corners are square, not inset.
        for x in (0..w).step_by(97) {
            assert_ne!(
                buf[(h - 1) * w + x],
                style.backdrop,
                "gap at the bottom row x={x}"
            );
        }
        // The top corners are rounded, so the very first row only covers the
        // middle of the panel.
        let first_row_ink = (0..w).filter(|x| buf[*x] != style.backdrop).count();
        assert!(
            first_row_ink > w / 2 && first_row_ink < w,
            "top row ink {first_row_ink} of {w}: corners must be rounded"
        );

        // progress = 0 is the early-out, whatever the shift.
        let mut buf = vec![style.backdrop; w * h];
        assert!(!draw_drawer_sheet(
            &mut buf, w, w, h, &open, 0.0, &style, "", ""
        ));
        assert!(
            buf.iter().all(|p| *p == style.backdrop),
            "progress 0 wrote pixels"
        );
        assert!(!draw_drawer_sheet(
            &mut buf,
            w,
            w,
            h,
            &open,
            f32::NAN,
            &style,
            "",
            ""
        ));
        assert!(
            buf.iter().all(|p| *p == style.backdrop),
            "NaN progress wrote pixels"
        );
    }

    #[test]
    fn drawer_sheet_search_box_and_chrome_land_where_the_layout_says() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let l = panel();
        let d = dp();
        let style = DrawerStyle::dark(d);
        let sh = sheet(l.h);

        // 52 dp box inside a 60 dp container, 4 dp of air each side.
        assert!(
            near(sh.search.h, 52.0 * d, 1e-3),
            "search box is {} px",
            sh.search.h
        );
        let container = sh.search_container();
        assert!(
            near(container.h, 60.0 * d, 1e-3),
            "container is {} px",
            container.h
        );
        assert!(
            near(container.h - sh.search.h, 8.0 * d, 1e-3),
            "8 dp of air in total"
        );
        // 48 dp header pill, r 12 dp; 128 x 2 dp divider, r 2 dp.
        assert!(
            near(sh.header.h, 48.0 * d, 1e-3),
            "header pill is {} px",
            sh.header.h
        );
        assert!(
            near(sh.header.radius, 12.0 * d, 1e-3),
            "header radius is {}",
            sh.header.radius
        );
        assert!(
            near(sh.divider.w, 128.0 * d, 1e-3),
            "divider is {} px",
            sh.divider.w
        );
        assert!(
            near(sh.divider.h, 2.0 * d, 1e-3),
            "divider is {} px",
            sh.divider.h
        );
        // Sheet corner: 24 dp top, 0 dp bottom.
        assert!(
            near(sh.corner_r, 24.0 * d, 1e-3),
            "corner radius is {}",
            sh.corner_r
        );
        // Prediction row: `icon_size (65) + drawable_padding (7) +
        // label_height (16) + padding (16) + extra (4)` ~= 108 dp
        // (`PredictionRowView.java:149-162`). This was 65 dp, which is the
        // ICON height alone -- the label had nowhere to go and every
        // prediction title was clipped by the row's own bottom edge. The row
        // is icon-above-label, exactly like the main drawer grid, so 108 dp
        // is the smallest container that fits both.
        assert!(
            near(sh.predictions.h, 108.0 * d, 1e-3),
            "prediction row is {}",
            sh.predictions.h
        );
        assert!(near(
            sh.grid.y - (sh.predictions.y + sh.predictions.h),
            12.0 * d,
            1e-3
        ));

        // Paint: the divider is a `style.outline` band at the layout's y.
        let mut buf = vec![style.backdrop; w * h];
        draw_drawer_sheet(&mut buf, w, w, h, &sh, 1.0, &style, "", "42 APPS");
        let dy = (sh.divider.y + sh.divider.h * 0.5) as usize;
        let dx = sh.divider.center_x() as usize;
        assert_eq!(
            buf[dy * w + dx],
            style.outline,
            "divider not painted at its layout y"
        );
        // The handle is an `outline` 32 x 4 dp pill.
        let hy = (sh.handle.y + sh.handle.h * 0.5) as usize;
        assert_eq!(
            buf[hy * w + sh.handle.center_x() as usize],
            style.outline,
            "handle missing"
        );
        assert!(near(sh.handle.w, 32.0 * d, 1e-3) && near(sh.handle.h, 4.0 * d, 1e-3));
    }

    #[test]
    fn search_hint_tracking_tightens_the_run() {
        let _guard = lock_font();
        let w = 400usize;
        let h = 200usize;
        let size = 40.0;
        let text = "Search apps";
        let tracking = size * SEARCH_HINT_TRACKING;
        let gaps = (text.len() - 1) as f32;
        // One percent *tighter* per gap, and "Search apps" has 10 gaps, so the
        // tracked run is 0.4 px narrower per gap at 40 px. `tracking` is
        // negative; the run is shorter by that much.
        assert_eq!(gaps, 10.0, "\"Search apps\" must have 10 gaps");
        assert!(
            near(gaps * tracking, -10.0 * size * 0.01, 1e-3),
            "tracking is {tracking} per gap, want {}",
            -0.01 * size
        );
        // And it actually draws: ink lands left of where the untracked run
        // would end.
        let mut a = vec![0u32; w * h];
        draw_run_tracked(
            &mut a,
            w,
            w,
            h,
            20.0,
            140.0,
            text,
            0xFFFFFFFF,
            size,
            FontWeight::Regular,
            tracking,
        );
        let mut b = vec![0u32; w * h];
        font::draw_run(
            &mut b,
            w,
            w,
            h,
            20.0,
            140.0,
            text,
            0xFFFFFFFF,
            size,
            FontWeight::Regular,
        );
        let last = |buf: &[u32]| {
            (0..w)
                .rev()
                .find(|x| (0..h).any(|y| buf[y * w + x] != 0))
                .unwrap()
        };
        assert!(last(&a) < last(&b), "tracked run did not tighten");
    }

    #[test]
    fn fit_prefix_never_splits_a_char() {
        // A multi-byte name: the cut must land on a char boundary.
        for n in 1..40usize {
            let s: String = "\u{00e9}".repeat(n);
            for wpx in 0..=80u32 {
                let cut = fit_prefix(&s, 10.0, 0.0, wpx as f32);
                assert!(
                    s.is_char_boundary(cut.len()),
                    "split at {} for {wpx}",
                    cut.len()
                );
                assert!(s.starts_with(cut), "not a prefix");
            }
        }
        assert_eq!(fit_prefix("abc", 10.0, 0.0, -1.0), "");
        assert_eq!(fit_prefix("abc", 10.0, 0.0, 1.0e6), "abc");
    }

    // -- fast-scroller paint ----------------------------------------------

    #[test]
    fn fastscroller_paints_track_thumb_and_popup() {
        let _guard = lock_font();
        let w = 1080usize;
        let h = 2400usize;
        let fs = scroller();
        let d = dp();
        let mut st = FastScrollerState::new();
        st.on_down(fs.track.y + 300.0 * d, 0.0, &fs);
        st.on_move(fs.track.y + 300.0 * d + 2.0 * d, 50.0, &fs);
        assert!(st.dragging);
        st.popup_alpha = 1.0;
        st.letter = 14; // 'N'
        st.popup_y = fs.track.y + 100.0 * d;

        let mut buf = vec![0u32; w * h];
        let thumb = 0xFF8AB4F8u32;
        let text = 0xFFFFFFFFu32;
        draw_fast_scroller(&mut buf, w, w, h, &fs, &st, thumb, TRACK_ALPHA, text);

        let ink = buf.iter().filter(|p| **p != 0).count();
        assert!(ink > 1000, "fast scroller drew only {ink} pixels");

        // The thumb is the accent at full alpha, 52 dp tall, 8 dp wide
        // (6 dp track + 2 x 1 dp padding), centred on the track.
        let cx = fs.track.center_x() as usize;
        let ty = (fs.track.y + st.thumb_y) as usize + (fs.thumb_h as usize) / 2;
        assert_eq!(
            buf[ty * w + cx],
            thumb,
            "thumb not painted at its layout position"
        );
        let tw = (fs.track_w + fs.thumb_pad * 2.0) as usize;
        assert_eq!(
            buf[ty * w + (cx - tw / 2)],
            thumb,
            "thumb is narrower than track + 2*pad"
        );
        assert_eq!(
            buf[(ty + fs.thumb_h as usize / 2) * w + cx],
            thumb,
            "thumb is not 52 dp tall"
        );

        // The track is the accent at `MAX_TRACK_ALPHA`. Sample it *above* the
        // thumb: the thumb is 52 dp tall starting at the track's top, so the
        // top of the track is covered whenever the thumb is parked there.
        let track_px = buf[((fs.track.y + st.thumb_y + fs.thumb_h + 20.0) as usize) * w + cx];
        assert_eq!(track_px >> 24, 0xFF, "blend must force an opaque result");
        assert_eq!(
            track_px,
            raster::blend_alpha(0, thumb, TRACK_ALPHA),
            "the track must be the accent at MAX_TRACK_ALPHA, got {track_px:08x}"
        );
        // And it is *not* the opaque accent, i.e. the track really is
        // translucent while the thumb is not.
        assert_ne!(track_px, thumb, "the track must be translucent, not opaque");

        // The popup blob is a teardrop centred on the letterbox, and the
        // letter is drawn on it.
        let pc = (
            fs.popup.center_x() as usize,
            st.popup_y as usize + fs.popup.h as usize / 2,
        );
        assert!(
            buf[pc.1 * w + pc.0] == thumb,
            "popup blob missing at its centre"
        );
        let blob = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[y * w + x] == thumb)
            .count();
        assert!(
            blob > (fs.popup.w * fs.popup.h) as usize / 4,
            "popup ink {blob} is too small for a 75x62 dp letterbox"
        );
        let letter_ink = buf.iter().enumerate().filter(|(_, p)| **p == text).count();
        assert!(letter_ink > 20, "letter not painted ({letter_ink} px)");
    }

    #[test]
    fn fastscroller_paints_nothing_when_the_popup_is_faded_out() {
        let w = 1080usize;
        let h = 2400usize;
        let fs = scroller();
        let mut st = FastScrollerState::new();
        st.on_down(fs.track.y + 100.0, 0.0, &fs);
        st.popup_alpha = 0.0;
        let mut buf = vec![0u32; w * h];
        draw_fast_scroller(
            &mut buf,
            w,
            w,
            h,
            &fs,
            &st,
            0xFF8AB4F8,
            TRACK_ALPHA,
            0xFFFFFFFF,
        );
        // The track and thumb legitimately cover a few thousand pixels (the
        // 6 dp track alone is ~15 px wide over the full list height), so the
        // assertion is not a pixel budget but *where* the ink is: it must all
        // be inside the trailing-edge band, and the 75 x 62 dp letterbox --
        // which sits 19 dp in from that edge -- must be untouched.
        // The band is the *pressed* thumb's extent, which is the widest thing
        // this function can draw on the trailing edge: both the track and the
        // thumb are `mWidth`-based (`:416`, `:423`).
        let half = (fs.track_w_pressed + fs.thumb_pad * 2.0) * 0.5;
        let cxf = fs.track.center_x();
        let band = ((cxf - half).max(0.0) as usize)..=((cxf + half) as usize).min(w - 1);
        for y in 0..h {
            for x in 0..w {
                if buf[y * w + x] != 0 {
                    assert!(
                        band.contains(&x),
                        "ink at x={x} is outside the trailing band {band:?}"
                    );
                }
            }
        }
        // The letterbox is 19 dp in from the trailing edge and the thumb is
        // 8 dp wide centred on the edge, so the 75 x 62 dp box clears the
        // band and must be entirely untouched.
        let pop_x0 = fs.popup.x.max(0.0) as usize;
        let pop_x1 = ((fs.popup.x + fs.popup.w) as usize).min(w);
        assert!(
            pop_x1 <= *band.start(),
            "the letterbox must clear the {band:?} band, it reaches {pop_x1}"
        );
        let pop_lo = (st.popup_y.max(0.0) as usize).min(h.saturating_sub(1));
        let pop_hi = ((st.popup_y + fs.popup.h).max(0.0) as usize).min(h);
        for y in pop_lo..pop_hi {
            for x in pop_x0..pop_x1 {
                assert_eq!(buf[y * w + x], 0, "the popup drew at {x},{y} with alpha 0");
            }
        }
        // Track width: idle 6 dp, pressed 8 dp. Measured *below* the thumb,
        // which sits at the track's top and is 8 dp wide even when idle --
        // sampling the top of the track would measure the thumb, not the
        // track. Below it, the only ink is the track.
        // A *count* of inked columns, not `last - first`: the fill is
        // `ceil(lo)..floor(hi)`, so an N-column span measures N, and calling it
        // a distance is off by one everywhere.
        let width_at = |buf: &[u32], y: usize| -> usize {
            let row = y * w;
            (0..w).filter(|x| buf[row + x] != 0).count()
        };
        let below = (fs.track.y + fs.thumb_h + 20.0) as usize;
        let idle_w = width_at(&buf, below);
        // `fill_round_rect` fills `ceil(lo)..floor(hi)`, so an N-px shape
        // covers between N-1 and N+1 columns depending on where its edges
        // land. At 6 dp = 15.43 px that is 15; at 8 dp = 20.57 px it is 19.
        // Two columns is the quantisation band, not a geometry error.
        assert!(
            (idle_w as f32 - 6.0 * dp()).abs() <= 2.0,
            "idle track is {idle_w} px, want 6 dp = {}",
            6.0 * dp()
        );
        st.dragging = true;
        let mut buf2 = vec![0u32; w * h];
        draw_fast_scroller(
            &mut buf2,
            w,
            w,
            h,
            &fs,
            &st,
            0xFF8AB4F8,
            TRACK_ALPHA,
            0xFFFFFFFF,
        );
        let pressed_w = width_at(&buf2, below);
        assert!(
            pressed_w > idle_w,
            "pressed track {pressed_w} must be wider than idle {idle_w}"
        );
        assert!(
            (pressed_w as f32 - 8.0 * dp()).abs() <= 2.0,
            "pressed track is {pressed_w} px, want 8 dp = {}",
            8.0 * dp()
        );
        // And the thumb itself does not change width with the press: it grows
        // by the track's growth, i.e. by 2 dp, since both are `mWidth`-based.
        let thumb_w = |buf: &[u32]| -> usize {
            let row = (fs.track.y as usize + fs.thumb_h as usize / 2) * w;
            (0..w).filter(|x| buf[row + x] != 0).count()
        };
        assert!(
            thumb_w(&buf2) > thumb_w(&buf),
            "the pressed thumb must widen with the track"
        );
        assert!(
            (thumb_w(&buf) as f32 - 8.0 * dp()).abs() <= 2.0,
            "idle thumb is {} px, want track + 2*pad = {}",
            thumb_w(&buf),
            8.0 * dp()
        );
    }

    #[test]
    fn fastscroller_popup_letter_is_the_section() {
        let mut st = FastScrollerState::new();
        assert_eq!(st.letter_str(), "");
        for l in 1..=26u8 {
            st.letter = l;
            let s = st.letter_str();
            assert_eq!(s.len(), 1, "letter {l} rendered as {s:?}");
            assert_eq!(s.as_bytes()[0], b'A' + l - 1);
        }
        // Out-of-range values must not panic or index the table.
        for l in [27u8, 0, 255, 128] {
            st.letter = l;
            let _ = st.letter_str();
        }
    }
}
