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
//!   per-scanline coverage accumulator is a fixed 256 byte stack array and
//!   curves are flattened on the fly. Nothing is allocated, ever.

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
/// Em size in pixels for UI scale 1. Scale 2 is the shell's body size
/// (~30px em, ~21px cap height on a 1080x2400 panel).
pub const EM_BASE_PX: f32 = 15.0;

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
pub struct Sub {
    pub s: (f32, f32),
    pub p: &'static [Q],
}

/// A glyph: advance width in design units plus its sub-paths.
pub struct Glyph {
    pub a: f32,
    pub sub: &'static [Sub],
}

include!("font_glyphs.rs");

/// Fallback advance for code points outside the Latin-1 table.
const FALLBACK_ADVANCE: f32 = 600.0;

#[inline]
fn glyph_of(b: u8) -> &'static Glyph {
    let i = (b.wrapping_sub(0x20) as usize).min(GLYPHS.len() - 1);
    &GLYPHS[i]
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

/// Advance width of one character, in pixels.
#[inline]
pub fn char_advance(b: u8, size_px: f32) -> f32 {
    let a = if (0x20..0x80).contains(&b) {
        glyph_of(b).a
    } else {
        FALLBACK_ADVANCE
    };
    a * size_px / UNITS_PER_EM
}

/// Total advance width of a string, in pixels.
pub fn measure(text: &str, size_px: f32) -> f32 {
    let mut total = 0.0;
    for b in text.bytes() {
        total += char_advance(b, size_px);
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
    b: u8,
    color: u32,
    size_px: f32,
    weight: FontWeight,
) -> f32 {
    let g = glyph_of(b);
    let s = size_px / UNITS_PER_EM;
    let r = weight.pen() * s;
    let baseline = y + ASCENDER * s;
    let mut pen = x;

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
            // Off screen: nothing to rasterise, but the pen still advances.
            pen += g.a * s;
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
                    // Conservative per-curve window (control hull + pen).
                    let cmin_x = ax.min(bx) - r - 1.0;
                    let cmax_x = ax.max(bx) + r + 1.0;
                    let cmin_y = ay.min(by) - r - 1.0;
                    let cmax_y = ay.max(by) + r + 1.0;
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
                    for px in lo..=hi {
                        let fxp = px as f32 + 0.5;
                        let cov = (r + 0.5
                            - dist_quad2(fxp, sy, px0, py0, cx, cy, bx, by).sqrt())
                            .clamp(0.0, 1.0);
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
                // Composite the accumulated tile once: no repeated blending,
                // so overlapping curves never darken each other.
                let row_base = (py as usize) * stride;
                for px in chunk..chunk_end {
                    let a = row[(px - chunk) as usize];
                    if a != 0 {
                        let i = row_base + px as usize;
                        buf[i] = blend_over(buf[i], color, a);
                    }
                }
                chunk = chunk_end;
            }
        }
    }

    pen += g.a * s;
    pen - x
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
    for b in text.bytes() {
        if b == b'\n' {
            continue;
        }
        pen += draw_glyph(buf, stride, w, h, pen, y, b, color, size_px, weight);
    }
    pen - x
}

/// Baseline offset in pixels below the ascender line, for call sites that want
/// to align text optically (centring on a cap, a row, an icon).
#[inline]
pub fn baseline_drop(size_px: f32) -> f32 {
    ASCENDER * size_px / UNITS_PER_EM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_are_proportional_and_tabular() {
        // Every printable ASCII glyph must have a positive, sane advance.
        for c in 0x20u8..0x80 {
            let a = glyph_of(c).a;
            assert!((200.0..=900.0).contains(&a), "glyph {} advance {}", c, a);
        }
        // Digits are tabular: the clock never jitters.
        let d0 = char_advance(b'0', 100.0);
        for c in b'1'..=b'9' {
            assert_eq!(char_advance(c, 100.0), d0);
        }
        // Proportional: an "i" is much narrower than an "m".
        assert!(char_advance(b'i', 100.0) < char_advance(b'm', 100.0) * 0.5);
        // And the table sums to the measured string.
        assert!((measure("ill", 100.0) - 3.0 * char_advance(b'i', 100.0)).abs() < 0.001);
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
            assert!(min_y < -100.0, "{} must descend (min y {})", c as char, min_y);
        }
        for c in *b"bdhkltf" {
            let g = glyph_of(c);
            let mut max_y = f32::MIN;
            for sub in g.sub {
                for q in sub.p {
                    max_y = max_y.max(sub.s.1).max(q.1).max(q.3);
                }
            }
            assert!(max_y > X_HEIGHT + 100.0, "{} must ascend (max y {})", c as char, max_y);
        }
    }

    #[test]
    fn weight_axis_thickens_stems() {
        assert!(FontWeight::Bold.pen() > FontWeight::Medium.pen());
        assert!(FontWeight::Medium.pen() > FontWeight::Regular.pen());
    }

    #[test]
    fn coverage_is_analytic_and_never_overshoots_the_pen() {
        // A vertical stem of the letter "l" must be exactly two pen radii
        // wide, and the ramp at its edge must span one pixel.
        let size = 60.0;
        let (w, h) = (80usize, 120usize);
        let mut buf = vec![0xFF000000u32; w * h];
        let adv = draw_run(&mut buf, w, w, h, 20.0, 10.0, "l", 0xFFFFFFFF, size, FontWeight::Regular);
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
            ("Tap to search apps, web and settings", 1, FontWeight::Regular),
            ("The quick brown fox jumps over the lazy dog", 1, FontWeight::Regular),
            ("10:34", 3, FontWeight::Medium),
        ];
        // Warm up so the measurement is not dominated by first-touch faults.
        for (text, scale, weight) in lines {
            draw_run(&mut buf, w, w, h, 10.0, 10.0, text, 0xFFFFFFFF, em_px(scale), weight);
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
            if cfg!(debug_assertions) { "debug" } else { "release" }
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
}
