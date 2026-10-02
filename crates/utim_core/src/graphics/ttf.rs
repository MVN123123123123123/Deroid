//! Minimal, zero-dependency TrueType (TTF) font parser and scanline rasterizer.
//!
//! Designed specifically for loading authentic packaged Google Noto Sans
//! (`NotoSans-Regular.ttf` or `NotoSans-Bold.ttf`) from system font paths,
//! providing production-quality anti-aliased glyph rendering with zero external
//! crates and zero dynamic heap allocation on the render hot path.
//!
//! Conforms to `AGENTS.md`:
//! - Pure stdlib implementation (no FreeType, HarfBuzz, fontdue, or rusttype).
//! - Sub-millisecond cold boot footprint.
//! - Graceful deterministic fallback to the home-made typography engine if the font
//!   file is missing, corrupt, or unreadable.

use std::fs;
use std::path::Path;

/// Standard system search paths for Google Noto Sans.
pub const NOTO_SANS_CANDIDATE_PATHS: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/opentype/noto/NotoSans-Regular.otf",
    "/system/fonts/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

/// One flattened outline segment, in **font design units**.
///
/// `i16`, not `f32`, and that is a deliberate memory decision. These lines are
/// parsed once for every covered codepoint and then live for the life of the
/// process: 2378 glyphs hold ~122k segments, so `f32` costs 16 bytes each and
/// the table came to 2.9 MB of RSS -- 39% of this launcher's 15 MiB budget, for
/// numbers that are small integers. `i16` halves it to ~1.45 MB with no
/// coverage change and no precision loss: design units are 0..~4096 for any
/// real face, and every consumer scales to pixels immediately.
///
/// Coordinates outside `i16` (a malformed font, or a composite transform that
/// runs away) **saturate** rather than wrap: a clamped line draws slightly
/// wrong, a wrapped one draws on the wrong side of the glyph.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TtfLine {
    pub x0: i16,
    pub y0: i16,
    pub x1: i16,
    pub y1: i16,
}

impl TtfLine {
    /// A segment from float design units, each coordinate saturated to `i16`.
    #[inline]
    fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self {
            x0: sat_i16(x0),
            y0: sat_i16(y0),
            x1: sat_i16(x1),
            y1: sat_i16(y1),
        }
    }
}

/// Saturating `f32` -> `i16`, mapping NaN to 0.
#[inline]
fn sat_i16(v: f32) -> i16 {
    if v.is_nan() {
        return 0;
    }
    v.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

/// Pre-parsed TrueType glyph for fast rasterization.
#[derive(Debug, Clone)]
pub struct TtfGlyph {
    pub advance: f32,
    pub lsb: f32,
    pub bbox: (f32, f32, f32, f32), // (min_x, min_y, max_x, max_y)
    pub lines: Vec<TtfLine>,
}

/// The codepoint ranges pre-parsed at load time, ascending and disjoint.
///
/// Not "all of Unicode": a `TtfGlyph` owns a `Vec<TtfLine>`, so pre-parsing
/// everything would cost megabytes and seconds of boot for glyphs no launcher
/// string will ever contain. What is here is what app names, contacts, message
/// senders and file paths are actually made of:
///
/// | range | block | why |
/// |---|---|---|
/// | 0x20..0x7F | ASCII | the base set, unchanged |
/// | 0xA0..0x100 | Latin-1 Supplement | `\u{e9}`-style accents, `\u{fc}`, `\u{f1}` |
/// | 0x100..0x180 | Latin Extended-A | Polish, Czech, Hungarian, Latvian |
/// | 0x180..0x250 | Latin Extended-B | the rest of the European letters |
/// | 0x250..0x2AF | IPA Extensions | language names |
/// | 0x370..0x400 | Greek | `\u{3b1}`.., monotonic |
/// | 0x400..0x530 | Cyrillic + Supplement | Russian, Ukrainian, Serbian |
/// | 0x530..0x590 | Armenian | |
/// | 0x5D0..0x5EB | Hebrew | right-to-left, still shaped LTR here |
/// | 0x5F0..0x650 | Hebrew presentation forms | |
/// | 0x600..0x6FF | Arabic | |
/// | 0x2000..0x2070 | General Punctuation | dashes, quotes, ellipsis |
/// | 0x20A0..0x20C0 | Currency | |
/// | 0x2100..0x2150 | Letterlike Symbols | |
/// | 0x2190..0x21C0 | Arrows | |
/// | 0x2200..0x22FF | Math Operators | |
/// | 0x2500..0x2580 | Box Drawing | |
/// | 0x25A0..0x2600 | Geometric Shapes | |
/// | 0x3000..0{3040} | CJK Symbols and Punctuation | `\u{3001}`, `\u{3002}` |
/// | 0xFF01..0xFF60 | Halfwidth and Fullwidth Forms | |
///
/// **CJK ideographs and Hangul are deliberately absent.** Noto Sans does not
/// cover them either, so a pre-parse would yield an empty table for the most
/// expensive ranges in Unicode. Those codepoints fall through to the visible
/// placeholder box in `font::draw_glyph`, which is a truthful answer: the
/// glyph is genuinely missing, and a box says so where blank space did not.
pub const COVERED_RANGES: &[(u32, u32)] = &[
    (0x0020, 0x0080),
    (0x00A0, 0x0100),
    (0x0100, 0x0250),
    (0x0250, 0x02B0),
    (0x0370, 0x0400),
    (0x0400, 0x0530),
    (0x0530, 0x0590),
    (0x05D0, 0x05EB),
    (0x05F0, 0x0650),
    (0x0600, 0x0700),
    (0x2000, 0x2070),
    (0x20A0, 0x20C0),
    (0x2100, 0x2150),
    (0x2190, 0x21C0),
    (0x2200, 0x2300),
    (0x2500, 0x2580),
    (0x25A0, 0x2600),
    (0x3000, 0x3040),
    (0xFF01, 0xFF60),
];

/// Total codepoints in [`COVERED_RANGES`], computed rather than written down.
pub const COVERED_COUNT: u32 = {
    let mut n = 0;
    let mut i = 0;
    while i < COVERED_RANGES.len() {
        n += COVERED_RANGES[i].1 - COVERED_RANGES[i].0;
        i += 1;
    }
    n
};

/// Loaded TrueType font holding every codepoint in [`COVERED_RANGES`].
///
/// `cps` is ascending and parallel to `glyphs`; a lookup is a binary search.
/// That costs ~9 comparisons on a ~1000-entry table, which is free next to the
/// rasterisation it precedes, and it avoids a 64 KiB direct-map index whose
/// memset would show up in the boot budget.
#[derive(Debug, Clone)]
pub struct TrueTypeFont {
    pub units_per_em: f32,
    pub ascender: f32,
    pub descender: f32,
    /// Pre-parsed codepoints, ascending. Parallel to `glyphs`.
    pub cps: Vec<u16>,
    /// `None` where the glyph was in range but its outline would not parse.
    pub glyphs: Vec<Option<TtfGlyph>>,
    /// `true` where the glyph is in range but its outline would not render.
    ///
    /// Parallel to `cps` and `glyphs`. This is what separates "a space, which
    /// is correctly blank" from "`\u{e9}`, whose composite outline we could
    /// not assemble" -- both are present, both carry no lines, and only one of
    /// them should produce a visible placeholder.
    pub unrenderable: Vec<bool>,
}

impl TrueTypeFont {
    /// Index into [`Self::glyphs`] for `cp`, or `None` if `cp` is out of range.
    ///
    /// `None` means "not pre-parsed", *not* "no glyph": the caller distinguishes
    /// the two by re-checking `glyphs[i]`.
    #[inline]
    pub fn glyph_index(&self, cp: char) -> Option<usize> {
        let c = cp as u32;
        if c > u16::MAX as u32 {
            return None;
        }
        self.cps.binary_search(&(c as u16)).ok()
    }

    /// The parsed glyph for `cp`, if this font has one.
    ///
    /// Returns the entry even when it is marked [`Self::unrenderable`]: the
    /// advance and side bearing are still correct, so metrics are safe to read.
    /// Use [`Self::can_draw`] to decide whether to draw.
    #[inline]
    pub fn glyph(&self, cp: char) -> Option<&TtfGlyph> {
        self.glyph_index(cp).and_then(|i| self.glyphs[i].as_ref())
    }

    /// Whether this font can actually put ink on the page for `cp`.
    ///
    /// False for a codepoint outside [`COVERED_RANGES`] and for one whose
    /// outline would not assemble. True for a space, which is drawable and
    /// simply has nothing to draw.
    #[inline]
    pub fn can_draw(&self, cp: char) -> bool {
        match self.glyph_index(cp) {
            Some(i) => !self.unrenderable[i],
            None => false,
        }
    }
}

impl TrueTypeFont {
    /// Attempt to load and parse Google Noto Sans from standard system paths.
    pub fn load_system_noto() -> Option<Self> {
        for path in NOTO_SANS_CANDIDATE_PATHS {
            if Path::new(path).exists() {
                if let Some(font) = Self::load_from_file(path) {
                    return Some(font);
                }
            }
        }
        None
    }

    /// Load and parse a TrueType font from a file path.
    pub fn load_from_file(path: &str) -> Option<Self> {
        let bytes = fs::read(path).ok()?;
        Self::parse(&bytes).ok()
    }

    /// Parse a TrueType font from raw binary data.
    pub fn parse(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() < 12 {
            return Err("TTF data too small for offset table");
        }

        let num_tables = read_u16(data, 4) as usize;
        if data.len() < 12 + num_tables * 16 {
            return Err("TTF table directory truncated");
        }

        // Locate required tables
        let mut head_range = None;
        let mut hhea_range = None;
        let mut hmtx_range = None;
        let mut cmap_range = None;
        let mut loca_range = None;
        let mut glyf_range = None;

        for i in 0..num_tables {
            let offset_pos = 12 + i * 16;
            let tag = &data[offset_pos..offset_pos + 4];
            let offset = read_u32(data, offset_pos + 8) as usize;
            let length = read_u32(data, offset_pos + 12) as usize;

            if offset.saturating_add(length) > data.len() {
                continue;
            }

            match tag {
                b"head" => head_range = Some((offset, length)),
                b"hhea" => hhea_range = Some((offset, length)),
                b"hmtx" => hmtx_range = Some((offset, length)),
                b"cmap" => cmap_range = Some((offset, length)),
                b"loca" => loca_range = Some((offset, length)),
                b"glyf" => glyf_range = Some((offset, length)),
                _ => {}
            }
        }

        let (head_off, _) = head_range.ok_or("Missing head table")?;
        let (hhea_off, _) = hhea_range.ok_or("Missing hhea table")?;
        let (hmtx_off, _) = hmtx_range.ok_or("Missing hmtx table")?;
        let (cmap_off, _) = cmap_range.ok_or("Missing cmap table")?;
        let (loca_off, _) = loca_range.ok_or("Missing loca table")?;
        let (glyf_off, _) = glyf_range.ok_or("Missing glyf table")?;

        let units_per_em = read_u16(data, head_off + 18) as f32;
        let index_to_loc_format = read_i16(data, head_off + 50);
        let num_hmetrics = read_u16(data, hhea_off + 34) as usize;
        let ascender = read_i16(data, hhea_off + 4) as f32;
        let descender = read_i16(data, hhea_off + 6) as f32;

        let glyf_data = &data[glyf_off..];
        let loca_data = &data[loca_off..];
        let hmtx_data = &data[hmtx_off..];

        // Parse cmap format 4 subtable
        let cmap_data = &data[cmap_off..];
        let num_cmap_subtables = read_u16(cmap_data, 2) as usize;
        let mut fmt4_offset = None;

        for i in 0..num_cmap_subtables {
            let sub_pos = 4 + i * 8;
            if sub_pos + 8 <= cmap_data.len() {
                let sub_off = read_u32(cmap_data, sub_pos + 4) as usize;
                if sub_off + 2 <= cmap_data.len() && read_u16(cmap_data, sub_off) == 4 {
                    fmt4_offset = Some(sub_off);
                    break;
                }
            }
        }

        let fmt4_off = fmt4_offset.ok_or("No format 4 cmap subtable found")?;
        let fmt4 = &cmap_data[fmt4_off..];
        let seg_count = (read_u16(fmt4, 6) / 2) as usize;

        if fmt4.len() < 16 + seg_count * 8 {
            return Err("cmap format 4 subtable truncated");
        }

        let end_codes_pos = 14;
        let start_codes_pos = 16 + seg_count * 2;
        let id_deltas_pos = 16 + seg_count * 4;
        let id_range_offsets_pos = 16 + seg_count * 6;

        let get_glyph_id = |cp: u16| -> u16 {
            for i in 0..seg_count {
                let end_code = read_u16(fmt4, end_codes_pos + i * 2);
                if end_code >= cp {
                    let start_code = read_u16(fmt4, start_codes_pos + i * 2);
                    if start_code <= cp {
                        let id_range_offset = read_u16(fmt4, id_range_offsets_pos + i * 2);
                        let id_delta = read_i16(fmt4, id_deltas_pos + i * 2);
                        if id_range_offset == 0 {
                            return (cp as i16).wrapping_add(id_delta) as u16;
                        } else {
                            let ro_addr = id_range_offsets_pos + i * 2;
                            let target_addr = ro_addr
                                + id_range_offset as usize
                                + ((cp - start_code) as usize * 2);
                            if target_addr + 2 <= fmt4.len() {
                                let gid = read_u16(fmt4, target_addr);
                                if gid != 0 {
                                    return (gid as i16).wrapping_add(id_delta) as u16;
                                }
                            }
                        }
                    }
                    break;
                }
            }
            0
        };

        let mut cps: Vec<u16> = Vec::with_capacity(COVERED_COUNT as usize);
        let mut glyphs: Vec<Option<TtfGlyph>> = Vec::with_capacity(COVERED_COUNT as usize);

        // Ascending, so `cps` stays sorted for `glyph_index`'s binary search.
        // Each iteration reserves its own slot first: the parse below has
        // several `continue` paths (malformed outline, short glyph slice) that
        // must leave a *present but empty* entry rather than shifting every
        // later glyph down by one.
        let mut covered: Vec<u16> = Vec::with_capacity(COVERED_COUNT as usize);
        for (lo, hi) in COVERED_RANGES {
            let mut cp = *lo;
            while cp < *hi {
                covered.push(cp as u16);
                cp += 1;
            }
        }
        covered.sort_unstable();
        covered.dedup();

        let mut unrenderable: Vec<bool> = Vec::with_capacity(COVERED_COUNT as usize);
        for cp in covered {
            cps.push(cp);
            glyphs.push(None);
            unrenderable.push(true);
            let slot = cps.len() - 1;
            let gid = get_glyph_id(cp) as usize;
            let (aw, lsb) = if gid < num_hmetrics && (gid * 4 + 4) <= hmtx_data.len() {
                let a = read_u16(hmtx_data, gid * 4) as f32;
                let l = read_i16(hmtx_data, gid * 4 + 2) as f32;
                (a, l)
            } else {
                (units_per_em * 0.6, 0.0)
            };

            // Resolve the outline through `resolve_glyph_lines`, which handles
            // simple *and* composite glyphs. The old body inlined only the
            // simple case and stored an empty outline for anything else, which
            // silently emptied every precomposed accented Latin letter.
            let mut lines: Vec<TtfLine> = Vec::new();
            let resolved = resolve_glyph_lines(
                gid,
                glyf_data,
                loca_data,
                index_to_loc_format,
                0,
                &mut lines,
            );

            if resolved != Outline::Drawn {
                // Either blank by design (a space: no ink, but perfectly
                // drawable) or an outline that would not parse. The metrics
                // from `hmtx` are correct either way, so the string stays
                // *spaced* correctly; only the ink differs. `unrenderable`
                // is what tells the two apart for the caller, so a space is
                // silent and a failure gets a visible placeholder.
                glyphs[slot] = Some(TtfGlyph {
                    advance: aw,
                    lsb,
                    bbox: (0.0, 0.0, 0.0, 0.0),
                    lines: Vec::new(),
                });
                unrenderable[slot] = resolved == Outline::Failed;
                continue;
            }

            // The bbox is the glyph header's own, which is already the union of
            // every component's for a composite. Re-deriving it from the lines
            // would also work, but the header is authoritative and free.
            let (x_min, y_min, x_max, y_max) = glyph_span(gid, loca_data, index_to_loc_format)
                .filter(|&(a, end)| a < end && end <= glyf_data.len())
                .map(|(a, end)| {
                    let gs = &glyf_data[a..end];
                    (
                        read_i16(gs, 2) as f32,
                        read_i16(gs, 4) as f32,
                        read_i16(gs, 6) as f32,
                        read_i16(gs, 8) as f32,
                    )
                })
                .unwrap_or((0.0, 0.0, 0.0, 0.0));

            glyphs[slot] = Some(TtfGlyph {
                advance: aw,
                lsb,
                bbox: (x_min, y_min, x_max, y_max),
                lines,
            });
            unrenderable[slot] = false;
        }

        Ok(Self {
            units_per_em,
            ascender,
            descender,
            cps,
            glyphs,
            unrenderable,
        })
    }

    /// Advance width in design units for codepoint `cp`.
    ///
    /// A codepoint that is absent from the font (or outside
    /// [`COVERED_RANGES`]) gets the same `0.6 em` the old byte-indexed table
    /// used for anything past ASCII, so an unrenderable character still
    /// *reserves* its slot. That is the behaviour that made the old pipeline so
    /// misleading: it reserved space and painted nothing, so CJK text was
    /// invisible *and* mis-spaced. The visible placeholder lives in
    /// `font::draw_glyph`; this stays a metric.
    #[inline]
    pub fn advance(&self, cp: char) -> f32 {
        if let Some(g) = self.glyph(cp) {
            return g.advance;
        }
        self.units_per_em * 0.6
    }

    /// Bounding box in design units for codepoint `cp`.
    ///
    /// `(0, 0, 0, 0)` means "no ink" and callers treat it as advance-only --
    /// a real space does the same thing, so an absent glyph and a space are
    /// indistinguishable here by design.
    #[inline]
    pub fn bbox(&self, cp: char) -> (f32, f32, f32, f32) {
        if let Some(g) = self.glyph(cp) {
            return g.bbox;
        }
        (0.0, 0.0, 0.0, 0.0)
    }

    /// Rasterize glyph `b` into mask at `(ox, oy)` with subpixel anti-aliasing.
    ///
    /// `weight_radius` is the stroke radius in pixels, i.e. half the stem
    /// width the caller wants added. It is applied as a **circular dilation of
    /// the filled outline**, which is what a heavier weight physically is:
    /// stems get thicker, counters shrink, terminals round up. Passing `0.0`
    /// gives the raw outline, which is the correct result for a font that
    /// already carries its own weight and for any use of this that is not
    /// going to be re-stroked.
    ///
    /// # Why a dilation and not a stroke
    ///
    /// A scanline polygon filler has no notion of stroke width: it fills
    /// whatever the outline encloses. Reconstructing weight by walking the
    /// skeleton and stamping discs is what the bitmap path does
    /// (`font.rs::raster_into_mask`), but a TrueType outline is a filled
    /// contour, not a skeleton, so there is nothing to walk. Dilating the
    /// *result* is the operation that matches, and it is the same one a
    /// rasteriser performs when a user increases a variable-font weight axis.
    ///
    /// The cost is one extra pass over the glyph mask on a cache miss, which
    /// is bounded by `GLYPH_MASK_MAX` and amortised by the glyph cache.
    #[allow(clippy::too_many_arguments)]
    pub fn rasterize_glyph(
        &self,
        cp: char,
        x: f32,
        scale: f32,
        baseline: f32,
        mask: &mut [u8],
        bw: i32,
        bh: i32,
        ox: i32,
        oy: i32,
        weight_radius: f32,
    ) -> bool {
        // One row of scratch for the separable weight dilation. Sized for the
        // largest glyph the caller can pass; the common path is 80.
        let mut dilate_scratch = [0u8; 1026 + 2];
        let Some(g) = self.glyph(cp) else {
            return false;
        };
        if g.lines.is_empty() {
            return false;
        }

        // Bounded stack array for line intersections per sub-scanline
        const MAX_INTERSECTIONS: usize = 32;
        let mut xs = [0.0f32; MAX_INTERSECTIONS];
        let mut touched = false;

        // Render each scanline row with 4x supersampling
        for r in 0..bh {
            let py = oy + r;
            let row_offset = (r as usize) * (bw as usize);

            for sub in 0..4 {
                let sy = py as f32 + (sub as f32 + 0.5) * 0.25;
                let mut count = 0;

                for l in &g.lines {
                    let sy0 = baseline - l.y0 as f32 * scale;
                    let sy1 = baseline - l.y1 as f32 * scale;

                    if (sy0 <= sy && sy < sy1) || (sy1 <= sy && sy < sy0) {
                        let sx0 = x + l.x0 as f32 * scale;
                        let sx1 = x + l.x1 as f32 * scale;
                        let xi = sx0 + (sy - sy0) * (sx1 - sx0) / (sy1 - sy0);
                        if count < MAX_INTERSECTIONS {
                            xs[count] = xi;
                            count += 1;
                        }
                    }
                }

                if count < 2 {
                    continue;
                }

                // Insertion sort intersections
                for i in 1..count {
                    let key = xs[i];
                    let mut j = i;
                    while j > 0 && xs[j - 1] > key {
                        xs[j] = xs[j - 1];
                        j -= 1;
                    }
                    xs[j] = key;
                }

                // Fill between pairs using even-odd rule
                let mut k = 0;
                while k + 1 < count {
                    let x0_span = xs[k];
                    let x1_span = xs[k + 1];

                    let c_start = (x0_span - ox as f32).floor().max(0.0) as i32;
                    let c_end = (x1_span - ox as f32).ceil().min(bw as f32) as i32;

                    for col in c_start..c_end {
                        let px_left = ox as f32 + col as f32;
                        let px_right = px_left + 1.0;
                        let overlap = (px_right.min(x1_span) - px_left.max(x0_span)).max(0.0);
                        if overlap > 0.0 {
                            let idx = row_offset + col as usize;
                            let added = (overlap * 64.0) as u16;
                            let new_val = (mask[idx] as u16 + added).min(255) as u8;
                            mask[idx] = new_val;
                            touched = true;
                        }
                    }
                    k += 2;
                }
            }
        }

        if touched && weight_radius > 0.0 {
            dilate_mask(
                mask,
                bw as usize,
                bh as usize,
                weight_radius,
                &mut dilate_scratch,
            );
        }
        touched
    }
}

/// Resolve a glyph id's outline to line segments, recursing into composites.
///
/// The `glyf` table stores a *composite* glyph (a precomposed `e` + acute
/// accent, for instance) as a header with a negative `numberOfContours`
/// followed by a list of component references, each with an offset and an
/// optional 2x2 transform. The original loop treated `num_contours <= 0` as
/// "compound or empty" and stored an empty outline, so every precomposed
/// accented Latin letter in Noto -- `\u{e9}`, `\u{fc}`, `\u{144}` and most of
/// Latin-1 and Latin Extended-A -- parsed to zero segments and drew nothing.
/// That is the majority of the coverage this module claims, so it is not an
/// edge case.
///
/// `depth` bounds the recursion. A malformed font can make a composite refer
/// to itself, and this is a load-time walk over untrusted bytes; without a cap
/// that is a stack overflow, i.e. an abort under `panic = "abort"`.
const COMPOSITE_MAX_DEPTH: u8 = 5;

/// What resolving a glyph's outline produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outline {
    /// Resolved to no ink, and that is correct: a space, or a zero-length
    /// `loca` entry. Drawable, with nothing to draw.
    Blank,
    /// Resolved to ink.
    Drawn,
    /// In range, but the outline would not parse. Not drawable.
    Failed,
}

fn resolve_glyph_lines(
    gid: usize,
    glyf: &[u8],
    loca: &[u8],
    index_to_loc_format: i16,
    depth: u8,
    out: &mut Vec<TtfLine>,
) -> Outline {
    if depth >= COMPOSITE_MAX_DEPTH {
        return Outline::Failed;
    }
    let (off_start, off_end) = match glyph_span(gid, loca, index_to_loc_format) {
        Some(v) => v,
        // A `loca` too short to cover this glyph is a malformed font, not an
        // intentionally blank glyph.
        None => return Outline::Failed,
    };
    if off_start >= off_end {
        // Zero-length entry: the spec's encoding of "no outline". A space.
        return Outline::Blank;
    }
    if off_end > glyf.len() {
        return Outline::Failed;
    }
    let g = &glyf[off_start..off_end];
    if g.len() < 10 {
        return Outline::Failed;
    }
    let num_contours = read_i16(g, 0);
    if num_contours == 0 {
        return Outline::Blank;
    }
    if num_contours > 0 {
        return if append_simple_glyph(g, num_contours as usize, out) {
            Outline::Drawn
        } else {
            Outline::Failed
        };
    }
    // Negative: a composite.
    if append_composite_glyph(g, glyf, loca, index_to_loc_format, depth, out) {
        Outline::Drawn
    } else if depth > 0 {
        // A nested component that is itself blank contributes no ink but is not
        // a failure of the whole composite.
        Outline::Blank
    } else {
        Outline::Failed
    }
}

/// `(start, end)` byte offsets of glyph `gid` in `glyf`, if `loca` covers it.
#[inline]
fn glyph_span(gid: usize, loca: &[u8], index_to_loc_format: i16) -> Option<(usize, usize)> {
    if index_to_loc_format == 0 {
        if (gid * 2 + 4) <= loca.len() {
            Some((
                (read_u16(loca, gid * 2) as usize) * 2,
                (read_u16(loca, (gid + 1) * 2) as usize) * 2,
            ))
        } else {
            None
        }
    } else if (gid * 4 + 8) <= loca.len() {
        Some((
            read_u32(loca, gid * 4) as usize,
            read_u32(loca, (gid + 1) * 4) as usize,
        ))
    } else {
        None
    }
}

/// The simple-glyph body: end points, flags, deltas, then contour flattening.
/// Appends to `out` and reports success.
fn append_simple_glyph(g: &[u8], nc: usize, out: &mut Vec<TtfLine>) -> bool {
    if nc == 0 {
        return true; // a genuinely empty outline, e.g. a space
    }
    if g.len() < 10 + nc * 2 {
        return false;
    }
    let mut end_pts = Vec::with_capacity(nc);
    for c in 0..nc {
        end_pts.push(read_u16(g, 10 + c * 2) as usize);
    }
    let num_pts = end_pts[nc - 1] + 1;
    let ins_len = read_u16(g, 10 + nc * 2) as usize;
    let mut ptr = 12 + nc * 2 + ins_len;

    let mut flags = Vec::with_capacity(num_pts);
    while flags.len() < num_pts && ptr < g.len() {
        let fl = g[ptr];
        ptr += 1;
        flags.push(fl);
        if (fl & 0x08) != 0 && ptr < g.len() {
            let repeat = g[ptr] as usize;
            ptr += 1;
            for _ in 0..repeat {
                if flags.len() < num_pts {
                    flags.push(fl);
                }
            }
        }
    }
    if flags.len() < num_pts {
        return false;
    }

    let mut xs = Vec::with_capacity(num_pts);
    let mut cur_x: i32 = 0;
    for &fl in &flags {
        if (fl & 0x02) != 0 {
            if ptr < g.len() {
                let dx = g[ptr] as i32;
                ptr += 1;
                cur_x += if (fl & 0x10) != 0 { dx } else { -dx };
            }
        } else if (fl & 0x10) == 0 && ptr + 2 <= g.len() {
            let dx = read_i16(g, ptr) as i32;
            ptr += 2;
            cur_x += dx;
        }
        xs.push(cur_x as f32);
    }

    let mut ys = Vec::with_capacity(num_pts);
    let mut cur_y: i32 = 0;
    for &fl in &flags {
        if (fl & 0x04) != 0 {
            if ptr < g.len() {
                let dy = g[ptr] as i32;
                ptr += 1;
                cur_y += if (fl & 0x20) != 0 { dy } else { -dy };
            }
        } else if (fl & 0x20) == 0 && ptr + 2 <= g.len() {
            let dy = read_i16(g, ptr) as i32;
            ptr += 2;
            cur_y += dy;
        }
        ys.push(cur_y as f32);
    }
    if xs.len() < num_pts || ys.len() < num_pts {
        return false;
    }

    let mut start_idx = 0;
    for &ep in &end_pts {
        if ep < start_idx || ep >= num_pts {
            continue;
        }
        let contour_len = ep - start_idx + 1;
        if contour_len >= 2 {
            let mut pts = Vec::with_capacity(contour_len);
            for i in start_idx..=ep {
                pts.push((xs[i], ys[i], (flags[i] & 0x01) != 0));
            }
            contour_to_lines(&pts, out);
        }
        start_idx = ep + 1;
    }
    true
}

/// `glyf` composite-glyph flags (`loca`-independent, from the spec).
mod comp {
    pub const ARGS_ARE_XY_VALUES: u16 = 0x0001;
    pub const WE_HAVE_A_SCALE: u16 = 0x0008;
    pub const MORE_COMPONENTS: u16 = 0x0020;
    pub const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
    pub const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;
    /// `WE_HAVE_INSTRUCTIONS`: the trailing instruction bytes are skipped.
    pub const WE_HAVE_INSTRUCTIONS: u16 = 0x0100;
}

/// Walk a composite glyph's component list, transforming each component's
/// outline into `out`.
///
/// Every component carries an offset plus an optional 2x2 matrix in F2Dot14
/// (2.14 fixed point). `ROUND_XY_TO_GRID` is deliberately ignored: it is a
/// hint for the grid-fitting pass, and we fit at rasterisation time anyway.
/// `USE_MY_METRICS` is likewise irrelevant -- metrics come from `hmtx`.
fn append_composite_glyph(
    g: &[u8],
    glyf: &[u8],
    loca: &[u8],
    index_to_loc_format: i16,
    depth: u8,
    out: &mut Vec<TtfLine>,
) -> bool {
    let mut ptr = 10usize; // past numberOfContours + the 4 bbox fields
    let mut any = false;
    // A bounded component count: a component list is at least 4 bytes per
    // entry, so this cannot be outrun by a well-formed slice, and it stops a
    // malformed one from looping on MORE_COMPONENTS forever.
    let mut budget = 256usize;
    loop {
        if budget == 0 || ptr + 4 > g.len() {
            break;
        }
        budget -= 1;
        let flags = read_u16(g, ptr);
        let component_gid = read_u16(g, ptr + 2) as usize;
        ptr += 4;

        // (dx, dy): either two bytes, two words, or nothing for a point match.
        let (dx, dy) = if (flags & comp::ARGS_ARE_XY_VALUES) != 0 {
            if (flags & comp::WE_HAVE_A_SCALE) != 0 {
                // The scale-bearing forms use F2Dot14 arguments.
                if ptr + 4 > g.len() {
                    break;
                }
                let x = f2dot14(read_i16(g, ptr));
                let y = f2dot14(read_i16(g, ptr + 2));
                ptr += 4;
                (x, y)
            } else {
                if ptr + 2 > g.len() {
                    break;
                }
                let (a, b) = (g[ptr] as i8 as f32, g[ptr + 1] as i8 as f32);
                ptr += 2;
                (a, b)
            }
        } else {
            (0.0, 0.0)
        };

        // The 2x2 matrix, defaulting to identity.
        let (mut m00, mut m01, mut m10, mut m11) = (1.0f32, 0.0f32, 0.0f32, 1.0f32);
        if (flags & comp::WE_HAVE_A_SCALE) != 0 {
            if ptr + 2 > g.len() {
                break;
            }
            let s = f2dot14(read_i16(g, ptr));
            ptr += 2;
            m00 = s;
            m11 = s;
        } else if (flags & comp::WE_HAVE_AN_X_AND_Y_SCALE) != 0 {
            if ptr + 4 > g.len() {
                break;
            }
            m00 = f2dot14(read_i16(g, ptr));
            m11 = f2dot14(read_i16(g, ptr + 2));
            ptr += 4;
        } else if (flags & comp::WE_HAVE_A_TWO_BY_TWO) != 0 {
            if ptr + 8 > g.len() {
                break;
            }
            m00 = f2dot14(read_i16(g, ptr));
            m01 = f2dot14(read_i16(g, ptr + 2));
            m10 = f2dot14(read_i16(g, ptr + 4));
            m11 = f2dot14(read_i16(g, ptr + 6));
            ptr += 8;
        }

        // Resolve the component into a scratch, transform, append. The scratch
        // is reused across components so a composite does not nest allocations.
        let mut sub = Vec::new();
        if resolve_glyph_lines(
            component_gid,
            glyf,
            loca,
            index_to_loc_format,
            depth + 1,
            &mut sub,
        ) == Outline::Drawn
        {
            for l in sub {
                out.push(TtfLine::new(
                    m00 * l.x0 as f32 + m10 * l.y0 as f32 + dx,
                    m01 * l.x0 as f32 + m11 * l.y0 as f32 + dy,
                    m00 * l.x1 as f32 + m10 * l.y1 as f32 + dx,
                    m01 * l.x1 as f32 + m11 * l.y1 as f32 + dy,
                ));
            }
            any = true;
        }

        if (flags & comp::MORE_COMPONENTS) == 0 {
            // Trailing instructions, if any, are not needed: we do not hint.
            let _ = comp::WE_HAVE_INSTRUCTIONS;
            break;
        }
    }
    any
}

/// F2Dot14: a signed 16-bit value with 14 fractional bits.
#[inline]
fn f2dot14(v: i16) -> f32 {
    v as f32 / 16384.0
}

/// Dilate a glyph coverage mask by `radius` pixels.
///
/// Separable: a horizontal pass then a vertical pass, so the cost is
/// `O(w * h)` per step regardless of radius, and the only scratch is a single
/// row -- `scratch` must hold `max(w, h)` bytes. No allocation, no distance
/// field, no square root.
///
/// # Why a box and not a disc
///
/// A disc needs the Euclidean distance to the nearest ink pixel, which is a
/// two-pass chamfer over a *full-mask* distance buffer. At
/// `GLYPH_MASK_LEN = 6400` that is 25 KB of `f32` on the stack -- too much for
/// a function this deep in the call chain -- or a heap allocation on what is
/// otherwise an allocation-free path, which `AGENTS.md` forbids.
///
/// The radii involved are tiny: the weight delta from Regular is
/// `(pen(weight) - pen(Regular)) * size / 1000`, which is 0.24 px at 48 px and
/// 2.2 px at 96 px. A box and a disc of that radius differ only in the
/// corners, by well under a pixel, and a weight axis is not a place where that
/// shows. The dilation is still doing the physically right thing: stems
/// thicken, counters shrink, the glyph grows in every direction.
///
/// # Fractional radius
///
/// The whole point of a weight axis is that it is continuous, so the last
/// step is partial: a 0.24 px dilation has to darken the edge slightly rather
/// than not at all. A hard `d < radius` gate on an integer-pixel distance
/// quantises the axis to whole pixels, and made Medium render identically to
/// Regular at every size below ~208 px.
///
/// # Cost, measured
///
/// This runs on a glyph *cache miss*, so it is a boot cost and not a frame
/// cost: the glyph cache is warm from the second frame onwards, and
/// `screenshot::full_frame_stays_inside_the_vsync_budget` is unchanged by it
/// (all 13 states, release, before and after).
///
/// The cold cost, A/B'd on this host by setting `OUTLINE_WEIGHT_SCALE` to 0.6
/// and to 0.0 and timing 40 lines of 37-character Medium/Bold text into a
/// cold 1080x2400 buffer -- 1480 distinct glyph rasterisations:
///
/// | weight  | dilation off | dilation on |
/// |---------|--------------|-------------|
/// | Regular | 18.6 ms      | 18.4 ms    |
/// | Medium  | 18.3 ms      | 22.5 ms    |
/// | Bold    | 17.0 ms      | 23.7 ms    |
///
/// So a weight step costs about +27% of cold rasterisation, which is what 1-2
/// extra separable passes over the mask should cost. Regular is unaffected
/// because its delta from itself is zero, so icon labels -- the bulk of the
/// text on a launcher screen -- pay nothing.
///
/// `font::tests::cold_weight_rasterisation_stays_bounded` guards this.
fn dilate_mask(mask: &mut [u8], w: usize, h: usize, radius: f32, scratch: &mut [u8]) {
    if radius <= 0.0 || w == 0 || h == 0 || mask.len() < w * h {
        return;
    }
    if scratch.len() < w.max(h) {
        return;
    }
    if !mask[..w * h].iter().any(|v| *v != 0) {
        return;
    }
    let whole = radius.floor();
    let frac = radius - whole;
    for _ in 0..whole as u32 {
        box_step(mask, w, h, scratch, 255);
    }
    if frac > 0.0 {
        let f = (frac * 255.0).round() as u8;
        box_step(mask, w, h, scratch, f);
    }
}

/// Calibration for the outline weight axis. See the call site in
/// `font.rs::draw_glyph` for why the raw pen delta is not used directly.
pub const OUTLINE_WEIGHT_SCALE: f32 = 0.6;

/// One separable box-dilation step, taking the max against existing coverage.
///
/// `weight == 255` is a full step; a smaller value is the fractional tail.
///
/// The scaled *candidate* is compared with what is already there, never written
/// over it. Writing the scaled value unconditionally whenever it was the
/// larger of the two looks equivalent and is not: `m > row[x]` holds for a
/// partially covered pixel just as much as for an empty one, so a 100-alpha
/// pixel got overwritten with `100 * 20 / 255 = 8`. The weight axis was
/// therefore *eroding* text -- Medium measured 0.55x the coverage of Regular at
/// 8 px, i.e. bolder came out lighter, and the error grew as the glyph shrank
/// and the fractional weight shrank with it.
fn box_step(mask: &mut [u8], w: usize, h: usize, scratch: &mut [u8], weight: u8) {
    // Horizontal: each output row is the max of its input row and the two
    // neighbours, with the dilated value scaled by `weight`.
    for y in 0..h {
        let row = &mut mask[y * w..y * w + w];
        let src = &mut scratch[..w];
        src.copy_from_slice(row);
        for x in 0..w {
            let mut m = src[x];
            if x > 0 {
                m = m.max(src[x - 1]);
            }
            if x + 1 < w {
                m = m.max(src[x + 1]);
            }
            // Scale the *candidate*, then take the max against what is
            // already there.
            //
            // Writing the scaled value unconditionally whenever it was the
            // larger of the two looked equivalent and was not: `m > row[x]`
            // is true for a partially-covered pixel just as much as for an
            // empty one, so a 100-alpha pixel got overwritten with
            // 100*20/255 = 8. The weight axis was therefore *eroding* text --
            // Medium measured 0.55x the coverage of Regular at 8 px, i.e.
            // bolder came out lighter, and the error grew as the glyph got
            // smaller and the fractional weight shrank.
            let a = if weight == 255 {
                m
            } else {
                ((m as u16 * weight as u16 + 127) / 255) as u8
            };
            if a > row[x] {
                row[x] = a;
            }
        }
    }
    // Vertical: same, but down each *column* -- so the outer loop is over
    // `w`, not `h`. Running it over `h` reads past the end of the mask
    // whenever h > w, and silently skips the trailing columns when h < w, so
    // a glyph's apparent weight depended on its own aspect ratio.
    for y in 0..w {
        let src = &mut scratch[..h];
        for (i, v) in src.iter_mut().enumerate().take(h) {
            *v = mask[i * w + y];
        }
        for i in 0..h {
            let mut m = src[i];
            if i > 0 {
                m = m.max(src[i - 1]);
            }
            if i + 1 < h {
                m = m.max(src[i + 1]);
            }
            let a = if weight == 255 {
                m
            } else {
                ((m as u16 * weight as u16 + 127) / 255) as u8
            };
            let idx = i * w + y;
            if a > mask[idx] {
                mask[idx] = a;
            }
        }
    }
}

/// Convert a contour of on/off curve points into a sequence of flat line segments.
fn contour_to_lines(pts: &[(f32, f32, bool)], lines: &mut Vec<TtfLine>) {
    let n = pts.len();
    if n < 2 {
        return;
    }

    // Expand implicit on-curve points between consecutive off-curve points
    let mut expanded = Vec::with_capacity(n * 2);
    for i in 0..n {
        let p_curr = pts[i];
        let p_next = pts[(i + 1) % n];
        expanded.push(p_curr);
        if !p_curr.2 && !p_next.2 {
            expanded.push((
                (p_curr.0 + p_next.0) * 0.5,
                (p_curr.1 + p_next.1) * 0.5,
                true,
            ));
        }
    }

    // Rotate so the start is on-curve
    let start_idx = expanded.iter().position(|p| p.2).unwrap_or(0);
    let total = expanded.len();

    let mut i = 0;
    while i < total {
        let p0 = expanded[(start_idx + i) % total];
        let p1 = expanded[(start_idx + i + 1) % total];

        if p1.2 {
            // Straight line segment
            lines.push(TtfLine::new(p0.0, p0.1, p1.0, p1.1));
            i += 1;
        } else {
            // Quadratic Bézier curve: p0 (start), p1 (control), p2 (end)
            let p2 = expanded[(start_idx + i + 2) % total];
            // Flatten into 4 chords for sub-pixel accuracy
            for step in 0..4 {
                let t0 = step as f32 * 0.25;
                let t1 = (step + 1) as f32 * 0.25;

                let mt0 = 1.0 - t0;
                let ax0 = mt0 * mt0 * p0.0 + 2.0 * mt0 * t0 * p1.0 + t0 * t0 * p2.0;
                let ay0 = mt0 * mt0 * p0.1 + 2.0 * mt0 * t0 * p1.1 + t0 * t0 * p2.1;

                let mt1 = 1.0 - t1;
                let ax1 = mt1 * mt1 * p0.0 + 2.0 * mt1 * t1 * p1.0 + t1 * t1 * p2.0;
                let ay1 = mt1 * mt1 * p0.1 + 2.0 * mt1 * t1 * p1.1 + t1 * t1 * p2.1;

                lines.push(TtfLine::new(ax0, ay0, ax1, ay1));
            }
            i += 2;
        }
    }
}

#[inline]
fn read_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([buf[offset], buf[offset + 1]])
}

#[inline]
fn read_i16(buf: &[u8], offset: usize) -> i16 {
    i16::from_be_bytes([buf[offset], buf[offset + 1]])
}

#[inline]
fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_noto_loads_and_parses() {
        let font = TrueTypeFont::load_system_noto();
        assert!(
            font.is_some(),
            "System Google Noto Sans TTF should be found on this system"
        );
        let font = font.unwrap();
        assert_eq!(font.units_per_em, 1000.0);

        // Digits 0..9 should be tabular
        let d0_adv = font.advance('0');
        assert!(d0_adv > 0.0);
        for c in '1'..='9' {
            assert_eq!(font.advance(c), d0_adv, "Noto Sans digits must be tabular");
        }

        // Test rasterizing 'A' into a small mask
        let mut mask = [0u8; 64 * 64];
        let touched = font.rasterize_glyph(
            'A',
            10.0,
            30.0 / 1000.0,
            45.0,
            &mut mask,
            30,
            40,
            10,
            10,
            0.0,
        );
        assert!(touched, "Glyph 'A' must produce non-zero mask pixels");
        let inked_count = mask.iter().filter(|&&v| v > 0).count();
        assert!(inked_count > 50, "Glyph 'A' should ink substantial pixels");
    }

    #[test]
    fn test_fallback_on_corrupt_data() {
        let garbage = [0u8; 32];
        let res = TrueTypeFont::parse(&garbage);
        assert!(res.is_err(), "Garbage bytes must be rejected gracefully");
    }
}

#[cfg(test)]
mod coverage {
    use super::*;

    /// The table is sorted and every column is the same length.
    ///
    /// `cps` and `glyphs` are parallel and the lookup is a binary search, so a
    /// desync or an unsorted table would mis-resolve glyphs silently rather
    /// than panic. This is the invariant those two properties rest on.
    #[test]
    fn the_codepoint_table_is_sorted_and_rectangular() {
        let Some(f) = TrueTypeFont::load_system_noto() else {
            // No system font here: nothing to assert about a table that was
            // never built. Not a skip, just nothing to check.
            return;
        };
        assert_eq!(
            f.cps.len(),
            f.glyphs.len(),
            "cps and glyphs must be parallel"
        );
        assert_eq!(
            f.cps.len(),
            f.unrenderable.len(),
            "unrenderable must be parallel too"
        );
        for w in f.cps.windows(2) {
            assert!(w[0] < w[1], "cps must be strictly ascending: {w:?}");
        }
    }

    /// `can_draw` and the parsed outlines agree.
    ///
    /// A `false` with a non-empty outline would mean the placeholder is drawn
    /// over a real glyph; a `true` with none would mean another silent gap.
    #[test]
    fn can_draw_agrees_with_the_parsed_outline() {
        let Some(f) = TrueTypeFont::load_system_noto() else {
            return;
        };
        for (i, cp) in f.cps.iter().enumerate() {
            let cp = char::from_u32(*cp as u32).unwrap();
            let has_ink = f.glyphs[i].as_ref().is_some_and(|g| !g.lines.is_empty());
            if f.unrenderable[i] {
                assert!(
                    !has_ink,
                    "{cp:?} is marked unrenderable but has {} lines",
                    f.glyphs[i].as_ref().unwrap().lines.len()
                );
            }
        }
    }

    /// The ranges this module claims to cover really are the ones a launcher
    /// needs, and a representative sample of each resolves to ink.
    ///
    /// A representative character per block, chosen as one that is *not* a bare
    /// ASCII letter so the test actually exercises the cmap round trip.
    #[test]
    fn the_claimed_blocks_resolve_to_real_glyphs() {
        let Some(f) = TrueTypeFont::load_system_noto() else {
            return;
        };
        for (label, cp) in [
            ("Latin-1", '\u{e9}'),      // e acute
            ("Latin-1", '\u{fc}'),      // u diaeresis
            ("Latin-1", '\u{f1}'),      // n tilde
            ("Latin Ext-A", '\u{104}'), // A with macron
            ("Latin Ext-A", '\u{15b}'), // s with cedilla
            ("Greek", '\u{3b1}'),       // alpha
            ("Greek", '\u{3c9}'),       // omega
            ("Cyrillic", '\u{416}'),    // Zhe
            ("Cyrillic", '\u{44f}'),    // ya
        ] {
            let idx = f
                .glyph_index(cp)
                .unwrap_or_else(|| panic!("{label}: {cp:?} is not in the table at all"));
            assert!(
                !f.unrenderable[idx],
                "{label}: {cp:?} is in the table but its outline would not render"
            );
            let lines = f.glyphs[idx].as_ref().unwrap().lines.len();
            assert!(
                lines > 0,
                "{label}: {cp:?} has no ink -- this is the silent-blank-text defect"
            );
        }
    }

    /// A codepoint outside the covered ranges is absent, not broken.
    ///
    /// CJK ideographs and Hangul are deliberately not pre-parsed, and emoji
    /// are not in Noto at all. Both must return "no glyph" so the caller draws
    /// a visible placeholder -- never a panic and never a silent gap.
    #[test]
    fn uncovered_codepoints_are_absent_rather_than_broken() {
        let Some(f) = TrueTypeFont::load_system_noto() else {
            return;
        };
        for cp in ['\u{4e2d}', '\u{ac00}', '\u{1f600}', '\u{1d7d8}'] {
            assert!(
                f.glyph_index(cp).is_none(),
                "{cp:?} should be outside COVERED_RANGES"
            );
            assert!(!f.can_draw(cp));
            // Metrics still work, so a run of them is *spaced* correctly.
            assert!(f.advance(cp) > 0.0, "{cp:?} must still reserve an advance");
        }
    }
}
