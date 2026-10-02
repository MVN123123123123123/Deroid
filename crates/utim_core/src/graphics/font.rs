//! High-definition vector typography engine for the UTLC shell.
//!
//! The launcher used to blit an ancient 8x16 bitmap font: every glyph was a
//! staircase of hard pixels, lowercase descenders were chopped off and there
//! was no real weight axis. This module replaces it with a genuine outline
//! type system:
//!
//! * **Outlines, not bitmaps.** Every glyph is a centreline skeleton of
//!   quadratic beziers in a 1000 unit/em design space (cap height 700,
//!   x-height 520, ascender 740, descender -200), so curves stay smooth at
//!   any size and the metrics are proportional (real side bearings, tabular
//!   digits, no fake monospace grid).
//! * **Real weights.** `FontWeight` selects the pen radius of the stroke, the
//!   same way a variable font's `wght` axis thins and thickens stems, giving
//!   distinct Regular / Medium / Bold text without duplicating any outline.
//! * **Analytic anti-aliasing.** Coverage is derived from the true distance
//!   between the pixel centre and the outline, giving a one pixel wide
//!   analytic ramp instead of the old corner-smoothing hack.
//! * **Zero heap on the hot path.** The glyph table lives in `.rodata`, the
//!   per-scanline coverage accumulator is a fixed 256 byte stack array, a
//!   fixed-size 2-way coverage cache in `.bss` absorbs redraws (keyed on the
//!   exact size and origin bits, so a hit is bit-identical), and curves are
//!   flattened on the fly. The hot path allocates nothing, ever; only
//!   oversize glyphs outside the cache fall back to a transient buffer.

use super::ttf::TrueTypeFont;
use std::sync::{Mutex, OnceLock};

/// Design units per em (the font is authored in this space).
pub const UNITS_PER_EM: f32 = 1000.0;
/// Ascender line, in design units above the baseline.
pub const ASCENDER: f32 = 740.0;
/// Descender line, in design units below the baseline.
pub const DESCENDER: f32 = -200.0;
/// Cap height, in design units.
pub const CAP_HEIGHT: f32 = 700.0;
/// Lowercase x-height, in design units.
pub const X_HEIGHT: f32 = 520.0;
/// Em size in pixels for UI scale 1.
///
/// **This is the scale-1 em, not the body size.** The shell draws body text at
/// scale 2 ([`em_px`] multiplies by `max(scale, 1)`), so the body em on the
/// reference panel is `2 * EM_BASE_PX`.
///
/// The body size is anchored to Lawnchair's `textAppearanceBodyMedium`, which
/// is 14 sp (`values/styles.xml`, the type scale Android 12+ inherited). At
/// the calibration density of 1080 px / 420 dp = 2.5714 px/dp, 14 sp is
/// `14 * 2.5714 = 36.0` px. So `EM_BASE_PX` is 18.0, and `em_px(2)` is 36.0.
///
/// It was 15.0, giving a 30 px body em -- 11.7 sp. That is visibly small for a
/// launcher label on a 1080 px panel: it is the size of a footnote at 12 sp,
/// and the plan's 6.3 calls it out as "labels and popup menu text render
/// legibly at true mobile scale without colliding".
///
/// A note on the plan's wording, which says to raise *this* constant "from
/// 15.0 to 36.0 px". 36 is the size of the *body em*, and setting the scale-1
/// base to 36 would make `em_px(2)` 72 px -- a 28 sp display type, twice the
/// reference body size, and larger than most of the panel's chrome. The two
/// numbers are the same measurement at two scales; the base is half of it.
pub const EM_BASE_PX: f32 = 18.0;

/// The reference body em in pixels on the 1080 px panel: 14 sp at 2.5714
/// px/dp. Asserted in the tests so the two constants cannot drift apart.
pub const REFERENCE_BODY_EM_PX: f32 = 36.0;

/// Scanline coverage accumulator width, in pixels. Glyphs wider than this
/// are rasterised in horizontal chunks, so any size stays correct.
const ROW_TILE: usize = 256;
/// Quadratic flattening resolution. 8 chords keep the distance error of the
/// analytic coverage well below a tenth of a pixel at any UI size.
const FLAT: usize = 8;

/// Material Design 3 type-scale weight axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontWeight {
    Regular,
    Medium,
    Bold,
}

impl FontWeight {
    /// Pen radius (half stem width) in design units.
    #[inline]
    pub const fn pen(self) -> f32 {
        match self {
            FontWeight::Regular => 55.0,
            FontWeight::Medium => 65.0,
            FontWeight::Bold => 78.0,
        }
    }
}

/// One quadratic bezier of a glyph skeleton. The segment starts where the
/// previous one ended (or at the sub-path origin) and ends at `(x, y)`.
#[derive(Clone, Copy)]
pub struct Q(pub f32, pub f32, pub f32, pub f32);

/// A sub-path: origin plus a chain of quadratics. Dots (`.`, `i`, `:`) are
/// degenerate chains, rendered as discs by the round pen.
#[derive(Clone, Copy)]
pub struct Sub {
    pub s: (f32, f32),
    pub p: &'static [Q],
}

/// A glyph: advance width in design units plus its sub-paths.
#[derive(Clone, Copy)]
pub struct Glyph {
    pub a: f32,
    pub sub: &'static [Sub],
}

/// Supported typography font families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum FontFamily {
    #[default]
    NotoSans = 0,
    Homemade = 1,
    AsciiMono = 2,
}

use core::sync::atomic::{AtomicU8, Ordering};
static ACTIVE_FAMILY: AtomicU8 = AtomicU8::new(0);
static FORCE_FALLBACK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static NOTO_TTF: OnceLock<Option<TrueTypeFont>> = OnceLock::new();

/// Set whether to force fallback to the home-made typography engine (e.g. for testing fallback behavior).
pub fn set_force_fallback(force: bool) {
    FORCE_FALLBACK.store(force, Ordering::Relaxed);
}

/// Returns the loaded Google Noto Sans TrueType font if available, or None if fallback is active.
pub fn get_noto_ttf() -> Option<&'static TrueTypeFont> {
    if FORCE_FALLBACK.load(Ordering::Relaxed) {
        return None;
    }
    NOTO_TTF
        .get_or_init(TrueTypeFont::load_system_noto)
        .as_ref()
}

/// Check whether the authentic Google Noto Sans TTF is loaded from system packages.
pub fn is_noto_ttf_loaded() -> bool {
    get_noto_ttf().is_some()
}

/// Set the globally active font family for UTLC rendering.
#[inline]
pub fn set_active_family(family: FontFamily) {
    ACTIVE_FAMILY.store(family as u8, Ordering::Relaxed);
}

/// Get the currently active font family.
#[inline]
pub fn active_family() -> FontFamily {
    match ACTIVE_FAMILY.load(Ordering::Relaxed) {
        1 => FontFamily::Homemade,
        2 => FontFamily::AsciiMono,
        _ => FontFamily::NotoSans,
    }
}

include!("font_glyphs.rs");

/// Fallback advance for code points outside the Latin-1 table.
const FALLBACK_ADVANCE: f32 = 600.0;

/// Lookup glyph for a specific font family.
#[inline]
pub fn glyph_for_family(b: u8, family: FontFamily) -> &'static Glyph {
    let table: &'static [Glyph; 96] = match family {
        FontFamily::NotoSans => &NOTO_GLYPHS,
        FontFamily::Homemade => &HOMEMADE_GLYPHS,
        FontFamily::AsciiMono => &ASCII_MONO_GLYPHS,
    };
    let i = (b.wrapping_sub(0x20) as usize).min(table.len() - 1);
    &table[i]
}

#[inline]
#[allow(dead_code)]
fn glyph_of(b: u8) -> &'static Glyph {
    glyph_for_family(b, active_family())
}

/// Em size in pixels for a UI scale factor.
#[inline]
pub fn em_px(scale: usize) -> f32 {
    EM_BASE_PX * scale.max(1) as f32
}

/// Em size in pixels for a UI scale on a panel `panel_w` pixels wide.
///
/// The shell's type scale is authored against a 1080px panel; every other
/// resolution scales proportionally from there, with a floor so text stays
/// legible on a small panel instead of collapsing to a few pixels. This is the
/// same contract the layout uses for every other dimension, so type and
/// geometry can never disagree about how big the screen is.
#[inline]
pub fn em_px_at(scale: usize, panel_w: usize) -> f32 {
    let k = (panel_w as f32 / REFERENCE_PANEL_W).clamp(0.5, 2.0);
    em_px(scale) * k
}

/// Panel width the UI scale is authored against.
pub const REFERENCE_PANEL_W: f32 = 1080.0;

/// The launcher body text size, in sp.
///
/// `textAppearanceBodyMedium` in Lawnchair's `values/styles.xml`, and the
/// Android 12+ type scale's default label size. 14 sp.
pub const REFERENCE_BODY_SP: f32 = 14.0;

/// Upper bound on the body em, px.
///
/// Android clamps `scaledDensity` so that a very large display cannot scale
/// text without limit; a tablet that is wide because it is a tablet, not
/// because it is close, must not get display-1 type for a label. 40 px is
/// roughly 15.5 sp at the 1080p density, which is the largest a launcher
/// label is normally set.
pub const MAX_BODY_EM_PX: f32 = 40.0;

/// The dp scale of a panel `w` px wide.
///
/// `LAWNCHAIR_PHONE_DP_WIDTH` is the calibration baseline, so this is 1.0 on
/// the 420 dp reference phone and 2.5714 on a 1080 px panel.
#[inline]
pub fn dp_scale_for(panel_w: f32) -> f32 {
    (panel_w / crate::graphics::layout::LAWNCHAIR_PHONE_DP_WIDTH).max(0.5)
}

/// The body em a panel of width `w` px should use, px: 14 sp at the panel's
/// dp scale, clamped to [`MAX_BODY_EM_PX`].
///
/// This is the *target*. The renderer rasterises at an integer rung of the
/// [`em_px`] ladder, so a caller picks the rung nearest to this and then reads
/// back `em_px_at(rung, w)` -- which is what the layout does. Keeping the
/// target and the rung separate is what lets the two agree exactly on the
/// reference panel (1080 px -> 36.0 px, exactly 14 sp at 2.5714 px/dp) while
/// degrading to the nearest available rung elsewhere.
///
/// The clamp is what stops a 2000 px-wide tablet from asking for
/// `14 * 4.76 = 66.7` px of label.
#[inline]
pub fn body_em_px_for(panel_w: f32) -> f32 {
    let dp = dp_scale_for(panel_w);
    (REFERENCE_BODY_SP * dp).clamp(EM_BASE_PX, MAX_BODY_EM_PX)
}

/// Whether the active face for `family` can actually draw `cp`.
///
/// True for printable ASCII (the hand-authored vector tables), and otherwise
/// only when the loaded TrueType face parsed a glyph for it. This is the
/// predicate that decides between "draw the glyph" and "draw a placeholder",
/// so it must agree with what `draw_glyph` would otherwise reach for --
/// otherwise a codepoint would be announced as missing while a glyph was
/// silently drawn, or the reverse.
#[inline]
pub fn family_has_glyph(family: FontFamily, cp: char) -> bool {
    if (' '..='\u{7e}').contains(&cp) {
        return true;
    }
    if family == FontFamily::NotoSans {
        if let Some(ttf) = get_noto_ttf() {
            // In range, present, *and* its outline assembles. An in-range
            // entry whose outline we could not build is a glyph the font claims
            // but cannot draw, which is a missing glyph as far as the user is
            // concerned. A space passes: drawable, and correctly blank.
            return ttf.can_draw(cp);
        }
    }
    false
}

/// The visible placeholder for a codepoint the active face cannot draw.
///
/// A hollow box, the universal "tofu". Drawn rather than skipped because the
/// failure mode it replaces was silent: a CJK app name rendered as blank space
/// of roughly the right width, which reads as "this launcher is broken" and
/// gives the user nothing to act on. A box says "this character is missing"
/// at the exact place it is missing.
///
/// Geometry is derived from the advance rather than hard-coded, so a
/// placeholder for a CJK-width codepoint looks square and one for a narrow
/// codepoint looks narrow -- the same relationship the real metrics would have.
/// Returned advance matches [`char_advance`] so `measure()` and `draw_run()`
/// still agree, which is what keeps centred and clipped text aligned.
#[allow(clippy::too_many_arguments)]
fn draw_missing_glyph(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: f32,
    y: f32,
    size_px: f32,
    weight: FontWeight,
    color: u32,
) -> f32 {
    let adv_units = FALLBACK_ADVANCE;
    let adv = adv_units * size_px / UNITS_PER_EM;
    let s = size_px / UNITS_PER_EM;
    // Inset by the stroke so the box sits inside its own advance, and keep a
    // hairline of daylight at small sizes so a 12 px label does not fill solid.
    let stroke = (weight.pen() * s).max(1.0);
    let inset = stroke * 0.5;
    let box_w = (FALLBACK_ADVANCE * s) - inset * 2.0;
    let box_h = (ASCENDER * 0.66 * s) - inset * 2.0;
    if box_w < 2.0 * stroke || box_h < 2.0 * stroke {
        // Too small for a legible outline: the advance alone is the honest
        // answer, and a filled smear would read as ink where there is none.
        return adv;
    }
    let baseline = y + ASCENDER * s;
    let top = baseline - ASCENDER * 0.66 * s + inset;
    let left = x + inset;
    // Four edges as a stroke, not a fill: a filled box at 0.6 em would be a
    // solid block, which is exactly the "ink where there is none" this is
    // avoiding.
    for (ex, ey, ew, eh) in [
        (left, top, box_w, stroke),
        (left, top + box_h - stroke, box_w, stroke),
        (left, top, stroke, box_h),
        (left + box_w - stroke, top, stroke, box_h),
    ] {
        let ex = ex.floor();
        let ey = ey.floor();
        let ew = ew.ceil().max(1.0) as usize;
        let eh = eh.ceil().max(1.0) as usize;
        if ex < 0.0 || ey < 0.0 {
            continue;
        }
        let (ex, ey) = (ex as usize, ey as usize);
        if ex >= w || ey >= h {
            continue;
        }
        for row in ey..(ey + eh).min(h) {
            let base = row * stride;
            for col in ex..(ex + ew).min(w) {
                buf[base + col] = blend_over(buf[base + col], color, 255);
            }
        }
    }
    adv
}

/// Advance width of one character, in pixels.
///
/// Takes a `char`, not a `u8`. The byte form made this meaningless for
/// anything but ASCII: a 3-byte CJK ideograph was charged the fallback advance
/// three times, so the string was laid out at roughly 3x its true width while
/// painting nothing. A `char` is charged once.
#[inline]
pub fn char_advance(cp: char, size_px: f32) -> f32 {
    char_advance_for(cp, size_px, active_family())
}

/// [`char_advance`] against an explicit family, for [`measure_with_family`].
#[inline]
pub fn char_advance_for(cp: char, size_px: f32, family: FontFamily) -> f32 {
    let a = if (' '..='\u{7e}').contains(&cp) {
        let b = cp as u8;
        if family == FontFamily::NotoSans {
            if let Some(ttf) = get_noto_ttf() {
                ttf.advance(cp)
            } else {
                glyph_for_family(b, FontFamily::NotoSans).a
            }
        } else {
            glyph_for_family(b, family).a
        }
    } else if family == FontFamily::NotoSans {
        if let Some(ttf) = get_noto_ttf() {
            ttf.advance(cp)
        } else {
            FALLBACK_ADVANCE
        }
    } else {
        FALLBACK_ADVANCE
    };
    a * size_px / UNITS_PER_EM
}

/// Total advance width of a string, in pixels.
pub fn measure(text: &str, size_px: f32) -> f32 {
    let mut total = 0.0;
    for cp in text.chars() {
        total += char_advance(cp, size_px);
    }
    total
}

/// Distance from `(px, py)` to the segment `(x0,y0)-(x1,y1)`, squared.
#[inline]
fn dist_seg2(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 1e-9 {
        (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let ex = x0 + t * dx - px;
    let ey = y0 + t * dy - py;
    ex * ex + ey * ey
}

/// Squared distance from `(px, py)` to the quadratic bezier
/// `(x0,y0) (cx,cy) (x1,y1)`, flattened on the fly.
#[allow(clippy::too_many_arguments)]
#[inline]
fn dist_quad2(px: f32, py: f32, x0: f32, y0: f32, cx: f32, cy: f32, x1: f32, y1: f32) -> f32 {
    let mut best = f32::MAX;
    let mut ax = x0;
    let mut ay = y0;
    let inv = 1.0 / FLAT as f32;
    let mut t = inv;
    while t <= 1.0 {
        let mt = 1.0 - t;
        let (a, b, c) = (mt * mt, 2.0 * mt * t, t * t);
        let bx = a * x0 + b * cx + c * x1;
        let by = a * y0 + b * cy + c * y1;
        let d = dist_seg2(px, py, ax, ay, bx, by);
        if d < best {
            best = d;
        }
        ax = bx;
        ay = by;
        t += inv;
    }
    best
}

/// Alpha blend `fg` over `bg`; the single source of pixel compositing truth for
/// the shell UI.
#[inline]
pub fn blend_over(bg: u32, fg: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    if a == 0 {
        return bg;
    }
    if a == 255 {
        return (0xFF << 24) | (fg & 0x00FF_FFFF);
    }
    let inv = 255 - a;
    let r = (((fg >> 16) & 0xFF) * a + ((bg >> 16) & 0xFF) * inv + 127) / 255;
    let g = (((fg >> 8) & 0xFF) * a + ((bg >> 8) & 0xFF) * inv + 127) / 255;
    let b = ((fg & 0xFF) * a + (bg & 0xFF) * inv + 127) / 255;
    (0xFF << 24) | (r << 16) | (g << 8) | b
}

/// Edge length of the per-glyph coverage mask served from the raster cache.
/// An 80px box covers every UI size (labels at em <= 60 need ~68px including
/// the pen); larger glyphs rasterise directly without being cached.
const GLYPH_MASK_MAX: i64 = 80;
const GLYPH_MASK_LEN: usize = GLYPH_MASK_MAX as usize * GLYPH_MASK_MAX as usize;
/// 2-way associative glyph coverage cache: 48 sets x 2 ways (96 entries).
/// The set index hashes the full key, so distinct pen positions spread across
/// sets instead of evicting each other; one victim bit per set absorbs a
/// colliding pair without either entry re-rasterising every frame.
const GLYPH_CACHE_SETS: usize = 48;
const GLYPH_CACHE_WAYS: usize = 2;
/// Rasteriser bail-out edge: a glyph box larger than this returns its advance
/// without ink. Unreachable through the UI type scale (`em_px_at` caps the
/// panel factor at 2x, so real boxes stay well under 256px); it only bounds
/// the transient mask buffer against absurd direct `size_px` arguments.
const GLYPH_HUGE_EDGE: i64 = 1024;

/// Cache key: every input bit that changes the raster. The exact `f32` size
/// bits matter because `em_px_at` yields fractional sizes on panels other
/// than 1080 wide; the exact origin bits matter because subpixel phase
/// changes coverage. A hit therefore means bit-identical raster inputs.
#[derive(Clone, Copy, PartialEq, Eq)]
struct GlyphKey {
    /// The **codepoint**, not a byte.
    ///
    /// It was a `u8`, which was sufficient while the pipeline was ASCII-only.
    /// The moment the glyph argument became a `char`, a `u8` key had to be
    /// synthesised for every non-ASCII codepoint, and the obvious choice -- 0
    /// -- made every one of them share a cache entry. `\u{416}` then returned
    /// whatever advance some earlier non-ASCII glyph had cached, and drew that
    /// glyph's mask: a real cross-contamination bug, observable as Cyrillic
    /// words rendering as each other.
    ///
    /// `u32` costs 3 bytes per key across 96 entries (288 bytes of `.bss`) and
    /// removes the entire class of bug.
    cp: u32,
    family: u8,
    weight: u8,
    size_bits: u32,
    x_bits: u32,
    y_bits: u32,
}

impl GlyphKey {
    const EMPTY: Self = Self {
        cp: 0,
        family: 0,
        weight: 0,
        size_bits: 0,
        x_bits: 0,
        y_bits: 0,
    };
}

/// One cached glyph: advance, integer mask origin and tight `bw` x `bh`
/// alpha mask (only `..bw*bh` of `mask` is valid).
#[derive(Clone, Copy)]
struct GlyphSlot {
    occupied: bool,
    key: GlyphKey,
    adv: f32,
    ox: i32,
    oy: i32,
    bw: u8,
    bh: u8,
    mask: [u8; GLYPH_MASK_LEN],
}

impl GlyphSlot {
    const EMPTY: Self = Self {
        occupied: false,
        key: GlyphKey::EMPTY,
        adv: 0.0,
        ox: 0,
        oy: 0,
        bw: 0,
        bh: 0,
        mask: [0; GLYPH_MASK_LEN],
    };
}

#[derive(Clone, Copy)]
struct GlyphSet {
    ways: [GlyphSlot; GLYPH_CACHE_WAYS],
}

impl GlyphSet {
    const EMPTY: Self = Self {
        ways: [GlyphSlot::EMPTY; GLYPH_CACHE_WAYS],
    };
}

struct GlyphCache {
    sets: [GlyphSet; GLYPH_CACHE_SETS],
    victim: [u8; GLYPH_CACHE_SETS],
}

impl GlyphCache {
    const EMPTY: Self = Self {
        sets: [GlyphSet::EMPTY; GLYPH_CACHE_SETS],
        victim: [0; GLYPH_CACHE_SETS],
    };

    #[inline]
    fn set_index(key: &GlyphKey) -> usize {
        let mut h = key.size_bits
            ^ key.x_bits
            ^ key.y_bits
            ^ key.cp.wrapping_mul(0x9E37_79B1)
            ^ ((key.weight as u32) << 24)
            ^ ((key.family as u32) << 28);
        h ^= h >> 16;
        h = h.wrapping_mul(0x7FEB_352D);
        h ^= h >> 15;
        (h as usize) % GLYPH_CACHE_SETS
    }

    #[inline]
    fn lookup(&self, key: &GlyphKey) -> Option<&GlyphSlot> {
        let set = &self.sets[Self::set_index(key)];
        set.ways.iter().find(|s| s.occupied && s.key == *key)
    }

    #[allow(clippy::too_many_arguments)]
    fn store(&mut self, key: GlyphKey, adv: f32, ox: i32, oy: i32, bw: u8, bh: u8, mask: &[u8]) {
        let si = Self::set_index(&key);
        let wi = if !self.sets[si].ways[0].occupied {
            0
        } else if !self.sets[si].ways[1].occupied {
            1
        } else {
            let v = (self.victim[si] & 1) as usize;
            self.victim[si] ^= 1;
            v
        };
        let slot = &mut self.sets[si].ways[wi];
        slot.occupied = true;
        slot.key = key;
        slot.adv = adv;
        slot.ox = ox;
        slot.oy = oy;
        slot.bw = bw;
        slot.bh = bh;
        slot.mask[..mask.len()].copy_from_slice(mask);
    }
}

/// Fixed-size glyph coverage cache in `.bss` (~600 KiB, zero-initialised):
/// no heap on the hot path. A hit skips the skeleton walk, the flattening
/// and every sqrt (blit only); a miss rasterises once and stores a copy for
/// the next frame. A poisoned lock degrades to "cache disabled", never to a
/// panic on the render path.
static GLYPH_CACHE: Mutex<GlyphCache> = Mutex::new(GlyphCache::EMPTY);

#[inline]
fn weight_id(weight: FontWeight) -> u8 {
    match weight {
        FontWeight::Regular => 0,
        FontWeight::Medium => 1,
        FontWeight::Bold => 2,
    }
}

/// Composite a tight `bw` x `bh` alpha mask whose top-left sits at the integer
/// origin `(ox, oy)` over `buf`, clipping to the buffer. Not culled here: if
/// the window missed the buffer the caller already returned early, so the
/// loops below always make progress.
#[allow(clippy::too_many_arguments)]
fn blit_mask(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    mask: &[u8],
    bw: i32,
    bh: i32,
    ox: i32,
    oy: i32,
    color: u32,
) {
    let x_lo = ox.max(0);
    let y_lo = oy.max(0);
    let x_hi = (ox + bw).min(w as i32);
    let y_hi = (oy + bh).min(h as i32);
    if x_hi <= x_lo || y_hi <= y_lo {
        return;
    }
    let bw_u = bw as usize;
    for py in y_lo..y_hi {
        let row_base = py as usize * stride;
        let mrow = (py - oy) as usize * bw_u;
        for px in x_lo..x_hi {
            let a = mask[mrow + (px - ox) as usize];
            if a != 0 {
                let i = row_base + px as usize;
                buf[i] = blend_over(buf[i], color, a);
            }
        }
    }
}

/// Rasterise glyph `g` (screen-space scale `s`, pen radius `r`, baseline)
/// into the tight `bw`-strided mask at integer origin `(ox, oy)`. Returns
/// true if any mask byte became nonzero.
///
/// Coverage accumulates with max both within and across sub-paths and is
/// composited exactly once by the caller, so overlapping curves never darken
/// each other.
#[allow(clippy::too_many_arguments)]
fn raster_into_mask(
    g: &Glyph,
    x: f32,
    s: f32,
    r: f32,
    baseline: f32,
    w: usize,
    h: usize,
    mask: &mut [u8],
    bw: i32,
    ox: i32,
    oy: i32,
) -> bool {
    let mut touched = false;
    let bw_u = bw as usize;
    for sub in g.sub {
        if sub.p.is_empty() {
            continue;
        }
        // Map the skeleton into screen space (y flipped: design space is up).
        // The control hull is a conservative bound on the curve, so the row
        // and column windows below can never clip ink.
        let start = (x + sub.s.0 * s, baseline - sub.s.1 * s);
        let mut min_x = start.0;
        let mut max_x = start.0;
        let mut min_y = start.1;
        let mut max_y = start.1;
        for q in sub.p {
            let cx = x + q.0 * s;
            let cy = baseline - q.1 * s;
            let bx = x + q.2 * s;
            let by = baseline - q.3 * s;
            min_x = min_x.min(cx).min(bx);
            max_x = max_x.max(cx).max(bx);
            min_y = min_y.min(cy).min(by);
            max_y = max_y.max(cy).max(by);
        }
        let x0 = (min_x - r - 1.0).floor().max(0.0) as i32;
        let x1 = (max_x + r + 1.0).ceil().min(w as f32) as i32;
        let y0 = (min_y - r - 1.0).floor().max(0.0) as i32;
        let y1 = (max_y + r + 1.0).ceil().min(h as f32) as i32;
        if x1 <= x0 || y1 <= y0 {
            // Culled sub-path: no ink. The advance is applied exactly once
            // by the caller, so nothing is added here.
            continue;
        }

        let mut row = [0u8; ROW_TILE];
        for py in y0..y1 {
            let sy = py as f32 + 0.5;
            let mut chunk = x0;
            while chunk < x1 {
                let chunk_end = (chunk + ROW_TILE as i32).min(x1);
                for c in row.iter_mut() {
                    *c = 0;
                }
                // Walk the skeleton, accumulating the largest coverage any
                // curve can reach on this scanline into the tile.
                let mut ax = x + sub.s.0 * s;
                let mut ay = baseline - sub.s.1 * s;
                for q in sub.p {
                    let cx = x + q.0 * s;
                    let cy = baseline - q.1 * s;
                    let bx = x + q.2 * s;
                    let by = baseline - q.3 * s;
                    // Conservative per-curve window (control hull + pen). A
                    // quadratic lies inside {P0, P1, P2}, so the control
                    // point must join the min/max or curved ink is clipped.
                    let cmin_x = ax.min(cx).min(bx) - r - 1.0;
                    let cmax_x = ax.max(cx).max(bx) + r + 1.0;
                    let cmin_y = ay.min(cy).min(by) - r - 1.0;
                    let cmax_y = ay.max(cy).max(by) + r + 1.0;
                    let (px0, py0) = (ax, ay);
                    ax = bx;
                    ay = by;
                    if sy < cmin_y || sy > cmax_y {
                        continue;
                    }
                    let lo = cmin_x.max(chunk as f32).ceil() as i32;
                    let hi = cmax_x.min(chunk_end as f32 - 0.5).floor() as i32;
                    if hi < lo {
                        continue;
                    }
                    // Reject outside the pen before the sqrt: the coverage
                    // would clamp to 0, so this is pixel-identical and saves
                    // a sqrt per rejected pixel.
                    let rr = r + 0.5;
                    let rr2 = rr * rr;
                    for px in lo..=hi {
                        let fxp = px as f32 + 0.5;
                        let d2 = dist_quad2(fxp, sy, px0, py0, cx, cy, bx, by);
                        if d2 >= rr2 {
                            continue;
                        }
                        let cov = (rr - d2.sqrt()).clamp(0.0, 1.0);
                        if cov <= 0.0 {
                            continue;
                        }
                        let idx = (px - chunk) as usize;
                        let a = (cov * 255.0 + 0.5) as u8;
                        if a > row[idx] {
                            row[idx] = a;
                        }
                    }
                }
                // Accumulate the tile into the glyph mask with max, so the
                // caller composites every pixel exactly once.
                let mrow = (py - oy) as usize * bw_u;
                for px in chunk..chunk_end {
                    let a = row[(px - chunk) as usize];
                    if a != 0 {
                        let mi = mrow + (px - ox) as usize;
                        if a > mask[mi] {
                            mask[mi] = a;
                            touched = true;
                        }
                    }
                }
                chunk = chunk_end;
            }
        }
    }
    touched
}

/// Rasterise one glyph whose origin sits at `(x, y)` with the baseline
/// `ASCENDER` design units below `y`. Returns the advance in pixels.
#[allow(clippy::too_many_arguments)]
pub fn draw_glyph(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: f32,
    y: f32,
    cp: char,
    color: u32,
    size_px: f32,
    weight: FontWeight,
) -> f32 {
    let family = active_family();
    // Control characters are refused before anything else, *including* the
    // font's opinion. `U+007F` (DEL) and `U+0085` (NEL) are inside the
    // pre-parsed range and Noto really does carry glyphs for them, so a
    // "does the font have it" check first would print a box for DEL. Whether a
    // control has ink is not a question for the font.
    if cp.is_control() {
        return FALLBACK_ADVANCE * size_px / UNITS_PER_EM;
    }
    let b = if (' '..='\u{7e}').contains(&cp) {
        cp as u8
    } else {
        0
    };
    // Outside ASCII there is no hand-authored vector glyph, so the only two
    // honest outcomes are "the loaded TrueType face has it" and "say so". The
    // old code took a third path -- return the advance and paint nothing --
    // which is the bug this replaces: a Greek, Cyrillic or CJK string occupied
    // correct-looking space and was completely invisible. Control characters
    // keep the old silent behaviour, because a tofu box for `\n` is noise.
    if !(' '..='\u{7e}').contains(&cp) && !family_has_glyph(family, cp) {
        return draw_missing_glyph(buf, stride, w, h, x, y, size_px, weight, color);
    }
    let s = size_px / UNITS_PER_EM;
    let r = weight.pen() * s;
    let baseline = y + ASCENDER * s;

    let (adv, ox64, oy64, ex64, ey64, use_ttf, g) = if family == FontFamily::NotoSans {
        if let Some(ttf) = get_noto_ttf() {
            let adv = ttf.advance(cp) * s;
            let bbox = ttf.bbox(cp);
            if bbox.0 == 0.0 && bbox.2 == 0.0 {
                // No ink at all (e.g. space): advance once.
                return adv;
            }
            let min_x = x + bbox.0 * s;
            let max_x = x + bbox.2 * s;
            let min_y = baseline - bbox.3 * s;
            let max_y = baseline - bbox.1 * s;
            // Reserve the same room the weight delta can reach, plus the AA
            // pixel. Only the *delta* is added, for the reason given at the
            // rasterise call; padding by the full radius would make the mask
            // two to three times larger for no visual gain.
            let pad = (r - FontWeight::Regular.pen() * s).max(0.0) + 1.0;
            let ox64 = (min_x - pad).floor() as i64;
            let oy64 = (min_y - pad).floor() as i64;
            let ex64 = (max_x + pad).ceil() as i64;
            let ey64 = (max_y + pad).ceil() as i64;
            (adv, ox64, oy64, ex64, ey64, true, None)
        } else {
            let g = glyph_for_family(b, family);
            let adv = g.a * s;
            let mut gmin_x = f32::INFINITY;
            let mut gmax_x = f32::NEG_INFINITY;
            let mut gmin_y = f32::INFINITY;
            let mut gmax_y = f32::NEG_INFINITY;
            for sub in g.sub {
                if sub.p.is_empty() {
                    continue;
                }
                gmin_x = gmin_x.min(x + sub.s.0 * s);
                gmax_x = gmax_x.max(x + sub.s.0 * s);
                gmin_y = gmin_y.min(baseline - sub.s.1 * s);
                gmax_y = gmax_y.max(baseline - sub.s.1 * s);
                for q in sub.p {
                    let cx = x + q.0 * s;
                    let cy = baseline - q.1 * s;
                    let bx = x + q.2 * s;
                    let by = baseline - q.3 * s;
                    gmin_x = gmin_x.min(cx).min(bx);
                    gmax_x = gmax_x.max(cx).max(bx);
                    gmin_y = gmin_y.min(cy).min(by);
                    gmax_y = gmax_y.max(cy).max(by);
                }
            }
            if gmin_x == f32::INFINITY {
                return adv;
            }
            let ox64 = (gmin_x - r - 1.0).floor() as i64;
            let oy64 = (gmin_y - r - 1.0).floor() as i64;
            let ex64 = (gmax_x + r + 1.0).ceil() as i64;
            let ey64 = (gmax_y + r + 1.0).ceil() as i64;
            (adv, ox64, oy64, ex64, ey64, false, Some(g))
        }
    } else {
        let g = glyph_for_family(b, family);
        let adv = g.a * s;
        let mut gmin_x = f32::INFINITY;
        let mut gmax_x = f32::NEG_INFINITY;
        let mut gmin_y = f32::INFINITY;
        let mut gmax_y = f32::NEG_INFINITY;
        for sub in g.sub {
            if sub.p.is_empty() {
                continue;
            }
            gmin_x = gmin_x.min(x + sub.s.0 * s);
            gmax_x = gmax_x.max(x + sub.s.0 * s);
            gmin_y = gmin_y.min(baseline - sub.s.1 * s);
            gmax_y = gmax_y.max(baseline - sub.s.1 * s);
            for q in sub.p {
                let cx = x + q.0 * s;
                let cy = baseline - q.1 * s;
                let bx = x + q.2 * s;
                let by = baseline - q.3 * s;
                gmin_x = gmin_x.min(cx).min(bx);
                gmax_x = gmax_x.max(cx).max(bx);
                gmin_y = gmin_y.min(cy).min(by);
                gmax_y = gmax_y.max(cy).max(by);
            }
        }
        if gmin_x == f32::INFINITY {
            return adv;
        }
        let ox64 = (gmin_x - r - 1.0).floor() as i64;
        let oy64 = (gmin_y - r - 1.0).floor() as i64;
        let ex64 = (gmax_x + r + 1.0).ceil() as i64;
        let ey64 = (gmax_y + r + 1.0).ceil() as i64;
        (adv, ox64, oy64, ex64, ey64, false, Some(g))
    };
    // Fully culled: nothing is visible, so the advance is applied exactly
    // once, here.
    if ex64.min(w as i64) <= ox64.max(0) || ey64.min(h as i64) <= oy64.max(0) {
        return adv;
    }
    let bw64 = ex64 - ox64;
    let bh64 = ey64 - oy64;
    if bw64 > GLYPH_HUGE_EDGE || bh64 > GLYPH_HUGE_EDGE {
        // Unreachable through the UI type scale; bounds the transient mask.
        return adv;
    }
    // Not culled, so bw/bh are positive and small: i32/usize math is safe,
    // and a non-culled window always overlaps the buffer, so ox+bw style
    // sums below cannot overflow either.
    let (bw, bh, ox, oy) = (bw64 as i32, bh64 as i32, ox64 as i32, oy64 as i32);

    // Cacheable only when the mask is position-independent: fully inside the
    // buffer (no clipping, so dimensions never affect content) and small
    // enough for a fixed slot.
    let cacheable = bw <= GLYPH_MASK_MAX as i32
        && bh <= GLYPH_MASK_MAX as i32
        && ox64 >= 0
        && oy64 >= 0
        && ex64 <= w as i64
        && ey64 <= h as i64;
    let key = GlyphKey {
        cp: cp as u32,
        family: family as u8,
        weight: weight_id(weight),
        size_bits: size_px.to_bits(),
        x_bits: x.to_bits(),
        y_bits: y.to_bits(),
    };
    if cacheable {
        // Hot path: blit only. No skeleton walk, no flattening, no sqrt,
        // no heap. The lock is held only for a fixed-size copy.
        let hit = GLYPH_CACHE.lock().ok().and_then(|cache| {
            cache.lookup(&key).map(|slot| {
                let len = slot.bw as usize * slot.bh as usize;
                let mut mask = [0u8; GLYPH_MASK_LEN];
                mask[..len].copy_from_slice(&slot.mask[..len]);
                (
                    slot.adv,
                    slot.ox,
                    slot.oy,
                    slot.bw as i32,
                    slot.bh as i32,
                    mask,
                )
            })
        });
        if let Some((cadv, cox, coy, cbw, cbh, cmask)) = hit {
            blit_mask(buf, stride, w, h, &cmask, cbw, cbh, cox, coy, color);
            return cadv;
        }
    }

    // Miss (or uncacheable): rasterise into a tight mask, then composite
    // once. Stack for cache-sized glyphs; a transient Vec only for oversize
    // ones, so the hot path above never allocates.
    let len = bw as usize * bh as usize;
    let mut stack_mask = [0u8; GLYPH_MASK_LEN];
    let mut heap_mask: Vec<u8>;
    let mask: &mut [u8] = if len <= GLYPH_MASK_LEN {
        &mut stack_mask[..len]
    } else {
        heap_mask = vec![0u8; len];
        &mut heap_mask[..]
    };
    let touched = if use_ttf {
        if let Some(ttf) = get_noto_ttf() {
            // The TrueType outline already encodes a stem width, so the pen
            // radius must NOT be added on top of it: at 96 px, Bold's radius
            // is 7.5 px, and adding that to an outline whose stems are already
            // ~8% of the em would give ~24%-em stems -- a black blob.
            //
            // What a heavier weight actually is, on an already-filled
            // outline, is the *delta* from the design's nominal stem. So
            // Regular renders the raw outline untouched and each step up adds
            // only the difference. (The bitmap path below is different: a
            // skeleton is a centreline, so it needs the absolute radius.)
            //
            // The delta is then scaled by `OUTLINE_WEIGHT_SCALE` so that a
            // dilated outline and a stroked skeleton agree on how much
            // heavier each step looks. They do not agree naturally: a
            // dilation thickens stems *and* closes counters *and* grows the
            // glyph's outer contour, so applying the raw pen delta measured
            // Bold at 1.64x Regular's coverage where the shipped bitmap path
            // measures 1.40x. The shell switches font family at runtime, so
            // the two have to look like the same type ramp; 0.6 brings them
            // together and `outline_and_skeleton_weights_agree` holds the
            // line.
            //
            // This is also why `r` was never passed before: with no weight
            // parameter the path rendered the raw outline for every weight,
            // which is what made Regular, Medium and Bold byte-identical.
            let nominal = FontWeight::Regular.pen() * s;
            ttf.rasterize_glyph(
                cp,
                x,
                s,
                baseline,
                mask,
                bw,
                bh,
                ox,
                oy,
                (r - nominal).max(0.0) * super::ttf::OUTLINE_WEIGHT_SCALE,
            )
        } else {
            false
        }
    } else {
        raster_into_mask(g.unwrap(), x, s, r, baseline, w, h, mask, bw, ox, oy)
    };
    blit_mask(buf, stride, w, h, mask, bw, bh, ox, oy, color);
    if cacheable && touched {
        if let Ok(mut cache) = GLYPH_CACHE.lock() {
            cache.store(key, adv, ox, oy, bw as u8, bh as u8, mask);
        }
    }
    adv
}

/// Draw a run of text whose ascender line is at `y` and left edge at `x`.
/// Returns the advance width of the run in pixels.
#[allow(clippy::too_many_arguments)]
pub fn draw_run(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: f32,
    y: f32,
    text: &str,
    color: u32,
    size_px: f32,
    weight: FontWeight,
) -> f32 {
    let mut pen = x;
    for cp in text.chars() {
        if cp == '\n' {
            continue;
        }
        pen += draw_glyph(buf, stride, w, h, pen, y, cp, color, size_px, weight);
    }
    pen - x
}

/// Draw a run of text with an explicitly specified font family.
#[allow(clippy::too_many_arguments)]
pub fn draw_run_with_family(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: f32,
    y: f32,
    text: &str,
    color: u32,
    size_px: f32,
    weight: FontWeight,
    family: FontFamily,
) -> f32 {
    let prev = active_family();
    set_active_family(family);
    let adv = draw_run(buf, stride, w, h, x, y, text, color, size_px, weight);
    set_active_family(prev);
    adv
}

/// Measure string advance with an explicitly specified font family.
pub fn measure_with_family(text: &str, size_px: f32, family: FontFamily) -> f32 {
    let mut total = 0.0;
    for cp in text.chars() {
        total += char_advance_for(cp, size_px, family);
    }
    total
}

/// Baseline offset in pixels below the ascender line, for call sites that want
/// to align text optically (centring on a cap, a row, an icon).
#[inline]
pub fn baseline_drop(size_px: f32) -> f32 {
    ASCENDER * size_px / UNITS_PER_EM
}

#[cfg(test)]
/// Serialises the tests that mutate the process-global font family.
///
/// Poison-tolerant on purpose. The lock exists so those tests cannot
/// interleave, not so a failure in one of them can veto the rest: a plain
/// `.lock().unwrap()` means any panic while the lock is held poisons it, and
/// every later test then fails with `PoisonError` instead of its own
/// assertion. That turns one real failure into a cascade that hides it. The
/// guard the caller gets is identical either way, because these tests only
/// serialise.
pub(crate) static TEST_FONT_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire [`TEST_FONT_MUTEX`], recovering from poisoning.
#[cfg(test)]
#[inline]
pub(crate) fn font_test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_FONT_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_are_proportional_and_tabular() {
        let _guard = font_test_lock();
        // Every printable ASCII glyph must have a positive, sane advance.
        for c in 0x20u8..0x80 {
            let a = glyph_of(c).a;
            assert!((200.0..=900.0).contains(&a), "glyph {} advance {}", c, a);
        }
        // Digits are tabular: the clock never jitters.
        let d0 = char_advance('0', 100.0);
        for c in '1'..='9' {
            assert_eq!(char_advance(c, 100.0), d0);
        }
        // Proportional: an "i" is much narrower than an "m".
        assert!(char_advance('i', 100.0) < char_advance('m', 100.0) * 0.5);
        // And the table sums to the measured string.
        assert!((measure("ill", 100.0) - 3.0 * char_advance('i', 100.0)).abs() < 0.001);
    }

    #[test]
    fn descenders_and_ascenders_exist_in_the_outlines() {
        // g, j, p, q, y must reach below the baseline; b, d, h, k, l above
        // the x-height.
        for c in *b"gjpqy,;" {
            let g = glyph_of(c);
            let mut min_y = f32::MAX;
            for sub in g.sub {
                for q in sub.p {
                    min_y = min_y.min(sub.s.1).min(q.1).min(q.3);
                }
            }
            assert!(
                min_y < -100.0,
                "{} must descend (min y {})",
                c as char,
                min_y
            );
        }
        for c in *b"bdhkltf" {
            let g = glyph_of(c);
            let mut max_y = f32::MIN;
            for sub in g.sub {
                for q in sub.p {
                    max_y = max_y.max(sub.s.1).max(q.1).max(q.3);
                }
            }
            assert!(
                max_y > X_HEIGHT + 100.0,
                "{} must ascend (max y {})",
                c as char,
                max_y
            );
        }
    }

    #[test]
    fn weight_axis_thickens_stems() {
        assert!(FontWeight::Bold.pen() > FontWeight::Medium.pen());
        assert!(FontWeight::Medium.pen() > FontWeight::Regular.pen());
    }

    #[test]
    fn coverage_is_analytic_and_never_overshoots_the_pen() {
        let _guard = font_test_lock();
        let prev = active_family();
        set_active_family(FontFamily::Homemade);
        // A vertical stem of the letter "l" must be exactly two pen radii
        // wide, and the ramp at its edge must span one pixel.
        let size = 60.0;
        let (w, h) = (80usize, 120usize);
        let mut buf = vec![0xFF000000u32; w * h];
        let adv = draw_run(
            &mut buf,
            w,
            w,
            h,
            20.0,
            10.0,
            "l",
            0xFFFFFFFF,
            size,
            FontWeight::Regular,
        );
        let y = 10 + (baseline_drop(size) - 300.0 * size / UNITS_PER_EM) as usize;
        let row: Vec<u8> = (0..w)
            .map(|x| ((buf[y * w + x] >> 16) & 0xFF) as u8)
            .collect();
        let solid = row.iter().filter(|&&v| v > 200).count();
        let stem = 2.0 * FontWeight::Regular.pen() * size / UNITS_PER_EM;
        assert!(
            (solid as f32 - stem).abs() <= 1.0,
            "stem width {} should match the pen {}",
            solid,
            stem
        );
        // Anti-aliasing must produce at least one partially covered pixel.
        let partial = row.iter().filter(|&&v| v > 20 && v < 235).count();
        assert!(partial >= 2, "expected an analytic AA ramp, got {:?}", row);
        assert!(adv > 0.0);
        set_active_family(prev);
    }

    /// The text engine must stay inside the shell's frame budget. This
    /// measures a realistic screen of type (labels, a header, a body block)
    /// rather than one glyph, so a regression in the rasteriser shows up here
    /// instead of as dropped frames on the device.
    #[test]
    fn rasteriser_stays_inside_the_frame_budget() {
        let (w, h) = (1080usize, 600usize);
        let mut buf = vec![0xFF000000u32; w * h];
        let lines = [
            ("All Applications", 2usize, FontWeight::Bold),
            ("Settings", 1, FontWeight::Medium),
            (
                "Tap to search apps, web and settings",
                1,
                FontWeight::Regular,
            ),
            (
                "The quick brown fox jumps over the lazy dog",
                1,
                FontWeight::Regular,
            ),
            ("10:34", 3, FontWeight::Medium),
        ];
        // Warm up so the measurement is not dominated by first-touch faults.
        for (text, scale, weight) in lines {
            draw_run(
                &mut buf,
                w,
                w,
                h,
                10.0,
                10.0,
                text,
                0xFFFFFFFF,
                em_px(scale),
                weight,
            );
        }
        let start = std::time::Instant::now();
        const FRAMES: u32 = 20;
        for _ in 0..FRAMES {
            for (i, (text, scale, weight)) in lines.iter().enumerate() {
                draw_run(
                    &mut buf,
                    w,
                    w,
                    h,
                    10.0,
                    10.0 + i as f32 * 60.0,
                    text,
                    0xFFFFFFFF,
                    em_px(*scale),
                    *weight,
                );
            }
        }
        let per_frame = start.elapsed() / FRAMES;
        eprintln!(
            "type: {:?} per frame ({} lines, build {})",
            per_frame,
            lines.len(),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
        // Absolute timings only mean something for an optimised build; a debug
        // build of the same code is ~6x slower and is used for correctness.
        if cfg!(debug_assertions) {
            return;
        }
        // 60 fps leaves 16.6ms; a full launcher screen has well under 200
        // glyphs, so a quarter of the budget is a generous ceiling that still
        // catches an order-of-magnitude regression.
        assert!(
            per_frame.as_micros() < 4_000,
            "text rasteriser too slow: {:?} per frame",
            per_frame
        );
    }

    /// Cold-cache weight rasterisation stays bounded.
    ///
    /// `rasteriser_stays_inside_the_frame_budget` warms the glyph cache first,
    /// which is right for a frame budget -- the cache is warm from the second
    /// frame onwards. It is blind to the first frame, and the TrueType weight
    /// axis dilates the outline on every cache miss, so that first frame is
    /// where its cost lands. Measured on this host at 40 lines x 37 characters
    /// (1480 distinct glyph rasterisations), A/B'd by setting
    /// `OUTLINE_WEIGHT_SCALE` to 0.6 and to 0.0:
    ///
    /// ```text
    ///          off      on
    /// Regular  18.6 ms  18.4 ms
    /// Medium   18.3 ms  22.5 ms
    /// Bold     17.0 ms  23.7 ms
    /// ```
    ///
    /// So a weight step is about +27% of cold rasterisation, which is the
    /// honest price of 1-2 extra separable passes over the glyph mask. Regular
    /// pays nothing, because its delta from itself is zero and the dilation is
    /// skipped -- and icon labels, the bulk of the text on a launcher screen,
    /// are Regular.
    ///
    /// This asserts only that the cost stays the shape it is, at 2x the
    /// measured figure. An absolute bound would be flaky across hosts; what is
    /// worth catching is a dilation that suddenly does a full-mask distance
    /// transform, or one whose step count is driven by the pen radius instead
    /// of the weight delta, either of which is an order of magnitude rather
    /// than a constant factor.
    #[test]
    fn cold_weight_rasterisation_stays_bounded() {
        use std::time::Instant;

        let _guard = font_test_lock();
        let prev = active_family();
        if get_noto_ttf().is_none() {
            return;
        }
        set_active_family(FontFamily::NotoSans);

        let (w, h) = (1080usize, 600usize);
        // Each weight is a distinct glyph-cache key, so measuring them in one
        // process still measures a cold rasterisation for each.
        let mut worst = 0u128;
        for weight in [FontWeight::Medium, FontWeight::Bold] {
            let mut canvas = vec![0xFF000000u32; w * h];
            let start = Instant::now();
            for i in 0..40 {
                draw_run(
                    &mut canvas,
                    w,
                    w,
                    h,
                    10.0,
                    10.0 + i as f32 * 14.0,
                    "Settings Clock Calculator Files 10:34 PM",
                    0xFFFFFFFF,
                    40.0,
                    weight,
                );
            }
            let per = start.elapsed().as_micros();
            worst = worst.max(per);
        }
        set_active_family(prev);

        if cfg!(debug_assertions) {
            return;
        }
        // 45 ms is ~2x the measured 23.7 ms.
        assert!(
            worst < 45_000,
            "cold weight rasterisation is {worst}us, more than 2x the measured \
             23.7ms; the dilation is doing more work than a separable box should"
        );
    }

    /// A culled glyph must advance exactly like a visible one: draw_run on a
    /// clipped buffer has to agree with measure(), or every following glyph
    /// in the run is displaced (and centring/truncation computed from
    /// measure disagrees with what is drawn).
    #[test]
    fn culled_glyphs_advance_exactly_once() {
        let _guard = font_test_lock();
        let size = 45.0;
        // Fully above the buffer: every glyph is culled.
        let (w, h) = (200usize, 60usize);
        let mut buf = vec![0xFF000000u32; w * h];
        let drawn = draw_run(
            &mut buf,
            w,
            w,
            h,
            10.0,
            -200.0,
            "00000",
            0xFFFFFFFF,
            size,
            FontWeight::Regular,
        );
        assert!(
            (drawn - measure("00000", size)).abs() < 0.001,
            "culled run {drawn} vs measured {}",
            measure("00000", size)
        );
        // Culled glyphs ink nothing.
        assert!(buf.iter().all(|&p| p == 0xFF000000));
        // A single culled glyph matches char_advance, like a visible one.
        let mut one = vec![0xFF000000u32; w * h];
        let a = draw_glyph(
            &mut one,
            w,
            w,
            h,
            10.0,
            -200.0,
            '2',
            0xFFFFFFFF,
            size,
            FontWeight::Regular,
        );
        assert!((a - char_advance('2', size)).abs() < 0.0001);
        // Multi-subpath culled glyphs ('!' has several) advance once too.
        let mut bang = vec![0xFF000000u32; w * h];
        let ab = draw_glyph(
            &mut bang,
            w,
            w,
            h,
            -500.0,
            10.0,
            '!',
            0xFFFFFFFF,
            size,
            FontWeight::Regular,
        );
        assert!((ab - char_advance('!', size)).abs() < 0.0001);
    }

    /// Bytes outside printable ASCII carry no ink but keep the fallback
    /// A codepoint the face cannot draw is *announced*, not silently dropped.
    ///
    /// This test used to assert the opposite -- that `0x80`, `0xC3`, `0xFF` and
    /// friends have no ink -- and that assertion is what the defect looked like
    /// from inside the codebase: a Greek, Cyrillic or CJK string occupied
    /// plausible space and drew nothing at all, so a launcher in a non-Latin
    /// locale rendered app names and message senders as blank gaps.
    ///
    /// The contract is now split by *intent*:
    ///
    /// * a control character is skipped (a tofu box for `\n` would be noise);
    /// * anything else printable gets a visible placeholder, so the user can see
    ///   that a character is missing, and where.
    ///
    /// Control characters are fed as `char`, not as the raw byte: `0x0A` and
    /// `0x85` are different characters, and `0x80` is `U+0080` (a control),
    /// which is precisely the case that must stay silent. `U+00E9` (`é`),
    /// `U+0416` (Cyrillic) and `U+4E2D` (CJK) are the visible half.
    #[test]
    fn an_undrawable_codepoint_is_announced_but_a_control_is_not() {
        let _guard = font_test_lock();
        let size = 30.0;
        let (w, h) = (200usize, 120usize);

        for cp in ['\u{0}', '\u{9}', '\u{a}', '\u{1f}', '\u{7f}', '\u{85}'] {
            let mut buf = vec![0xFF000000u32; w * h];
            let a = draw_glyph(
                &mut buf,
                w,
                w,
                h,
                20.0,
                10.0,
                cp,
                0xFFFFFFFF,
                size,
                FontWeight::Regular,
            );
            assert!(
                (a - char_advance(cp, size)).abs() < 0.0001,
                "control {cp:?}: advance must still match char_advance"
            );
            assert!(
                buf.iter().all(|&p| p == 0xFF000000),
                "control {cp:?} must stay silent, but it inked {} px",
                buf.iter().filter(|&&p| p != 0xFF000000).count()
            );
        }

        for cp in ['\u{e9}', '\u{416}', '\u{4e2d}', '\u{1f600}'] {
            let mut buf = vec![0xFF000000u32; w * h];
            let a = draw_glyph(
                &mut buf,
                w,
                w,
                h,
                20.0,
                10.0,
                cp,
                0xFFFFFFFF,
                size,
                FontWeight::Regular,
            );
            assert!(
                (a - char_advance(cp, size)).abs() < 0.0001,
                "{cp:?}: the drawn advance must equal the measured one"
            );
            let inked = buf.iter().filter(|&&p| p != 0xFF000000).count();
            assert!(
                inked > 0,
                "{cp:?} must draw a visible placeholder; it drew nothing, which is \
                 the silent-blank-text defect"
            );
            // A placeholder is an outline, not a block: a filled box would be
            // "ink where there is none" at display scale.
            let filled = inked as f64 / (w * h) as f64;
            assert!(
                filled < 0.35,
                "{cp:?}: placeholder covers {pct:.1} of the frame, too solid to be an outline",
                pct = filled * 100.0
            );
        }
    }

    /// Two different non-ASCII characters must not share a cached raster.
    ///
    /// The glyph cache is keyed on the glyph argument, which was a `u8` when
    /// the pipeline was ASCII-only. Once it became a `char`, a `u8` key had to
    /// be synthesised for non-ASCII codepoints and every one of them hashed
    /// and compared equal -- so `\u{416}` could be served the mask and advance
    /// that some earlier non-ASCII glyph had cached, and Cyrillic words rendered
    /// as each other. The symptom is a *wrong advance*, not a wrong glyph index,
    /// so the assertion is on the advance: two characters with different metrics
    /// must not be interchangeable.
    #[test]
    fn distinct_non_ascii_characters_do_not_share_a_cached_raster() {
        let _guard = font_test_lock();
        set_active_family(FontFamily::NotoSans);
        let size = 40.0;
        // The collision only ever affected *non-ASCII* codepoints: ASCII has a
        // real byte to key on, so an ASCII pair cannot demonstrate it. These
        // two are both drawable from Noto and differ by more than 3x (Cyriс
        // Zhe 905 units against Ukrainian i 258).
        let wide = '\u{416}';
        let narrow = '\u{456}';
        let (aw, bw) = (char_advance(narrow, size), char_advance(wide, size));
        assert!(
            bw > aw * 2.0,
            "fixture unusable: {narrow:?}={aw}, {wide:?}={bw} -- needs a \
             drawable face with two clearly different non-ASCII advances"
        );

        let (w, h) = (200usize, 140usize);
        // Draw the wide one first, so a colliding narrow lookup inherits the
        // wide advance rather than the other way round.
        let mut buf = vec![0xFF000000u32; w * h];
        let a_wide = draw_glyph(
            &mut buf,
            w,
            w,
            h,
            10.0,
            10.0,
            wide,
            0xFFFFFFFF,
            size,
            FontWeight::Regular,
        );
        assert!(
            (a_wide - bw).abs() < 0.001,
            "first draw of {wide:?}: {a_wide} vs {bw}"
        );

        let a_narrow = draw_glyph(
            &mut buf,
            w,
            w,
            h,
            10.0,
            10.0,
            narrow,
            0xFFFFFFFF,
            size,
            FontWeight::Regular,
        );
        assert!(
            (a_narrow - aw).abs() < 0.001,
            "{narrow:?} was served a cached advance of {a_narrow}, expected {aw}"
        );

        // And the reverse order, for a 2-way cache: whichever slot is evicted,
        // neither advance may leak into the other.
        let mut buf2 = vec![0xFF000000u32; w * h];
        let a_n2 = draw_glyph(
            &mut buf2,
            w,
            w,
            h,
            10.0,
            10.0,
            narrow,
            0xFFFFFFFF,
            size,
            FontWeight::Regular,
        );
        assert!(
            (a_n2 - aw).abs() < 0.001,
            "first draw of {narrow:?}: {a_n2} vs {aw}"
        );
        let a_w2 = draw_glyph(
            &mut buf2,
            w,
            w,
            h,
            10.0,
            10.0,
            wide,
            0xFFFFFFFF,
            size,
            FontWeight::Regular,
        );
        assert!(
            (a_w2 - bw).abs() < 0.001,
            "{wide:?} was served a cached advance of {a_w2}, expected {bw}"
        );
    }

    /// A multi-byte character is charged and drawn once, not once per byte.
    ///
    /// The byte pipeline made this impossible to get right: a 3-byte CJK
    /// ideograph accumulated three fallback advances, so the string measured
    /// ~3x too wide *and* painted nothing.
    #[test]
    fn a_multi_byte_character_advances_once() {
        let _guard = font_test_lock();
        let size = 100.0;
        let one = char_advance('\u{4e2d}', size);
        // Three CJK characters must measure the same as three of anything else
        // that the font also cannot draw -- and, critically, three times one,
        // not nine times.
        let three = measure("\u{4e2d}\u{4e2d}\u{4e2d}", size);
        assert!(
            (three - 3.0 * one).abs() < 0.001,
            "three CJK characters measured {three}, expected {}",
            3.0 * one
        );
        // A 3-byte string is not 3 glyphs: the byte length must not leak into
        // the width.
        assert_eq!(
            "\u{4e2d}".len(),
            3,
            "the test premise: this character is 3 bytes"
        );
        assert!(
            (three - measure("\u{fffd}\u{fffd}\u{fffd}", size)).abs() < 0.001,
            "byte length must not affect the advance"
        );
    }

    /// Cache hits must reproduce misses exactly, on any background: draw once
    /// (miss), then twice more (hits) over different backgrounds, and require
    /// the same inked-pixel set every time.
    #[test]
    fn glyph_cache_hits_reproduce_misses() {
        let _guard = font_test_lock();
        set_active_family(FontFamily::NotoSans);
        fn inked(buf: &[u32], bg: u32) -> Vec<usize> {
            buf.iter()
                .enumerate()
                .filter(|(_, p)| **p != bg)
                .map(|(i, _)| i)
                .collect()
        }
        let size = 30.0;
        let (w, h) = (300usize, 120usize);
        let text = "Agc@e08";
        let mut buf1 = vec![0xFF000000u32; w * h];
        draw_run(
            &mut buf1,
            w,
            w,
            h,
            20.0,
            10.0,
            text,
            0xFFFFFFFF,
            size,
            FontWeight::Bold,
        );
        let set1 = inked(&buf1, 0xFF000000);
        assert!(!set1.is_empty(), "expected ink");
        // Second run over a different background: all hits, same ink shape.
        let mut buf2 = vec![0xFF101725u32; w * h];
        let adv = draw_run(
            &mut buf2,
            w,
            w,
            h,
            20.0,
            10.0,
            text,
            0xFFFFFFFF,
            size,
            FontWeight::Bold,
        );
        assert_eq!(inked(&buf2, 0xFF101725), set1);
        assert!((adv - measure(text, size)).abs() < 0.01);
    }

    #[test]
    fn test_font_family_switching_and_metrics() {
        let _guard = font_test_lock();
        // Noto Sans
        set_active_family(FontFamily::NotoSans);
        assert_eq!(active_family(), FontFamily::NotoSans);
        let d0_noto = char_advance('0', 100.0);
        if is_noto_ttf_loaded() {
            assert!(
                (d0_noto - 57.2).abs() < 0.1,
                "TTF Noto Sans advance expected 57.2, got {d0_noto}"
            );
        } else {
            assert!(
                (d0_noto - 56.0).abs() < 0.01,
                "Fallback Noto advance expected 56.0, got {d0_noto}"
            );
        }
        for c in '1'..='9' {
            assert_eq!(char_advance(c, 100.0), d0_noto);
        }

        // Test forced fallback to ensure fallback path works deterministically
        set_force_fallback(true);
        let d0_noto_fallback = char_advance('0', 100.0);
        assert!((d0_noto_fallback - 56.0).abs() < 0.01);
        set_force_fallback(false);

        // Homemade (used in Super Extreme power saver mode)
        set_active_family(FontFamily::Homemade);
        assert_eq!(active_family(), FontFamily::Homemade);
        let d0_home = char_advance('0', 100.0);
        assert!((d0_home - 62.0).abs() < 0.01);

        // AsciiMono: strictly fixed 60.0px advance for all ASCII chars
        set_active_family(FontFamily::AsciiMono);
        assert_eq!(active_family(), FontFamily::AsciiMono);
        for c in 0x20u8..0x7F {
            assert_eq!(char_advance(c as char, 100.0), 60.0);
        }

        // Revert to NotoSans
        set_active_family(FontFamily::NotoSans);
        assert_eq!(active_family(), FontFamily::NotoSans);
    }

    /// Dev aid: dump a type specimen so the outlines can be eyeballed.
    /// Enabled with UTLC_FONT_SPECIMEN=/path/to.ppm cargo test -p utim_core font
    #[test]
    fn dump_specimen() {
        let Ok(path) = std::env::var("UTLC_FONT_SPECIMEN") else {
            return;
        };
        let (w, h) = (1000usize, 760usize);
        let mut buf = vec![0xFF101725u32; w * h];
        let mut y = 12.0f32;
        for (text, size, weight) in [
            ("AaBbCcDdEeFfGgHhIiJjKk", 34.0, FontWeight::Regular),
            ("LlMmNnOoPpQqRrSsTtUuVv", 34.0, FontWeight::Regular),
            ("WwXxYyZz 0123456789", 34.0, FontWeight::Regular),
            ("!\"#$%&'()*+,-./:;<=>?@", 30.0, FontWeight::Regular),
            ("[\\]^_`{|}~ backslash", 30.0, FontWeight::Regular),
            ("The quick brown fox jumps", 30.0, FontWeight::Regular),
            ("over the lazy dog, 42 times.", 30.0, FontWeight::Regular),
            ("ALL APPLICATIONS", 26.0, FontWeight::Bold),
            ("Medium weight body copy", 26.0, FontWeight::Medium),
            ("Regular weight body copy", 26.0, FontWeight::Regular),
            ("1080x2400 @ 120Hz  5G 98%", 22.0, FontWeight::Medium),
            ("Search apps, web...", 22.0, FontWeight::Regular),
            ("10:34", 96.0, FontWeight::Medium),
            ("12:59", 64.0, FontWeight::Bold),
        ] {
            draw_run(&mut buf, w, w, h, 24.0, y, text, 0xFFEFF4FF, size, weight);
            y += size * 1.6;
        }
        let mut out: Vec<u8> = Vec::with_capacity(w * h * 3 + 32);
        out.extend_from_slice(format!("P6\n{} {}\n255\n", w, h).as_bytes());
        for px in &buf {
            out.push(((*px >> 16) & 0xFF) as u8);
            out.push(((*px >> 8) & 0xFF) as u8);
            out.push((*px & 0xFF) as u8);
        }
        std::fs::write(&path, out).unwrap();
        eprintln!("specimen written to {path}");
    }

    #[test]
    fn test_super_extreme_power_saver_font_strategy() {
        let _guard = font_test_lock();

        // 1. In Normal Mode: NotoSans is active.
        set_active_family(FontFamily::NotoSans);
        assert_eq!(active_family(), FontFamily::NotoSans);
        let normal_adv = measure("ANDROID RECOVERY", 24.0);
        assert!(normal_adv > 0.0);

        // 2. Transition into Super Extreme Power Saver Mode:
        // System explicitly switches to FontFamily::Homemade.
        set_active_family(FontFamily::Homemade);
        assert_eq!(active_family(), FontFamily::Homemade);
        let recovery_adv = measure("ANDROID RECOVERY", 24.0);
        assert!(recovery_adv > 0.0);

        // Advance widths between NotoSans and Homemade differ appropriately
        // due to different typeface design spaces:
        if is_noto_ttf_loaded() {
            assert_ne!(normal_adv, recovery_adv);
        }

        // 3. ASCII Monospace preview font for camera ASCII terminal viewfinder:
        set_active_family(FontFamily::AsciiMono);
        assert_eq!(active_family(), FontFamily::AsciiMono);
        let mono_adv = measure("######", 24.0);
        assert!((mono_adv - 6.0 * char_advance('#', 24.0)).abs() < 1e-4);

        // 4. Return to Super Extreme Mode:
        set_active_family(FontFamily::Homemade);
        assert_eq!(active_family(), FontFamily::Homemade);

        // 5. Exit back to Normal Mode:
        set_active_family(FontFamily::NotoSans);
        assert_eq!(active_family(), FontFamily::NotoSans);
    }
}
