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
    /// the same reasoning that used to demote the `auto-rotate` row to `Text`
    /// before its modeset path existed.
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

/// Ceiling on what the wallpaper probe may transiently allocate, bytes.
///
/// The seed is one `u32` that selects a palette. What it costs to produce one
/// is a full inflate of the image, because PNG has no random access:
/// `crate::graphics::png::decode_working_set_estimate` puts that at about
/// 9 bytes per pixel. 8 MiB is the transient headroom between the shell's
/// ~3.1 MiB steady RSS and the plan's 15 MiB ceiling
/// (`crates/utlc/src/main.rs:9734`, `WALLPAPER_PROBE_BUDGET`). This is the
/// same number, hoisted here so the shell's file picker and its probe agree
/// about what "affordable" means without re-deriving it.
pub const WALLPAPER_PROBE_BUDGET: u64 = 8 * 1024 * 1024;

/// How many leading bytes the shell should read to validate a candidate
/// without decoding it.
///
/// 33 covers PNG (`crate::graphics::png::header_size` needs signature +
/// `IHDR`) and 30 covers WebP's `RIFF....WEBP` + chunk header; JPEG's `SOF`
/// can sit several kilobytes in (after `APPn`/`DQT`/`DHT`), so 4 KiB is the
/// bound that makes [`validate_image_prefix`] useful for all three without a
/// full read. Off the frame path: the picker never reads, the shell does.
pub const WALLPAPER_PREFIX_LEN: usize = 4096;

/// Whether `path` names a file the picker can ever accept, by extension alone.
///
/// `png`/`jpg`/`jpeg`/`webp`, case-insensitive, matching the shell's
/// `wallpaper_candidates` (`crates/utlc/src/main.rs:1306`) which today is
/// png-only and the file picker which must not be. Pure string check, no
/// filesystem access: the shell owns enumeration and hands candidates in,
/// which is what keeps this module FS-agnostic.
///
/// A `true` here is not acceptance: the shell must still read
/// [`WALLPAPER_PREFIX_LEN`] bytes and pass them to
/// [`validate_image_prefix`], which checks the magic and the probe budget
/// without decoding. Extension first (cheap reject), magic second (no
/// spoofed suffix), decode never on the frame path.
pub fn validate_image_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    let ext = &name[dot + 1..];
    ext.eq_ignore_ascii_case("png")
        || ext.eq_ignore_ascii_case("jpg")
        || ext.eq_ignore_ascii_case("jpeg")
        || ext.eq_ignore_ascii_case("webp")
}

/// Dimensions of a candidate from its leading bytes, if affordable.
///
/// Tries PNG (`crate::graphics::png::header_size`, which verifies the `IHDR`
/// CRC), then JPEG `SOF`, then WebP `VP8`/`VP8L`/`VP8X`, in that order, and
/// returns `None` when the prefix is not a supported image or when
/// `crate::graphics::png::decode_working_set_estimate(w, h)` exceeds
/// [`WALLPAPER_PROBE_BUDGET`]. No allocation, no decode: safe to call with
/// the shell's prefix buffer.
///
/// `None` covers both "not an image" and "too big to probe": the shell skips
/// the file either way (`crates/utlc/src/main.rs:9774-9780`), so
/// distinguishing them would only add a branch the caller cannot act on.
pub fn validate_image_prefix(prefix: &[u8]) -> Option<(u32, u32)> {
    let (w, h) = image_dimensions(prefix)?;
    if crate::graphics::png::decode_working_set_estimate(w, h) > WALLPAPER_PROBE_BUDGET {
        return None;
    }
    Some((w, h))
}

/// Sort and dedupe a candidate list in place.
///
/// The shell's `wallpaper_candidates` (`crates/utlc/src/main.rs:1328-1329`)
/// does `out.sort(); out.dedup();` after enumeration; this is that step as a
/// pure helper so the file-picker import path and the boot enumeration share
/// one ordering. No filesystem access: takes the `Vec<String>` the shell
/// already owns.
pub fn dedupe_candidates(candidates: &mut Vec<String>) {
    candidates.sort();
    candidates.dedup();
}

fn image_dimensions(prefix: &[u8]) -> Option<(u32, u32)> {
    if let Some(wh) = crate::graphics::png::header_size(prefix) {
        return Some(wh);
    }
    if let Some(wh) = jpeg_dimensions(prefix) {
        return Some(wh);
    }
    webp_dimensions(prefix)
}

fn jpeg_dimensions(prefix: &[u8]) -> Option<(u32, u32)> {
    if prefix.len() < 4 || prefix[0] != 0xFF || prefix[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    while pos + 1 < prefix.len() {
        if prefix[pos] != 0xFF {
            return None;
        }
        let mut m = pos + 1;
        while m < prefix.len() && prefix[m] == 0xFF {
            m += 1;
        }
        if m >= prefix.len() {
            return None;
        }
        let marker = prefix[m];
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            pos = m + 1;
            continue;
        }
        if marker == 0xDA {
            return None;
        }
        if m + 2 >= prefix.len() {
            return None;
        }
        let len = u16::from_be_bytes([prefix[m + 1], prefix[m + 2]]) as usize;
        if len < 2 {
            return None;
        }
        let is_sof = matches!(
            marker,
            0xC0 | 0xC1
                | 0xC2
                | 0xC3
                | 0xC5
                | 0xC6
                | 0xC7
                | 0xC9
                | 0xCA
                | 0xCB
                | 0xCD
                | 0xCE
                | 0xCF
        );
        if is_sof {
            if m + 7 >= prefix.len() {
                return None;
            }
            let h = u16::from_be_bytes([prefix[m + 4], prefix[m + 5]]) as u32;
            let w = u16::from_be_bytes([prefix[m + 6], prefix[m + 7]]) as u32;
            if w == 0 || h == 0 {
                return None;
            }
            return Some((w, h));
        }
        let next = m.checked_add(1)?.checked_add(len)?;
        if next <= m || next > prefix.len() {
            return None;
        }
        // A zero-length advance would spin; `len >= 2` plus the marker byte
        // guarantees `next > pos`, but assert it structurally.
        if next <= pos {
            return None;
        }
        pos = next;
    }
    None
}

fn webp_dimensions(prefix: &[u8]) -> Option<(u32, u32)> {
    if prefix.len() < 12 || &prefix[0..4] != b"RIFF" || &prefix[8..12] != b"WEBP" {
        return None;
    }
    if prefix.len() < 20 {
        return None;
    }
    let fourcc = &prefix[12..16];
    if fourcc == b"VP8 " {
        if prefix.len() < 30 {
            return None;
        }
        if prefix[23] != 0x9D || prefix[24] != 0x01 || prefix[25] != 0x2A {
            return None;
        }
        let w = u16::from_le_bytes([prefix[26], prefix[27]]) as u32 & 0x3FFF;
        let h = u16::from_le_bytes([prefix[28], prefix[29]]) as u32 & 0x3FFF;
        if w == 0 || h == 0 {
            return None;
        }
        Some((w, h))
    } else if fourcc == b"VP8L" {
        if prefix.len() < 25 {
            return None;
        }
        if prefix[20] != 0x2F {
            return None;
        }
        let bits = u32::from_le_bytes([prefix[21], prefix[22], prefix[23], prefix[24]]);
        let w = (bits & 0x3FFF) + 1;
        let h = ((bits >> 14) & 0x3FFF) + 1;
        if w == 0 || h == 0 || w > 16384 || h > 16384 {
            return None;
        }
        Some((w, h))
    } else if fourcc == b"VP8X" {
        if prefix.len() < 30 {
            return None;
        }
        let w = (prefix[24] as u32 | (prefix[25] as u32) << 8 | (prefix[26] as u32) << 16) + 1;
        let h = (prefix[27] as u32 | (prefix[28] as u32) << 8 | (prefix[29] as u32) << 16) + 1;
        if w == 0 || h == 0 {
            return None;
        }
        Some((w, h))
    } else {
        None
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

    /// The extension gate the shell's file picker needs: `png` today,
    /// `jpg`/`jpeg`/`webp` once it imports, nothing else. Pure string check,
    /// no filesystem, so the module stays FS-agnostic.
    #[test]
    fn validate_image_path_allows_only_image_extensions() {
        for ok in [
            "/data/wallpapers/00.png",
            "/usr/share/backgrounds/a.JPG",
            "b.jpeg",
            "c.JPEG",
            "/run/user/1000/d.webp",
            "/run/user/1000/e.WEBP",
            "relative/path/f.png",
        ] {
            assert!(validate_image_path(ok), "{ok} should be accepted");
        }
        for bad in [
            "",
            "noextension",
            "/data/wallpapers/00.png.txt",
            "/data/wallpapers/00.gif",
            "/data/wallpapers/00.bmp",
            "/data/wallpapers/.hidden",
            "/data/wallpapers/png",
            "foo.",
            ".png",
        ] {
            // `.png` as a bare dotfile has an empty stem but a `png`
            // extension by `rfind('.')`, so it is accepted by construction;
            // assert the rest are rejected and pin `.png` as accepted.
            if bad == ".png" {
                assert!(validate_image_path(bad), ".png has a png extension");
            } else {
                assert!(!validate_image_path(bad), "{bad} should be rejected");
            }
        }
        // The directory part never contributes an extension.
        assert!(!validate_image_path("/data.png.dir/noext"));
        assert!(validate_image_path("/data.png.dir/ok.jpg"));
    }

    /// Magic + dimensions + budget without decoding. PNG via
    /// `header_size` (CRC-verified), JPEG via `SOF`, WebP via
    /// `VP8/VP8L/VP8X`, all gated by `WALLPAPER_PROBE_BUDGET`.
    #[test]
    fn validate_image_prefix_checks_magic_and_budget() {
        assert_eq!(WALLPAPER_PROBE_BUDGET, 8 * 1024 * 1024);
        const _: () = assert!(WALLPAPER_PREFIX_LEN >= 33);
        // PNG: build a real one with the crate's own encoder so the IHDR CRC
        // is valid by construction rather than by a hardcoded blob.
        let img = crate::graphics::png::RgbaImage {
            width: 4,
            height: 4,
            pixels: vec![7u8; 4 * 4 * 4],
        };
        let png = crate::graphics::png::encode_png(&img).expect("encodes");
        assert_eq!(validate_image_prefix(&png), Some((4, 4)));
        // JPEG: SOI + SOF0 declaring 32x16. Length 11 = 2 + 9 payload.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x20, 0x01, 0x01, 0x11,
            0x00, 0xFF, 0xD9,
        ];
        assert_eq!(validate_image_prefix(&jpeg), Some((32, 16)));
        // JPEG with APP0 before SOF: the scan must skip it.
        let mut jpeg_app: Vec<u8> =
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x06, b'J', b'F', b'I', b'F'];
        jpeg_app.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x20]);
        jpeg_app.extend_from_slice(&[0x01, 0x01, 0x11, 0x00, 0xFF, 0xD9]);
        assert_eq!(validate_image_prefix(&jpeg_app), Some((32, 16)));
        // WebP lossy: RIFF + VP8 with 9D 01 2A start code, 32x16.
        let mut vp8 = vec![0u8; 30];
        vp8[0..4].copy_from_slice(b"RIFF");
        vp8[8..12].copy_from_slice(b"WEBP");
        vp8[12..16].copy_from_slice(b"VP8 ");
        vp8[23] = 0x9D;
        vp8[24] = 0x01;
        vp8[25] = 0x2A;
        vp8[26..28].copy_from_slice(&32u16.to_le_bytes());
        vp8[28..30].copy_from_slice(&16u16.to_le_bytes());
        assert_eq!(validate_image_prefix(&vp8), Some((32, 16)));
        // WebP lossless: 0x2F signature + 14+14 bit dims (31x15 stored as 30,14).
        let mut vp8l = vec![0u8; 25];
        vp8l[0..4].copy_from_slice(b"RIFF");
        vp8l[8..12].copy_from_slice(b"WEBP");
        vp8l[12..16].copy_from_slice(b"VP8L");
        vp8l[20] = 0x2F;
        let bits: u32 = 30 | (14 << 14);
        vp8l[21..25].copy_from_slice(&bits.to_le_bytes());
        assert_eq!(validate_image_prefix(&vp8l), Some((31, 15)));
        // WebP extended: canvas 32x16 stored as 31,15 in 24-bit LE.
        let mut vp8x = vec![0u8; 30];
        vp8x[0..4].copy_from_slice(b"RIFF");
        vp8x[8..12].copy_from_slice(b"WEBP");
        vp8x[12..16].copy_from_slice(b"VP8X");
        vp8x[24] = 31;
        vp8x[27] = 15;
        assert_eq!(validate_image_prefix(&vp8x), Some((32, 16)));
        // Garbage, truncated, and wrong magic are all None.
        assert_eq!(validate_image_prefix(&[]), None);
        assert_eq!(validate_image_prefix(b"not an image"), None);
        assert_eq!(validate_image_prefix(&[0xFF, 0xD8]), None);
        assert_eq!(validate_image_prefix(&vp8[..20]), None);
        // Over budget: a JPEG claiming 5000x5000 is ~225 MiB working set.
        let mut huge = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08];
        huge.extend_from_slice(&5000u16.to_be_bytes());
        huge.extend_from_slice(&5000u16.to_be_bytes());
        huge.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        assert_eq!(validate_image_prefix(&huge), None);
        // PNG over budget via dimensions alone: 5000x5000 estimate exceeds it.
        assert!(
            crate::graphics::png::decode_working_set_estimate(5000, 5000) > WALLPAPER_PROBE_BUDGET
        );
        // Spoofed suffix: PNG extension but JPEG magic still resolves by magic.
        assert!(validate_image_path("/tmp/evil.png"));
        assert_eq!(validate_image_prefix(&jpeg), Some((32, 16)));
    }

    /// The probe budget boundary, from the shell's side: a 1280x720 claim
    /// (~7.9 MiB working set) is affordable and resolves, while a 1440x900
    /// claim (~11.1 MiB) is refused. Adjacent answers from the same gate, so
    /// a budget that drifts in either direction fails one side or the other.
    #[test]
    fn validate_image_prefix_accepts_just_under_budget_and_refuses_just_over() {
        fn jpeg_claim(w: u16, h: u16) -> Vec<u8> {
            let mut v = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08];
            v.extend_from_slice(&h.to_be_bytes());
            v.extend_from_slice(&w.to_be_bytes());
            v.extend_from_slice(&[0x01, 0x01, 0x11, 0x00, 0xFF, 0xD9]);
            v
        }
        assert_eq!(
            validate_image_prefix(&jpeg_claim(1280, 720)),
            Some((1280, 720))
        );
        assert_eq!(validate_image_prefix(&jpeg_claim(1440, 900)), None);
    }

    /// The sort+dedup the shell's `wallpaper_candidates` does after
    /// enumeration, as the shared helper the import path also uses.
    #[test]
    fn dedupe_candidates_sorts_and_dedupes() {
        let mut v = vec![
            "/b.png".to_string(),
            "/a.png".to_string(),
            "/b.png".to_string(),
            "/c.jpg".to_string(),
            "/a.png".to_string(),
        ];
        dedupe_candidates(&mut v);
        assert_eq!(v, vec!["/a.png", "/b.png", "/c.jpg"]);
        let mut empty: Vec<String> = Vec::new();
        dedupe_candidates(&mut empty);
        assert!(empty.is_empty());
        let mut single = vec!["/only.webp".to_string()];
        dedupe_candidates(&mut single);
        assert_eq!(single, vec!["/only.webp"]);
    }
}
