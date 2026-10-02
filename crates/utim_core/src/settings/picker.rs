//! The wallpaper picker's paging model: a cursor over an injected candidate
//! list.
//!
//! # Why this is a module of its own
//!
//! [`super::SettingRow`] is `Copy` and holds only `&'static str`, which is what
//! lets the settings panel build its row vector once on open and outlive a
//! mutation of the state (`settings.rs`, module docs). That is the right
//! constraint for a *row*. It is the wrong constraint for a *picker*: a wallpaper
//! list is a `Vec<String>` of paths, unbounded by anything this crate can know,
//! and no `&'static str` sequence can express it.
//!
//! So the two are split. The row carries only the *position* -- `at`/`of`, two
//! `Option<u32>` -- and the state machine that owns the candidate list lives
//! here, behind a plain `&mut` handle the shell holds for the lifetime of the
//! panel.
//!
//! # Why it takes the list rather than finding it
//!
//! This module never touches the filesystem. The shell owns directory
//! enumeration and hands the paths in. That is the same split the rest of the
//! crate keeps between "resolve" and "draw": it is what makes the whole thing
//! testable with no temp directory and no root, and it is why the file that
//! decides *what* the user can pick (`/data/wallpapers`) is not baked into a
//! library that has no business knowing the device's filesystem layout.
//!
//! # What it borrows from the reference
//!
//! The reference has no `WallpaperFragment` and no `WallpaperPersister` in this
//! tree; its picker is a carousel of the wallpaper history, fed by a
//! `WallpaperViewModel` state flow (`data/wallpaper/model/WallpaperViewModel.kt:18-19`,
//! `wallpapers: StateFlow<List<Wallpaper>>`, filled from
//! `dao.getTopWallpapers()` at `:44`). Three of its behaviours are reproduced
//! here, and the third is the reason this file exists:
//!
//! * **Selection and commit are separate.** `WallpaperCarouselView.kt:108-115`:
//!   a touch on a card that is *not* `currentItemIndex` only moves the selection
//!   (`animateWidthTransition`), and only a touch on the card that *is* the
//!   selection commits it (`setWallpaper`). Hence [`Self::move_by`] is a
//!   separate call from [`Self::selected`], and the shell commits on a second
//!   tap.
//! * **Only the selected card is marked.** `WallpaperCarouselView.kt:232` adds
//!   the tick `iconFrame` to `currentItemIndex` and to nothing else, so the mark
//!   follows the cursor exactly.
//! * **A candidate that has gone missing is a hole, not a crash.**
//!   `WallpaperCarouselView.kt:136` reads
//!   `File(path).takeIf { it.exists() } ?: return null`, and `:141-153` then
//!   leaves `ic_deepshortcut_placeholder` in the card. That is precisely why
//!   [`Self::page`] returns `Option`: `None` is the placeholder slot, and it is
//!   a state the reference renders rather than skips.
//!
//! The carousel itself is a single scrolling row with no pages
//! (`WallpaperCarouselView.kt:53-56`, one `HORIZONTAL` `LinearLayout`), so the
//! four-slot paging here is ours: the shell has no scroll gesture in the panel,
//! and a fixed window is the only thing a per-frame renderer can index without
//! allocating.

/// How many candidates one page shows.
///
/// Four because that is what a settings row's width holds at the panel's type
/// scale, and because it is the reference's dock width on a small phone
/// (`DeviceProfile.java:222`, `numShownHotseatIcons` is 4 at 360 dp) -- the same
/// "a phone fits about four of these" answer. Fixed rather than variable
/// because [`Self::page`] returns a `[Option<&str>; SLOTS]` and
/// [`Self::candidates_at`] fills an array a frame later: a `Vec` return there
/// would allocate per frame, which [`crate::graphics::screenshot`] has a test
/// (`paint_frame_does_not_allocate`) that would fail.
pub const SLOTS: usize = 4;

/// A cursor over an injected list of wallpaper candidates.
///
/// The candidate list is owned (it is a `Vec<String>` of paths the shell
/// allocated while enumerating) but the cursor is the only mutable state, and
/// every accessor here is a fixed-capacity read. So the type is cheap to hold
/// across a frame and every method but [`Self::new`] is allocation-free.
///
/// The one invariant everything else rests on:
///
/// > `cursor < candidates.len()` whenever `candidates` is non-empty.
///
/// [`Self::new`] establishes it by clamping, [`Self::move_by`] preserves it by
/// reducing `cursor + delta` modulo `len`, and [`Self::page`] only produces a
/// `None` for indices `>= len`. Because the cursor is therefore *always* a real
/// candidate, `move_by` cannot land it on a padding slot -- see
/// [`Self::move_by`] for why that is stronger than checking for it after the
/// fact, and [`Self::new`] for the case it does have to handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallpaperPicker {
    /// Candidate paths, in the order the shell enumerated them. Empty is legal
    /// and is the normal state for a device with no wallpaper directory.
    candidates: Vec<String>,
    /// Index into `candidates`. Always valid when non-empty -- clamped by
    /// [`Self::new`] and preserved by [`Self::move_by`]. Zero when empty, where
    /// it is not an index into anything.
    cursor: usize,
}

impl WallpaperPicker {
    /// A picker over `candidates` with the cursor on `cursor`.
    /// Allocates nothing beyond moving `candidates` in; `Vec` is a move.
    ///
    /// # Why `cursor` is clamped rather than trusted
    ///
    /// `cursor` comes from the caller's *previous* picker -- the user leaves the
    /// settings panel, deletes three wallpapers from a file manager, comes back
    /// -- and the list has since been re-enumerated, so the index that was
    /// valid is one past the end. The reference hits the same state from the
    /// other side: its list is a `StateFlow` refilled by `getTopWallpapers()`
    /// (`data/wallpaper/model/WallpaperViewModel.kt:43-46`) after every
    /// `updateWallpaperRank` (`:56-59`), and the carousel redraws the whole list
    /// on the new one (`WallpaperCarouselView.kt:207`) without consulting
    /// `currentItemIndex`, so its selection is a bare `Int` that survives a list
    /// shrink (`WallpaperCarouselView.kt:45`).
    ///
    /// UTLC's list can shrink the same way -- and faster, because a plain `ls`
    /// will notice a file another app deleted. So the index is clamped rather
    /// than debug-asserted: this is a value off a rescan, and the one thing it
    /// must not do is take the shell down. Clamping to `len - 1` (the last real
    /// candidate) rather than to 0 is the other half of that: 0 would silently
    /// re-select the *first* wallpaper, which looks like the setting changed.
    pub fn new(candidates: Vec<String>, cursor: usize) -> Self {
        let cursor = if candidates.is_empty() {
            0
        } else {
            cursor.min(candidates.len() - 1)
        };
        Self { candidates, cursor }
    }

    /// `true` when there is nothing to choose from.
    ///
    /// A distinct method rather than `len() == 0` because the shell asks this
    /// question at every gesture and `is_empty` is the name the reader wants.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// How many candidates there are in total, across every page.
    #[inline]
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// The four candidates on the cursor's page, padded with `None`.
    ///
    /// `None` is an empty slot: the renderer draws a placeholder in it -- the
    /// reference leaves `ic_deepshortcut_placeholder` in a card whose image
    /// failed to load (`WallpaperCarouselView.kt:141-153`) rather than drawing
    /// nothing -- and so `move_by` has to refuse to put the cursor there.
    ///
    /// Fixed-capacity and allocation-free, so it is safe to call per frame.
    /// `Copy` out of the four slots rather than cloned `String`s.
    #[inline]
    pub fn page(&self) -> [Option<&str>; SLOTS] {
        self.candidates_at(self.page_index() * SLOTS)
    }

    /// The `SLOTS` candidates starting at list index `first`.
    ///
    /// Split out of [`Self::page`] so a caller that wants to paint page *n*
    /// directly -- which is what a scrolled-back-to-top gesture needs, and what
    /// keeps the frame path from recomputing `page_index() * SLOTS` -- reads one
    /// loop rather than re-deriving the arithmetic.
    #[inline]
    pub fn candidates_at(&self, first: usize) -> [Option<&str>; SLOTS] {
        let mut out: [Option<&str>; SLOTS] = [None; SLOTS];
        for (slot, cell) in out.iter_mut().enumerate() {
            // `get`, not indexing: `first` is `page_index() * SLOTS` and the last
            // page is short, so the tail of this window is out of range by
            // construction and is exactly the padding.
            *cell = self.candidates.get(first + slot).map(String::as_str);
        }
        out
    }

    /// Zero-based index of the cursor's page.
    ///
    /// Zero for an empty picker, where it is not a page index into anything --
    /// the same "unset, not 0" reading [`Self::position`] gives.
    #[inline]
    pub fn page_index(&self) -> usize {
        self.cursor / SLOTS
    }

    /// How many pages there are, or `0` when there are no candidates.
    ///
    /// Zero rather than one for an empty list is deliberate and is the renderer's
    /// signal: `settings::build_paged` writes `of: 0` and `super::build` leaves
    /// both position fields `None` for a picker with nothing in it, which is
    /// what "not a picker yet" means. A page count of 1 would tell the renderer
    /// there is a page of nothing to draw.
    #[inline]
    pub fn page_count(&self) -> usize {
        self.candidates.len().div_ceil(SLOTS)
    }

    /// Move the cursor by `delta` candidates, wrapping. `true` if it moved.
    ///
    /// `delta` is a candidate count and not a sign: a long-press steps by `-1`
    /// and a page tap steps by `SLOTS`, and both go through here so there is
    /// one place where wrapping is decided. It may be any `i32`, including one
    /// larger than the list or `i32::MIN`; the reduction is done in `i64`, so
    /// neither overflows.
    ///
    /// # Why this cannot land on a padding slot
    ///
    /// The brief for this function was "must not silently land the cursor on a
    /// padding slot on the last page", and the obvious way to honour it is a
    /// check: compute the target, refuse it if `page()[target % SLOTS].is_none()`.
    /// That check is unnecessary, and this is why. `len` is the modulus, so
    /// `target` is reduced into `0..len` and *every* index in that range is a
    /// real candidate -- `page()` only yields `None` for indices `>= len`. The
    /// cursor invariant `cursor < len` then holds after the move by
    /// construction, so a picker that highlights an empty cell is not something
    /// this function can be asked to do.
    ///
    /// Which also settles wrap-or-clamp. Both keep the cursor off the padding,
    /// so the tie-break has to be something else: this wraps. The reference's
    /// carousel has no dead end either -- every card is live to a touch
    /// (`WallpaperCarouselView.kt:108-115`), and the first card is reachable
    /// from the last by the same gesture that reaches the last from the first.
    /// A clamp would leave the first wallpaper reachable only by starting there,
    /// which is a control that stops working at one end and reads as a bug.
    ///
    /// Returns `false`, having changed nothing, in the two cases where there is
    /// no move to make: an empty list, and a target equal to the current cursor
    /// (`delta == 0`, or a `delta` that is a whole number of laps). `false` means
    /// "do not mark the state dirty", which is the existing
    /// [`super::apply`] convention and the reason it is spelled as a no-op
    /// rather than as success.
    pub fn move_by(&mut self, delta: i32) -> bool {
        if self.candidates.is_empty() {
            return false;
        }
        let len = self.candidates.len() as i64;
        let target = (self.cursor as i64 + delta as i64).rem_euclid(len) as usize;
        if target == self.cursor {
            return false;
        }
        self.cursor = target;
        true
    }

    /// The chosen path, or `None` when there is nothing chosen.
    ///
    /// `None` only for an empty picker: within it the cursor always names a
    /// real candidate, which is the invariant [`Self::new`] establishes and
    /// [`Self::move_by`] preserves.
    ///
    /// # The shell commits on the first tap, not the second
    ///
    /// The reference requires two: `WallpaperCarouselView.kt:112` reaches
    /// `setWallpaper` only on a touch of the *already-selected* card, so a tap
    /// previews and a second tap commits. The shell diverges, and deliberately.
    ///
    /// The reference's first tap changes the carousel to show that wallpaper full
    /// bleed -- the preview *is* the feedback, because the thing being configured
    /// is on screen. UTLC's wallpaper row is one line in a settings list with no
    /// preview beside it, so under the two-tap rule the first tap would change
    /// nothing a user can see except a counter going from "1 of 4" to "2 of 4",
    /// and the second would be required before anything happened at all. That is
    /// a control that looks broken, which is the exact failure this module's
    /// `every_row_is_reachable_and_no_row_is_a_dead_tap` test exists to prevent --
    /// the same reasoning that demoted the `auto-rotate` row.
    ///
    /// The divergence is forced by the UI shape, not chosen for convenience. If
    /// the row ever gains a thumbnail preview, the two-tap model becomes the
    /// better one and this is the note to revisit.
    #[inline]
    pub fn selected(&self) -> Option<&str> {
        self.candidates.get(self.cursor).map(String::as_str)
    }

    /// 1-based position for display: `Some(3)` for the third of seven.
    ///
    /// `None` when there is nothing selected, so a caller can distinguish "page
    /// 1 of 7" from "nothing to choose" with one check instead of testing
    /// `selected()`. One-based because that is how the reference labels a
    /// selected card to a screen reader -- `contentDescription` is set from the
    /// item, not the 0-based index -- and because "3 of 7" is the only form that
    /// reads correctly; `at`/`of` are rendered verbatim by the settings panel
    /// and a 0 would put a `0 of 7` in front of the user.
    #[inline]
    pub fn position(&self) -> Option<u32> {
        if self.candidates.is_empty() {
            return None;
        }
        // `cursor < len` and `len <= MAX_FOLDER_ITEMS`-style small, so the `as`
        // cannot truncate. Kept as a `u32` because that is what `SettingRow::at`
        // holds and the two must not disagree about the type of the number.
        Some(self.cursor as u32 + 1)
    }

    /// The number of candidates on the cursor's page, for a "4 of 7" style
    /// trailing hint.
    ///
    /// Not `page_count() * SLOTS`: the last page is short, and a hint claiming
    /// eight items when the page holds three is the same lie as a cursor on an
    /// empty cell.
    #[inline]
    pub fn page_len(&self) -> usize {
        let on = self.cursor / SLOTS * SLOTS;
        (self.candidates.len() - on).min(SLOTS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` distinguishable candidate paths, as the shell would hand them over.
    fn cands(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("/data/wallpapers/{i:02}.png"))
            .collect()
    }

    /// A page is exactly `SLOTS` cells, and a short last page is padded with
    /// `None` rather than with a hole in the return type.
    #[test]
    fn a_page_is_four_slots_padded_with_nothing() {
        let p = WallpaperPicker::new(cands(7), 0);
        assert_eq!(SLOTS, 4);
        let page = p.page();
        assert_eq!(page.len(), SLOTS, "the renderer indexes a fixed window");
        assert_eq!(
            page,
            [
                Some("/data/wallpapers/00.png"),
                Some("/data/wallpapers/01.png"),
                Some("/data/wallpapers/02.png"),
                Some("/data/wallpapers/03.png")
            ]
        );

        // The last page holds three real candidates and one placeholder. This is
        // the shape a `Vec` return type could not express at all.
        let p = WallpaperPicker::new(cands(7), 6);
        assert_eq!(p.page_index(), 1);
        assert_eq!(
            p.page(),
            [
                Some("/data/wallpapers/04.png"),
                Some("/data/wallpapers/05.png"),
                Some("/data/wallpapers/06.png"),
                None
            ],
            "the tail of the last page is padding, not a hole in the array"
        );
        assert_eq!(p.page_len(), 3, "a page of three does not claim four");

        // A window starts at its page's *first* index, not at the cursor, so the
        // cursor can sit in slot 0 of a page that has three placeholders after it.
        let p = WallpaperPicker::new(cands(5), 4);
        assert_eq!(
            p.page(),
            [Some("/data/wallpapers/04.png"), None, None, None],
            "one candidate, three placeholders"
        );
        assert_eq!(p.page_len(), 1);
    }

    /// The cursor starts where the caller asked, and `page_index` follows it.
    #[test]
    fn the_page_index_follows_the_cursor() {
        let p = WallpaperPicker::new(cands(9), 0);
        assert_eq!(p.page_count(), 3, "nine is three pages of four");
        assert_eq!(p.page_index(), 0);
        let p = WallpaperPicker::new(cands(9), 4);
        assert_eq!(p.page_index(), 1);
        let p = WallpaperPicker::new(cands(9), 8);
        assert_eq!(p.page_index(), 2);
        assert_eq!(p.page_len(), 1, "the last page of nine holds one");
    }

    /// An exact multiple has no short page, and an empty list has no pages.
    /// The page counts are exact at the boundaries, and `len` is the *total*.
    #[test]
    fn page_counts_are_exact_at_the_boundaries() {
        assert_eq!(WallpaperPicker::new(cands(0), 0).page_count(), 0);
        assert_eq!(WallpaperPicker::new(cands(1), 0).page_count(), 1);
        assert_eq!(WallpaperPicker::new(cands(4), 3).page_count(), 1);
        assert_eq!(WallpaperPicker::new(cands(5), 4).page_count(), 2);
        assert_eq!(
            WallpaperPicker::new(cands(4), 3).page(),
            [
                Some("/data/wallpapers/00.png"),
                Some("/data/wallpapers/01.png"),
                Some("/data/wallpapers/02.png"),
                Some("/data/wallpapers/03.png")
            ],
            "a full last page has no padding to fall into"
        );

        // `len` counts every candidate on every page, and `is_empty` is about the
        // list rather than about the page the cursor happens to be on. The last
        // page of nine holds one candidate, so the two answer differently here --
        // which is the only place they can be told apart.
        let p = WallpaperPicker::new(cands(9), 8);
        assert_eq!(p.page_len(), 1, "one candidate on the cursor's page");
        assert_eq!(
            p.len(),
            9,
            "and nine in the list the picker is choosing from"
        );
        assert!(!p.is_empty());
        assert_eq!(WallpaperPicker::new(cands(0), 0).len(), 0);
        assert!(WallpaperPicker::new(cands(0), 0).is_empty());
    }

    /// Moving off the end wraps to the first candidate, and moving off the
    /// beginning wraps to the last. Neither end is a dead end.
    #[test]
    fn moving_wraps_at_both_ends() {
        let mut p = WallpaperPicker::new(cands(7), 6);
        assert!(p.move_by(1), "past the last candidate");
        assert_eq!(
            p.selected(),
            Some("/data/wallpapers/00.png"),
            "the first candidate is reachable from the last"
        );
        assert!(p.move_by(-1), "back off the front");
        assert_eq!(
            p.selected(),
            Some("/data/wallpapers/06.png"),
            "the last candidate is reachable from the first"
        );
    }

    /// The invariant this type exists to hold: the cursor is always on a real
    /// candidate, so no walk can ever highlight an empty slot.
    #[test]
    fn no_walk_from_any_start_ever_lands_on_a_padding_slot() {
        for len in [1usize, 3, 4, 5, 7, 8, 16] {
            for start in 0..len {
                let mut p = WallpaperPicker::new(cands(len), start);
                for step in 0..(len * 3) {
                    let slot = p.cursor % SLOTS;
                    assert_eq!(
                        p.page()[slot],
                        Some(format!("/data/wallpapers/{:02}.png", p.cursor).as_str()),
                        "len {len}, start {start}, step {step}: the cursor sits on \
                         padding"
                    );
                    assert!(
                        p.selected().is_some(),
                        "len {len}, start {start}, step {step}: nothing selected \
                         while a candidate exists"
                    );
                    p.move_by(1);
                }
            }
        }
    }

    /// A delta larger than the list, and `i32::MIN`, are both ordinary moves.
    ///
    /// The reduction is in `i64` precisely so `cursor + delta as i64` cannot
    /// overflow; a naive `cursor as i32 + delta` would wrap to a positive index
    /// and select the wrong wallpaper on a long-press.
    #[test]
    fn a_delta_larger_than_the_list_or_at_the_extremes_is_a_normal_move() {
        let mut p = WallpaperPicker::new(cands(7), 0);
        assert!(p.move_by(1_000));
        assert_eq!(
            p.selected(),
            Some("/data/wallpapers/06.png"),
            "1000 = 7 * 142 + 6"
        );
        let mut p = WallpaperPicker::new(cands(7), 0);
        assert!(p.move_by(i32::MIN));
        assert_eq!(
            p.selected(),
            Some("/data/wallpapers/05.png"),
            "i32::MIN == -(7 * 306783378 + 2), so it reduces to 5"
        );
        let mut p = WallpaperPicker::new(cands(7), 3);
        assert!(p.move_by(-1_000));
        assert_eq!(p.selected(), Some("/data/wallpapers/04.png"));
    }

    /// A move that changes nothing reports `false`, so the shell does not write
    /// the state file for a tap that moved no cursor.
    #[test]
    fn a_move_that_lands_where_it_started_reports_false() {
        let mut p = WallpaperPicker::new(cands(7), 2);
        assert!(!p.move_by(0), "a zero delta is not a move");
        assert_eq!(p.selected(), Some("/data/wallpapers/02.png"));
        assert!(!p.move_by(7), "a whole lap is not a move");
        assert_eq!(p.selected(), Some("/data/wallpapers/02.png"));
        assert!(!p.move_by(-7));
        assert_eq!(p.selected(), Some("/data/wallpapers/02.png"));
    }

    /// The candidate list can shrink under a stale cursor -- the user deletes a
    /// file in another app -- and the picker must clamp, not panic and not
    /// re-select the first candidate.
    #[test]
    fn a_cursor_past_the_end_clamps_to_the_last_candidate() {
        let p = WallpaperPicker::new(cands(3), 99);
        assert_eq!(
            p.selected(),
            Some("/data/wallpapers/02.png"),
            "clamped to the last, not wrapped to the first"
        );
        assert_eq!(p.position(), Some(3));
        assert_eq!(p.page()[2], Some("/data/wallpapers/02.png"));

        // Exactly one past the end is the interesting value: the off-by-one a
        // `len - 1` guard gets wrong.
        let p = WallpaperPicker::new(cands(3), 3);
        assert_eq!(p.selected(), Some("/data/wallpapers/02.png"));

        // And `usize::MAX`, which a `len - 1` computed on an empty list would
        // have underflowed on.
        let p = WallpaperPicker::new(cands(3), usize::MAX);
        assert_eq!(p.selected(), Some("/data/wallpapers/02.png"));
    }

    /// An empty list is a legal state -- a device with no wallpaper directory --
    /// and every accessor has to answer it without a panic.
    #[test]
    fn an_empty_picker_answers_every_question_without_panicking() {
        let mut p = WallpaperPicker::new(Vec::new(), 7);
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        assert_eq!(p.page(), [None; SLOTS]);
        assert_eq!(p.page_index(), 0);
        assert_eq!(p.page_count(), 0, "no candidates, no pages to draw");
        assert_eq!(p.page_len(), 0);
        assert_eq!(p.selected(), None, "nothing to commit");
        assert_eq!(p.position(), None, "so the row has no \"3 of 7\"");
        assert!(!p.move_by(1));
        assert!(!p.move_by(-1));
        assert!(!p.move_by(i32::MAX));
        // And it is still empty afterwards.
        assert!(p.is_empty());
    }

    /// `position` is 1-based and agrees with `selected`, so the two cannot be
    /// used together and disagree about which wallpaper is chosen.
    #[test]
    fn position_is_one_based_and_never_disagrees_with_the_selection() {
        let mut p = WallpaperPicker::new(cands(7), 0);
        for want in 1..=7u32 {
            assert_eq!(p.position(), Some(want));
            let n = want as usize - 1;
            assert_eq!(
                p.selected(),
                Some(format!("/data/wallpapers/{n:02}.png").as_str()),
                "position {want} does not describe the selection"
            );
            p.move_by(1);
        }
    }

    /// Every candidate is reachable from every start, so the list can be scanned
    /// by moving rather than by jumping.
    #[test]
    fn every_candidate_is_reachable_from_every_start() {
        for len in [1usize, 4, 7, 13] {
            for start in 0..len {
                let mut p = WallpaperPicker::new(cands(len), start);
                let mut seen = vec![false; len];
                for _ in 0..len {
                    seen[p.cursor] = true;
                    p.move_by(1);
                }
                assert!(
                    seen.iter().all(|s| *s),
                    "len {len} start {start}: some candidate is unreachable by \
                     moving forwards"
                );
                assert_eq!(
                    p.cursor, start,
                    "len {len} start {start}: a full lap is a lap"
                );
            }
        }
    }

    /// `candidates_at` is `page` for the cursor's page, and is safe for a window
    /// that starts past the end.
    #[test]
    fn a_window_past_the_end_is_all_padding() {
        let p = WallpaperPicker::new(cands(2), 1);
        assert_eq!(
            p.page(),
            p.candidates_at(0),
            "page is the window at page_index * SLOTS"
        );
        assert_eq!(
            p.candidates_at(usize::MAX / 2),
            [None; SLOTS],
            "a wildly out-of-range window must not overflow into a candidate"
        );
        assert_eq!(p.candidates_at(2), [None; SLOTS]);
    }
}
