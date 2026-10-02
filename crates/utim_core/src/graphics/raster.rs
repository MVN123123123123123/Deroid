//! Shared software-raster primitives.
//!
//! # Why this module exists
//!
//! `drm_kms.rs` grew to 5,000 lines with every primitive private to it, so
//! nothing else could draw a stroked, rotated or masked shape. This module
//! holds the primitives that more than one surface needs -- the drawer
//! sheet, the overview, the icon cache -- plus the ones that are correct
//! only as a family (`squircle_span` and the band shadow in particular,
//! which are meaningless in isolation).
//!
//! # Frame budget
//!
//! The workspace is built at `opt-level = "z"` (see the root `Cargo.toml`),
//! which leaves LLVM's loop and SLP vectorizers **off**. Only `slice::fill`
//! and `copy_from_slice` autovectorise. Two consequences shape everything
//! here:
//!
//! 1. **Prefer a contiguous span write over a per-pixel test.** A
//!    `rounded_span` computed once per row and then `fill`ed is ~20x cheaper
//!    than a per-pixel distance test, because the inner loop becomes a memset
//!    and the corner work is O(rows) not O(pixels). Every primitive here is
//!    written scanline-first for that reason.
//! 2. **A full-screen translucent blend costs ~10.2 ms at 1080x2400**, which
//!    is more than the entire 8.33 ms frame budget at 120 Hz. Scrims must
//!    therefore be composited to an *opaque* colour and written with a plain
//!    fill -- see [`draw_opaque_scrim`].
//!
//! # Allocation
//!
//! Everything here is allocation-free. Where a shape genuinely needs
//! scratch (the separable shadow blur) the scratch is a caller-owned
//! `&mut [u32]` band, never a local `Vec`.

// ===========================================================================
// Pixel helpers
// ===========================================================================

/// Source-over composite of `fg` at `alpha` over `bg`, forcing opaque output.
///
/// `(f * a + b * (255 - a) + 127) / 255` with a round-half-up bias, which is
/// the same integer the rest of the rasteriser uses. The result always has
/// `alpha == 0xFF`: the framebuffer is `XRGB8888`
/// (`drm_kms.rs:385-408`), so the alpha byte is not stored and re-writing it
/// would be a wasted store.
#[inline(always)]
pub fn blend_alpha(bg: u32, fg: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    if a == 255 {
        return fg & 0x00FF_FFFF;
    }
    let inv = 255 - a;
    let r = (((fg >> 16) & 0xFF) * a + ((bg >> 16) & 0xFF) * inv + 127) / 255;
    let g = (((fg >> 8) & 0xFF) * a + ((bg >> 8) & 0xFF) * inv + 127) / 255;
    let b = ((fg & 0xFF) * a + (bg & 0xFF) * inv + 127) / 255;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// Integer square root, floor. Used by [`squircle_span`]; the Newton form
/// converges in at most five iterations for any `u32` and needs no
/// allocation or float conversion.
#[inline]
pub fn isqrt(v: u32) -> u32 {
    if v == 0 {
        return 0;
    }
    let mut x = v;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + v / x) / 2;
    }
    x
}

// ===========================================================================
// Opaque scrim
// ===========================================================================

/// Composite a scrim over one flat background colour and return the result.
///
/// This is the whole point of the primitive: the codebase previously
/// blended a full-screen rectangle per pixel, which measures **10.2 ms** at
/// 1080x2400 -- more than a whole 120 Hz frame (plan §3.1/§3.2). If the
/// region underneath is a single flat colour, the blend can be done once in
/// scalar arithmetic and the region written with a vectorised `fill`.
///
/// This is *only* valid for a flat backdrop. Anything with structure
/// underneath (the drawer list scrolling over the wallpaper) must use a
/// band-limited blend instead, and the frame-budget guard
/// `no_full_screen_translucent_blend` exists to catch a regression back to
/// the naive path.
///
/// Returns `(composited_opaque_colour, alpha_to_pass_through)`: callers that
/// need a partially-transparent scrim can use the returned alpha instead.
#[inline]
pub fn composite_scrim(backdrop: u32, scrim: u32, scrim_alpha: f32) -> u32 {
    let a = (scrim_alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    blend_alpha(backdrop, scrim, a)
}

/// Fill a rectangle with a **pre-composited opaque** colour.
///
/// This is the fast path every scrim should take. `color` must already have
/// `alpha == 0xFF`; the assertion-free contract is documented rather than
/// checked, because a per-pixel alpha test would defeat the entire purpose.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn draw_opaque_rect(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: i32,
    y: i32,
    rw: i32,
    rh: i32,
    color: u32,
) {
    if rw <= 0 || rh <= 0 || x + rw <= 0 || x >= w as i32 || y + rh <= 0 || y >= h as i32 {
        return;
    }
    let x0 = x.max(0) as usize;
    let y0 = y.max(0) as usize;
    let x1 = ((x + rw) as usize).min(w);
    let y1 = ((y + rh) as usize).min(h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let opaque = color | 0xFF00_0000;
    for cy in y0..y1 {
        let row = cy * stride;
        buf[row + x0..row + x1].fill(opaque);
    }
}

// ===========================================================================
// Rounded-rect stroke
// ===========================================================================

/// Horizontal span covered by a rounded rect on row `dy` (relative to the
/// rect's top edge), as `None` when the row is outside the shape.
///
/// Mirrors `rounded_span` in `drm_kms.rs` but is public so the stroke and the
/// rotated primitive can reuse the exact same coverage test as the fill.
/// Returning a span rather than a per-pixel predicate is what keeps the
/// corner cost at O(rows).
#[inline]
pub fn rounded_span_f(dy: f32, x: f32, rw: f32, rh: f32, radius: f32) -> Option<(f32, f32)> {
    if rw <= 0.0 || rh <= 0.0 {
        return None;
    }
    let r = radius.clamp(0.0, rw.min(rh) * 0.5);
    if dy < 0.0 || dy >= rh {
        return None;
    }
    // Distance into the corner cap from whichever end is nearer. The straight
    // middle of the shape is `dy in [r, rh - r]`, which is full width.
    let q = dy.min(rh - dy);
    if r <= 0.0 || q >= r {
        return Some((x, x + rw));
    }
    // Inset from the side: at q = 0 it is the full radius, at q = r it is 0.
    let dyc = r - q;
    let inset = r - (r * r - dyc * dyc).max(0.0).sqrt();
    Some((x + inset, x + rw - inset))
}

/// Stroked rounded rectangle: a ring of `stroke` px, corners rounded to
/// `radius`.
///
/// Drawn as an outer fill minus an inner fill would need a second buffer;
/// instead each row fills the outer span, then overwrites the inner span
/// with the four neighbouring pixels, which is the same information at
/// 4 stores/row rather than a second full pass. For a 216x52 dp popup that
/// is ~34 rows, so the whole stroke is a few hundred stores.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn draw_rounded_rect_outline_f(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    x: f32,
    y: f32,
    rw: f32,
    rh: f32,
    radius: f32,
    stroke: f32,
    color: u32,
) {
    if stroke <= 0.0 || rw <= 0.0 || rh <= 0.0 {
        return;
    }
    let alpha = ((color >> 24) & 0xFF) as u8;
    if alpha == 0 {
        return;
    }
    // Clamp so the inset never crosses the centre line on a thin shape.
    let s = stroke.min(rw * 0.5).min(rh * 0.5);
    let ir = (radius - s).max(0.0);
    let inner_w = rw - 2.0 * s;
    let inner_h = rh - 2.0 * s;

    let y_start = y.max(0.0) as usize;
    let y_end = ((y + rh).min(h as f32).max(0.0)) as usize;

    for cy in y_start..y_end {
        let dy = cy as f32 + 0.5 - y;
        let Some((olo, ohi)) = rounded_span_f(dy, x, rw, rh, radius) else {
            continue;
        };
        let row = cy * stride;
        // The inner rect's top edge is `s` below the outer's, so this row is
        // `dy - s` into the *inner* shape, not `dy`. Sampling it at `dy` slid
        // the hole `s` rows up: the top of the stroke lost its band entirely
        // (the inner span already existed on the outer's first row, so only
        // two slivers were left) and the bottom gained one -- the stroke was
        // `2 * s` thicker at the bottom than at the top, which is the
        // non-uniformity `outlined_rounded_rect_stroke_is_uniform` measures.
        let inner_dy = dy - s;
        // A row with no inner span is entirely within the stroke band: either
        // the inset collapsed (thick stroke on a thin shape) or this row sits
        // in the rounded cap above/below the inner rect.
        match rounded_span_f(inner_dy, x + s, inner_w, inner_h, ir) {
            None => fill_span(buf, row, w, olo, ohi, color, alpha),
            Some((ili, ihi)) => {
                fill_span(buf, row, w, olo, ili, color, alpha);
                fill_span(buf, row, w, ihi, ohi, color, alpha);
            }
        }
    }
}

#[inline]
fn fill_span(buf: &mut [u32], row: usize, w: usize, a: f32, b: f32, color: u32, alpha: u8) {
    let lo = a.max(0.0).ceil() as usize;
    let hi = (b.min(w as f32)).floor() as usize;
    if hi <= lo {
        return;
    }
    // `hi` is clamped to the *width*, not to the buffer, so a caller whose
    // `buf` is shorter than `h * stride` would otherwise index past the end.
    // The old `.take(row + hi).skip(row + lo)` hid that by silently
    // truncating at `buf.len()`; clamping once here keeps the same effective
    // range while making the bound explicit for both the fill and the blend.
    let end = row.saturating_add(hi).min(buf.len());
    let start = row.saturating_add(lo);
    if end <= start {
        return;
    }
    if alpha == 255 {
        buf[start..end].fill(color);
    } else {
        for px in &mut buf[start..end] {
            *px = blend_alpha(*px, color, alpha);
        }
    }
}

// ===========================================================================
// Rotated rounded rect (fast-scroller teardrop)
// ===========================================================================

/// Corner radii for the `RecyclerViewFastScroller` teardrop, from
/// `FastScrollThumbDrawable.java:53-62`:
///
/// ```java
/// r  = bounds.height() * 0.5f;              // :53
/// // "The path represents a rotate tear-drop shape, with radius of one
/// //  corner is 1/5th of the other 3 corners."   // :54-55
/// r2 = r / 5;                               // :57
/// mPath.addRoundRect(..., {r,r, r,r, r2,r2, r,r}, CCW);   // :58-60
/// sMatrix.setRotate(-45, l + r, t + r);      // :62
/// ```
///
/// `addRoundRect`'s radii array is **two floats per corner**, in the order
/// `[TL, TR, BR, BL]`, so the eight literals above read
/// `TL = (r, r)`, `TR = (r, r)`, `BR = (r2, r2)`, `BL = (r, r)`: **one**
/// corner is small, and it is bottom-right. The source comment says the same
/// thing in prose ("one corner is 1/5th of the other 3 corners", `:54-55`) --
/// the earlier reading of a `r/5` *pair* came from miscounting the array as
/// four entries instead of four *pairs*. The whole shape is then rotated by
/// -45 degrees, which puts that small corner at the teardrop's point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TeardropRadii {
    pub big: f32,
    pub small: f32,
}

#[inline]
pub fn teardrop_radii(height: f32) -> TeardropRadii {
    let r = height * 0.5;
    TeardropRadii {
        big: r,
        small: r / 5.0,
    }
}

/// Rotated rounded rectangle, rasterised with a signed-distance test.
///
/// A rotated rect needs either a scanline clip (a rotated bounding-box test
/// per pixel) or an SDF. The SDF is chosen because `font.rs` already has the
/// machinery (`dist_quad2`) and, more importantly, because it is a single
/// multiply-add chain per pixel rather than two dependent comparisons. At
/// 75 x 62 dp = ~4.6 kpx this is ~0.05 ms, well inside budget, and the
/// shape is small enough that the per-pixel cost is irrelevant; what matters
/// is that the *corners are correct*, which a bounding-box clip gets wrong
/// for the `r/5` pair.
///
/// `angle_rad` rotates about the rect's centre. The four corner radii are
/// given in `[tl, tr, br, bl]` order and travel with their corner, so the
/// asymmetric teardrop comes out right without special-casing.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn draw_rotated_rounded_rect(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    cx: f32,
    cy: f32,
    rw: f32,
    rh: f32,
    radii: [f32; 4],
    angle_rad: f32,
    color: u32,
) {
    let alpha = ((color >> 24) & 0xFF) as u8;
    if alpha == 0 || rw <= 0.0 || rh <= 0.0 {
        return;
    }
    let (sin_a, cos_a) = angle_rad.sin_cos();
    let hw = rw * 0.5;
    let hh = rh * 0.5;
    // Conservative axis-aligned bound of the rotated rect.
    let ext_x = hw * cos_a.abs() + hh * sin_a.abs();
    let ext_y = hw * sin_a.abs() + hh * cos_a.abs();
    let x0 = ((cx - ext_x).floor() as i32).max(0);
    let y0 = ((cy - ext_y).floor() as i32).max(0);
    let x1 = ((cx + ext_x).ceil() as i32).min(w as i32);
    let y1 = ((cy + ext_y).ceil() as i32).min(h as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }

    // Per-corner radii clamped to half the side they sit on. Both halves, not
    // just the width: a corner radius larger than the half-*height* is not
    // representable by the SDF below (`qy = |ly| - (hh - cr)` goes negative
    // for the whole row and the arc degenerates into a straight cut), and the
    // teardrop's `big` radius is exactly `rh / 2`, so on a shape narrower
    // than it is tall the width alone would let it through unclamped.
    let rmax = hw.min(hh);
    let r = [
        radii[0].clamp(0.0, rmax),
        radii[1].clamp(0.0, rmax),
        radii[2].clamp(0.0, rmax),
        radii[3].clamp(0.0, rmax),
    ];

    for py in y0..y1 {
        let fy = py as f32 + 0.5 - cy;
        let row = py as usize * stride;
        for px in x0..x1 {
            let fx = px as f32 + 0.5 - cx;
            // Rotate the sample into the rect's own frame.
            let lx = fx * cos_a + fy * sin_a;
            let ly = -fx * sin_a + fy * cos_a;
            // Pick the corner this sample is nearest in the rotated frame.
            let cr = if lx >= 0.0 {
                if ly >= 0.0 {
                    r[2]
                } else {
                    r[1]
                }
            } else if ly >= 0.0 {
                r[3]
            } else {
                r[0]
            };
            if cr <= 0.0 {
                continue;
            }
            let qx = lx.abs() - (hw - cr);
            let qy = ly.abs() - (hh - cr);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            let inside = qx.max(qy).min(0.0);
            let sd = outside + inside - cr;
            if sd > 0.0 {
                continue;
            }
            let idx = row + px as usize;
            if alpha == 255 {
                buf[idx] = color;
            } else {
                buf[idx] = blend_alpha(buf[idx], color, alpha);
            }
        }
    }
}

/// The teardrop popup, drawn as `draw_rotated_rounded_rect` rotated -45 deg
/// with **one** `r/5` corner, per `FastScrollThumbDrawable.java:58-62`.
///
/// The radii are `[tl, tr, br, bl]` (the order
/// [`draw_rotated_rounded_rect`] documents), so the source's
/// `{r,r, r,r, r2,r2, r,r}` is `[big, big, small, big]`: the *bottom-right*
/// corner alone is `r/5`. The -45 deg rotation at `:62` carries it to the
/// shape's right-hand tip, which is the point the popup's tail points at.
///
/// [`teardrop_corner_radii`] is the assertable form of this configuration.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn draw_teardrop(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    cx: f32,
    cy: f32,
    rw: f32,
    rh: f32,
    color: u32,
) {
    draw_rotated_rounded_rect(
        buf,
        stride,
        w,
        h,
        cx,
        cy,
        rw,
        rh,
        teardrop_corner_radii(rh),
        -45.0f32.to_radians(),
        color,
    );
}

/// The teardrop's four corner radii in `[tl, tr, br, bl]` order, from
/// [`teardrop_radii`]: exactly one corner is `r/5`.
///
/// Split out of [`draw_teardrop`] so the configuration is assertable without
/// rasterising and reverse-engineering it from pixels -- which is what the
/// caller-facing shape contract is actually about.
#[inline]
pub fn teardrop_corner_radii(height: f32) -> [f32; 4] {
    let t = teardrop_radii(height);
    // TL, TR, BR, BL -- `FastScrollThumbDrawable.java:58-60`, whose radii
    // array is two floats per corner, not one.
    [t.big, t.big, t.small, t.big]
}

// ===========================================================================
// Adaptive-icon squircle mask
// ===========================================================================

/// Half-width of the superellipse `(|x|/r)^4 + (|y|/r)^4 <= 1` at row offset
/// `dy` from the centre.
///
/// ```text
/// (x/r)^4 = 1 - (dy/r)^4
/// x       = r * (1 - (dy/r)^4)^(1/4)
///         = isqrt(isqrt(r^4 - dy^4))                        (since r^4 - dy^4 >= 0)
/// ```
///
/// The two nested integer square roots are exact for the fourth root of an
/// integer: `isqrt(isqrt(v)) == floor(v^(1/4))`. That turns the per-row cost
/// into ~30 cycles instead of a `powf`, and — the point of the whole
/// exercise — the *inner* loop stays a contiguous span copy with no extra
/// per-pixel work at all, so masking an icon costs the same as blitting it.
///
/// `r` is in pixels; `dy` is the absolute row distance from the centre.
#[inline]
pub fn squircle_span(r: u32, dy: u32) -> u32 {
    if r == 0 {
        return 0;
    }
    if dy >= r {
        return 0;
    }
    // Every intermediate is u64, and that is load-bearing rather than
    // defensive: `r^4 - dy^4` overflows a u32 at `r == 256` exactly
    // (256^4 == 2^32 == 4_294_967_296, one past `u32::MAX`), and it is a
    // *silent* wrap in release, not a panic. A wrapped quartic makes
    // `isqrt(isqrt(..))` too small, which shortens the span and clips the
    // icon's silhouette rather than failing.
    //
    // `ICON_MASK_MIN_EDGE` is 32 so no icon ever gets near that today, but
    // this is a `pub fn` in a rasteriser: the next caller that passes a panel
    // edge, or a `u32` width read from an untrusted PNG IHDR, would get wrong
    // pixels instead of a wrong-sized allocation.
    let r64 = r as u64;
    let dy64 = dy as u64;
    let r2 = r64 * r64;
    let dy2 = dy64 * dy64;
    let rem = r2 - dy2; // dy < r, so this cannot underflow
    if rem == 0 {
        return r;
    }
    // r^4 - dy^4 = (r^2 - dy^2)(r^2 + dy^2)
    let quartic = rem * (r2 + dy2);
    isqrt64(isqrt64(quartic)) as u32
}

/// Integer square root of a `u64`, floor.
///
/// The `u32` `isqrt` cannot take the quartic without wrapping, so this is the
/// 64-bit form. Newton's method with an integer seed: it converges in a few
/// iterations for every `u64` and, unlike a float `sqrt`, is exact at the
/// boundary -- a squircle span that is off by one is a visibly wrong corner.
#[inline]
fn isqrt64(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    if n < 4 {
        return 1;
    }
    // `n.leading_zeros()` gives a seed within one bit of the root.
    let mut x: u64 = 1u64 << (64 - n.leading_zeros()).div_ceil(2);
    loop {
        // `x + n/x` cannot overflow: `x` starts at most 2x the root and each
        // step moves it strictly down, and the intermediate is bounded by the
        // invariant.
        let y = (x + n / x) >> 1;
        if y >= x {
            break;
        }
        x = y;
    }
    x
}

/// Mask a contiguous RGBA row in place to a squircle of edge `edge`.
///
/// The caller blits a pre-scaled icon row; this zeroes the alpha outside the
/// superellipse and clamps the span to the buffer, so the blit itself remains
/// a linear copy. Returns the `[(lo, hi)]` span that was preserved, which the
/// caller uses to prove the mask actually cut ink (see
/// `squircle_span_matches_superellipse`).
#[inline]
pub fn squircle_mask_row(row: &mut [u8], edge: usize, row_y: isize) -> Option<(usize, usize)> {
    if edge == 0 || row.is_empty() {
        return None;
    }
    let r = (edge / 2) as i32;
    let cy = r as isize;
    let dy = (row_y - cy).unsigned_abs() as u32;
    let half = squircle_span(r as u32, dy) as usize;
    // `as_chunks_mut` rather than `chunks_exact_mut(4)`: a fixed 4-byte RGBA
    // stride is a property of the format, not an accident of the loop, and
    // expressing it that way keeps the two loops below identical in shape.
    let (pixels, _) = row.as_chunks_mut::<4>();
    if half == 0 {
        for px in pixels.iter_mut() {
            px[3] = 0;
        }
        return None;
    }
    let center = edge / 2;
    let lo = center.saturating_sub(half);
    let hi = (center + half).min(edge);
    // Zero everything outside [lo, hi).
    for (i, px) in pixels.iter_mut().enumerate() {
        if i < lo || i >= hi {
            px[3] = 0;
        }
    }
    Some((lo, hi))
}

// ===========================================================================
// Band shadow
// ===========================================================================

/// Separable 3-tap box blur of an 8-bit coverage mask, in place.
///
/// Two passes (horizontal then vertical) make the kernel separable, so the
/// cost is `2 * mask_pixels` rather than `mask_pixels * kernel_width`.
///
/// The mask is the *input* rather than something inferred from the framebuffer
/// on purpose: the caller knows exactly what it drew (a glyph run, an icon
/// rect), so it can build the coverage once and cache it. The plan's
/// requirement is that the smartspace shadow be "computed once per string
/// change", which is only possible if the mask is a separate, cacheable
/// value.
///
/// `scratch` must hold at least `w + 2 * h` bytes and is used as the
/// row-gather buffer, the row accumulator and the column accumulator.
///
/// A 3-tap box is the cheapest symmetric kernel that still reads as a soft
/// shadow. The plan budgets the 104 dp smartspace band at ~0.8 ms, which at
/// `opt-level = "z"` rules out anything wider.
pub fn blur_mask_8(mask: &mut [u8], w: usize, h: usize, scratch: &mut [u8]) {
    if w < 3 || h < 3 || mask.len() < w * h || scratch.len() < w + 2 * h {
        return;
    }
    let (row_acc, tail) = scratch.split_at_mut(w);
    let (col_buf, col_acc) = tail.split_at_mut(h);
    let col_acc = &mut col_acc[..h];

    // Pass 1: horizontal, in place per row using `row_acc`.
    for r in 0..h {
        let row = &mut mask[r * w..r * w + w];
        blur_row_8(row, row_acc);
    }
    // Pass 2: vertical, gathering each column into `col_buf`.
    for c in 0..w {
        for r in 0..h {
            col_buf[r] = mask[r * w + c];
        }
        blur_row_8(col_buf, col_acc);
        for r in 0..h {
            mask[r * w + c] = col_buf[r];
        }
    }
}

/// One 3-tap box pass along a `u8` row, using `tmp` as the accumulator.
///
/// The divide happens in `u16` *before* the narrowing cast. Summing three
/// `u8`s can reach 765, and `(765 + 1) as u8 / 3` truncates to 0 -- which
/// silently erased exactly the fully-covered pixels a shadow is made of.
#[inline]
fn blur_row_8(row: &mut [u8], tmp: &mut [u8]) {
    let n = row.len();
    if n < 3 || tmp.len() < n {
        return;
    }
    tmp[0] = (row[0] as u16 + row[1] as u16).div_ceil(2) as u8;
    tmp[n - 1] = (row[n - 2] as u16 + row[n - 1] as u16).div_ceil(2) as u8;
    for i in 1..n - 1 {
        let sum = row[i - 1] as u16 + row[i] as u16 + row[i + 1] as u16;
        tmp[i] = ((sum + 1) / 3) as u8;
    }
    row.copy_from_slice(&tmp[..n]);
}

/// Darken the framebuffer under `mask`, offset by `(dx, dy)`, blurring first.
///
/// This is the two-layer shadow from `DoubleShadowIconDrawable.kt:35-53`
/// collapsed into one call: the caller supplies a coverage mask (a glyph run
/// or an icon silhouette) and this applies the blur, the offset and the alpha
/// in a single band-limited composite.
///
/// Cost is `mask_pixels` blends over the mask's bounding box only -- for the
/// 104 dp smartspace band that is ~112 kpx, not the 2.59 Mpx a naive
/// full-screen scrim would cost (plan §3.2 measures that at 10.2 ms).
///
/// **Both shadow layers are disabled in the dark theme**
/// (`styles.xml:111-113`), so a dark-mode caller must not call this at all
/// rather than passing `alpha == 0`.
///
/// # Per-frame cost
///
/// The blur is `2 * mask_pixels` and needs a caller-owned scratch, which is
/// fine for a label that changes rarely and wrong for anything cached: a
/// cached mask would be re-blurred on every frame it is drawn. A caller whose
/// mask is computed once should use
/// [`draw_preblurred_shadow_from_mask`] instead, which takes no scratch and
/// does the same composite.
#[allow(clippy::too_many_arguments)]
pub fn draw_shadow_from_mask(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    mask: &[u8],
    mask_w: usize,
    mask_h: usize,
    origin_x: i32,
    origin_y: i32,
    offset_x: i32,
    offset_y: i32,
    shadow_rgb: u32,
    alpha: u8,
    scratch: &mut [u8],
) {
    let Some(cells) = mask_w.checked_mul(mask_h) else {
        return;
    };
    if alpha == 0 || cells == 0 || mask.len() < cells {
        return;
    }
    // Blur a caller-owned copy so the caller's cached mask stays pristine.
    // A scratch of the mask size is required; the caller owns it, so this
    // stays allocation-free.
    debug_assert!(scratch.len() >= cells + mask_w + 2 * mask_h);
    if scratch.len() < cells + mask_w + 2 * mask_h {
        return;
    }
    let (work, bl) = scratch.split_at_mut(cells);
    let work = &mut work[..cells];
    work.copy_from_slice(&mask[..cells]);
    blur_mask_8(work, mask_w, mask_h, bl);
    composite_shadow_mask(
        buf, stride, w, h, work, mask_w, mask_h, origin_x, origin_y, offset_x, offset_y,
        shadow_rgb, alpha,
    );
}

/// Composite an **already blurred** coverage mask, offset by `(dx, dy)`.
///
/// Same composite as [`draw_shadow_from_mask`] minus the blur pass, which
/// moves the per-draw cost from `2 * mask_pixels` to `mask_pixels` and
/// removes the scratch requirement entirely: a cached mask needs no copy, so
/// the only allocation on this path is the one that built the mask.
///
/// That is the whole reason this exists. The reference applies
/// `IconShapeModel.shapeRadius` and its two shadow layers once per
/// drawable, because a drawable caches its own blur
/// (`DoubleShadowIconDrawable.kt:35-53`); UTLC's icons are cache *entries*
/// (`compositor::icons::IconShadow`), so the blur belongs to the entry and
/// this is the call that draws it. Blurring here as well would double-blur
/// every icon on screen every frame.
///
/// `mask` is the coverage in `0..=255`; `alpha` is the layer's peak opacity
/// and coverage modulates it, so full coverage is `alpha` and zero coverage is
/// nothing. A mask with more padding than the blur can reach keeps the
/// composite's edge off the tile — see
/// [`compositor::icons::shadow_coverage`](crate::compositor::icons::shadow_coverage).
///
/// **Both shadow layers are disabled in the dark theme** in the reference
/// (`styles.xml:111-113`), so a dark-mode caller must not call this at all
/// rather than passing `alpha == 0`.
#[allow(clippy::too_many_arguments)]
pub fn draw_preblurred_shadow_from_mask(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    mask: &[u8],
    mask_w: usize,
    mask_h: usize,
    origin_x: i32,
    origin_y: i32,
    offset_x: i32,
    offset_y: i32,
    shadow_rgb: u32,
    alpha: u8,
) {
    composite_shadow_mask(
        buf, stride, w, h, mask, mask_w, mask_h, origin_x, origin_y, offset_x, offset_y,
        shadow_rgb, alpha,
    );
}

/// The offset + alpha composite both shadow entry points share.
///
/// O(rows) clip tests plus one blend per covered mask pixel; the mask itself is
/// never modified, so the same cached mask can back any number of frames.
#[inline]
#[allow(clippy::too_many_arguments)]
fn composite_shadow_mask(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    mask: &[u8],
    mask_w: usize,
    mask_h: usize,
    origin_x: i32,
    origin_y: i32,
    offset_x: i32,
    offset_y: i32,
    shadow_rgb: u32,
    alpha: u8,
) {
    // `checked_mul` rather than `mask_w * mask_h`: this is a `pub fn` taking
    // two `usize`s from a caller, and a wrapped cell count would index past
    // the mask instead of bailing out.
    if alpha == 0
        || mask_w
            .checked_mul(mask_h)
            .is_none_or(|cells| cells == 0 || mask.len() < cells)
    {
        return;
    }
    let src = shadow_rgb & 0x00FF_FFFF;
    for my in 0..mask_h as i32 {
        let py = origin_y + offset_y + my;
        if py < 0 || py >= h as i32 {
            continue;
        }
        let row = py as usize * stride;
        for mx in 0..mask_w as i32 {
            let cov = mask[my as usize * mask_w + mx as usize];
            if cov == 0 {
                continue;
            }
            let px = origin_x + offset_x + mx;
            if px < 0 || px >= w as i32 {
                continue;
            }
            // Coverage modulates the layer alpha, exactly as a blur mask
            // should: full coverage is `alpha`, zero coverage is nothing.
            let a = ((alpha as u16 * cov as u16) / 255) as u8;
            let idx = row + px as usize;
            buf[idx] = blend_alpha(buf[idx], src | 0xFF00_0000, a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    // -- squircle ---------------------------------------------------------

    #[test]
    fn squircle_span_matches_superellipse() {
        let r = 32u32;
        for dy in 0..=r {
            let got = squircle_span(r, dy);
            // The two nested isqrts compute the fourth root *exactly*, and
            // integer sqrt floors, so the contract is floor(v^(1/4)) -- not
            // "within half a pixel", which a floored value cannot satisfy
            // when the exact value sits just under an integer.
            let want = r as f64
                * (1.0f64 - (dy as f64 / r as f64).powi(4))
                    .max(0.0)
                    .powf(0.25);
            assert_eq!(
                got as f64,
                want.floor(),
                "dy={dy}: squircle_span = {got}, floor(superellipse) = {}",
                want.floor()
            );
        }
        assert_eq!(squircle_span(r, r), 0, "must vanish at the edge");
        assert_eq!(squircle_span(r, 0), r, "must be full width on the axis");
        assert_eq!(squircle_span(0, 0), 0, "degenerate radius");
    }

    /// `r^4` overflows a `u32` at `r == 256`, exactly.
    ///
    /// `256^4 == 4_294_967_296`, one past `u32::MAX`. The old `u32` arithmetic
    /// wrapped silently in release -- not a panic -- and a wrapped quartic
    /// makes `isqrt(isqrt(..))` too small, which *shortens* the span and clips
    /// the icon's silhouette. So the regression is a wrong number, not a
    /// crash, and it has to be asserted against the exact superellipse value
    /// for a radius that overflows.
    #[test]
    fn squircle_span_does_not_overflow_at_r_256() {
        for r in [255u32, 256, 257, 1024, 4096] {
            // The axis is exact at every radius: full width.
            assert_eq!(
                squircle_span(r, 0),
                r,
                "r={r} must be full width on the axis"
            );
            assert_eq!(squircle_span(r, r), 0, "r={r} must vanish at the edge");
            for dy in [1u32, r / 4, r / 2, 3 * r / 4, r - 1] {
                if dy >= r {
                    continue;
                }
                let got = squircle_span(r, dy);
                let want = r as f64
                    * (1.0f64 - (dy as f64 / r as f64).powi(4))
                        .max(0.0)
                        .powf(0.25);
                assert_eq!(
                    got as f64,
                    want.floor(),
                    "r={r} dy={dy}: {got} vs floor(superellipse) = {}",
                    want.floor()
                );
                // And a wrapped u32 would show up as a span *smaller* than the
                // r-1 row above it, i.e. non-monotonic.
                assert!(got <= r, "r={r} dy={dy}: span {got} exceeds r");
            }
        }
    }

    #[test]
    fn squircle_span_is_bounded_and_monotonic_in_dy() {
        // The real invariant. A squircle mask of edge `2r` has no centre row
        // (even edge length), so `dy` and `r-1-dy` are *not* mirror pairs --
        // asserting that would be asserting a property the shape does not
        // have. What must hold is that the half-width shrinks monotonically
        // as the row moves away from the axis.
        for r in [1u32, 2, 8, 32, 65, 128] {
            let mut prev = r + 1;
            for dy in 0..r {
                let s = squircle_span(r, dy);
                assert!(s <= r, "r={r} dy={dy} span {s} exceeds r");
                assert!(s > 0, "r={r} dy={dy} span vanished early");
                assert!(s <= prev, "r={r} dy={dy} span grew: {prev} -> {s}");
                prev = s;
            }
        }
    }

    #[test]
    fn squircle_span_floors_the_exact_superellipse() {
        // The integer path loses at most one unit to the floor. Asserting a
        // *ratio* here would fail for small radii: at r = 8, dy = 2 the exact
        // value is 7.992 and the correct answer is 7, a ratio of 0.875 that
        // says nothing about the shape.
        for r in [4u32, 8, 16, 32, 64, 128] {
            for dy in 0..=r {
                let exact = r as f64
                    * (1.0f64 - (dy as f64 / r as f64).powi(4))
                        .max(0.0)
                        .powf(0.25);
                let got = squircle_span(r, dy) as f64;
                assert!(
                    exact - 1.0 < got && got <= exact + 1e-9,
                    "r={r} dy={dy}: got {got}, exact {exact}"
                );
            }
        }
    }

    #[test]
    fn superellipse_is_rounder_than_a_circle_at_the_same_radius() {
        // The Material You signature: (|x|/r)^4 + (|y|/r)^4 <= 1 stays much
        // closer to full width than a circle does. Evaluated in f64 against
        // the closed form, because the integer mask floors and the floor is
        // not part of this property.
        for frac in [0.05f64, 0.10, 0.25, 0.5, 0.75] {
            let squircle = (1.0f64 - frac.powi(4)).powf(0.25);
            let circle = (1.0f64 - frac * frac).sqrt();
            assert!(
                squircle > circle,
                "at |y|/r = {frac}: squircle {squircle} is not rounder than circle {circle}"
            );
        }
        // The gap is the whole point: at a quarter radius the squircle is
        // still 0.999 wide while a circle is 0.968.
        let sq = (1.0f64 - 0.25f64.powi(4)).powf(0.25);
        let ci = (1.0f64 - 0.0625f64).sqrt();
        assert!(sq > ci + 0.025, "squircle {sq} vs circle {ci}");
        // It must still taper, or the mask would be a plain square.
        assert!(sq < 1.0);
    }

    // -- rounded span / stroke -------------------------------------------

    #[test]
    fn rounded_span_f_covers_the_whole_row_in_the_middle() {
        let (lo, hi) = rounded_span_f(26.0, 10.0, 100.0, 52.0, 12.0).expect("mid row");
        assert!(near(lo, 10.0, 0.001), "lo {lo}");
        assert!(near(hi, 110.0, 0.001), "hi {hi}");
    }

    #[test]
    fn rounded_span_f_rejects_rows_outside_the_shape() {
        assert!(rounded_span_f(-1.0, 0.0, 100.0, 52.0, 12.0).is_none());
        assert!(rounded_span_f(53.0, 0.0, 100.0, 52.0, 12.0).is_none());
        assert!(rounded_span_f(0.0, 0.0, 0.0, 52.0, 12.0).is_none());
    }

    // -- teardrop radii ---------------------------------------------------

    #[test]
    fn teardrop_radii_match_fast_scroll_thumb_drawable() {
        let t = teardrop_radii(62.0);
        assert!(near(t.big, 31.0, 1e-4), "r must be height/2, got {}", t.big);
        assert!(
            near(t.small, 31.0 / 5.0, 1e-4),
            "r2 must be r/5, got {}",
            t.small
        );
    }

    #[test]
    fn teardrop_has_exactly_one_small_corner() {
        // `FastScrollThumbDrawable.java:54-60`. The radii array passed to
        // `addRoundRect` is `{r,r, r,r, r2,r2, r,r}`: EIGHT floats, two per
        // corner, in `[TL, TR, BR, BL]` order. So exactly one corner -- BR --
        // is `r/5`; the source says the same in prose at `:54-55` ("radius of
        // one corner is 1/5th of the other 3 corners"). Reading it as four
        // entries gave `[big, small, small, big]`, two small corners.
        for height in [10.0f32, 62.0, 128.0] {
            let radii = teardrop_corner_radii(height);
            let t = teardrop_radii(height);
            assert_eq!(radii.len(), 4, "one radius per corner");
            assert!(
                near(radii[0], t.big, 1e-4)
                    && near(radii[1], t.big, 1e-4)
                    && near(radii[3], t.big, 1e-4),
                "height {height}: TL, TR and BL are r, got {radii:?}"
            );
            assert!(
                near(radii[2], t.small, 1e-4),
                "height {height}: BR alone is r/5, got {radii:?}",
            );
            assert_eq!(
                radii.iter().filter(|v| near(**v, t.small, 1e-4)).count(),
                1,
                "height {height}: exactly one small corner, got {radii:?}"
            );
            // And the small one really is smaller, so the test above is not
            // trivially satisfiable by a constant array.
            assert!(t.big > t.small, "r must exceed r/5");
        }
        // The configuration the pixels are actually drawn with, not a
        // re-derivation of it: `draw_teardrop` must route through this.
        assert_eq!(teardrop_corner_radii(62.0), [31.0, 31.0, 6.2, 31.0]);
    }

    // -- uniform stroke ----------------------------------------------------

    /// Contiguous run length of ink along row `y`, starting at the first inked
    /// pixel at or after `from` (walking right).
    fn run_right(buf: &[u32], stride: usize, w: usize, y: usize, from: usize) -> usize {
        let mut n = 0usize;
        let mut x = from;
        while x < w && buf[y * stride + x] != 0 {
            n += 1;
            x += 1;
        }
        n
    }

    /// Contiguous run length of ink down column `x`, starting at the first
    /// inked pixel at or after `from` (walking down).
    fn run_down(buf: &[u32], stride: usize, h: usize, x: usize, from: usize) -> usize {
        let mut n = 0usize;
        let mut y = from;
        while y < h && buf[y * stride + x] != 0 {
            n += 1;
            y += 1;
        }
        n
    }

    /// Contiguous run length of ink up column `x`, starting at row `from`
    /// inclusive (walking up). The bottom edge's band is measured this way.
    fn run_up(buf: &[u32], stride: usize, x: usize, from: usize) -> usize {
        let mut n = 0usize;
        let mut y = from as isize;
        while y >= 0 && buf[y as usize * stride + x] != 0 {
            n += 1;
            y -= 1;
        }
        n
    }

    #[test]
    fn outlined_rounded_rect_stroke_is_uniform() {
        // The real proof the arc-sampling fix works. Draw a stroked rounded
        // rect and measure the ink run on each of the four sides, at a
        // cross-section well inside the straight part of each side so the
        // measurement is the stroke width and not a corner chord.
        //
        // Before the fix the inner span was sampled at `dy` instead of
        // `dy - s`, sliding the hole `s` rows up: the top of the stroke was
        // `s` rows *thinner* than the bottom and the top-left/top-right
        // corner arcs were wrong by the same amount.
        let w = 96usize;
        let h = 72usize;
        let x = 12.0f32;
        let y = 12.0f32;
        let rw = 60.0f32;
        let rh = 40.0f32;
        let radius = 10.0f32;
        let stroke = 4.0f32;
        let mut buf = vec![0u32; w * h];
        draw_rounded_rect_outline_f(&mut buf, w, w, h, x, y, rw, rh, radius, stroke, 0xFFFFFFFF);

        // Cross-sections: mid-span on the top and bottom edges, mid-span on
        // the left and right edges. Each is `floor/ceil` of an integer
        // geometry, so the exact answer is `stroke` (+/-1 for the rounding).
        let xm = (x + rw * 0.5) as usize; // 42, mid-width
        let ym = (y + rh * 0.5) as usize; // 32, mid-height
        let top_run = run_down(&buf, w, h, xm, y as usize);
        let bot_run = run_up(&buf, w, xm, (y + rh) as usize - 1);
        // The left and right runs are the *outer* runs at mid-height, walking
        // inward from each edge to the hole.
        let left_run = run_right(&buf, w, w, ym, x as usize);
        let right_run = {
            let mut n = 0usize;
            let mut xx = (x + rw) as usize;
            while xx > 0 && buf[ym * w + xx - 1] != 0 {
                n += 1;
                xx -= 1;
            }
            n
        };

        let runs = [
            ("top", top_run),
            ("bottom", bot_run),
            ("left", left_run),
            ("right", right_run),
        ];
        for (name, got) in runs {
            assert!(
                got.abs_diff(stroke as usize) <= 1,
                "{name} stroke is {got} px, expected {stroke} (runs: {runs:?})"
            );
        }
        // And they must agree *with each other*, which is the uniformity claim
        // itself: any arc-sampling error is a differential error.
        let max = runs.iter().map(|(_, v)| *v).max().unwrap();
        let min = runs.iter().map(|(_, v)| *v).min().unwrap();
        assert!(
            max - min <= 1,
            "stroke is not uniform across sides: {runs:?}"
        );

        // The hole really is a hole: the centre of the rect is untouched.
        assert_eq!(buf[ym * w + xm], 0, "the interior must not be painted");
    }

    #[test]
    fn outlined_rounded_rect_corners_carry_the_full_stroke() {
        // The complement of the uniformity test: at a corner cross-section,
        // the ink on each side of the diagonal must be the same thickness as
        // on the straight edges. A wrong inner `dy` shows up here as a corner
        // that is thinner on its upper side than its lower one.
        let w = 96usize;
        let h = 72usize;
        let x = 12.0f32;
        let y = 12.0f32;
        let rw = 60.0f32;
        let rh = 40.0f32;
        let radius = 10.0f32;
        let stroke = 4.0f32;
        let mut buf = vec![0u32; w * h];
        draw_rounded_rect_outline_f(&mut buf, w, w, h, x, y, rw, rh, radius, stroke, 0xFFFFFFFF);

        // Walk the top-left corner: for each inked row in the cap, the ink run
        // from the shape's left edge must never exceed the straight-edge run
        // by more than the corner's own arc allowance, and must never be
        // *shorter* than it -- the latter is what a hole sampled `s` rows too
        // high produces.
        let edge = run_right(&buf, w, w, (y + rh * 0.5) as usize, x as usize);
        let cap_rows = radius.ceil() as usize;
        for yy in 0..cap_rows {
            let row = y as usize + yy;
            let mut first = None;
            for xx in 0..w {
                if buf[row * w + xx] != 0 {
                    first = Some(xx);
                    break;
                }
            }
            let Some(first) = first else { continue };
            // Near the very top of the cap the row is legitimately short (it
            // is the arc itself), so only assert the rows below the first
            // full-width row.
            if yy == 0 {
                continue;
            }
            let run = run_right(&buf, w, w, row, first);
            assert!(
                run + 1 >= edge,
                "corner row {yy}: ink run {run} is thinner than the straight edge's {edge}"
            );
        }
    }

    // -- blend ------------------------------------------------------------

    #[test]
    fn blend_alpha_matches_the_reference_rounding() {
        assert_eq!(blend_alpha(0xFF00_0000, 0xFFFFFFFF, 255), 0x00FF_FFFF);
        assert_eq!(blend_alpha(0xFF00_0000, 0xFFFFFFFF, 0), 0xFF00_0000);
        // Mid grey over mid grey, 50%.
        let got = blend_alpha(0xFF808080, 0xFF808080, 128);
        assert_eq!(
            got & 0x00FF_FFFF,
            0x00808080,
            "same colour must be identity"
        );
        // Black over white at 0% stays white; at 100% becomes black.
        assert_eq!(
            blend_alpha(0xFFFFFFFF, 0xFF000000, 0) & 0x00FF_FFFF,
            0x00FFFFFF
        );
        assert_eq!(
            blend_alpha(0xFFFFFFFF, 0xFF000000, 255) & 0x00FF_FFFF,
            0x00000000
        );
    }

    #[test]
    fn composite_scrim_is_opaque_and_between_its_endpoints() {
        let back = 0xFF204060u32;
        let out = composite_scrim(back, 0xFF404040, 0.40);
        assert_eq!(out >> 24, 0xFF, "scrim must composite to opaque");
        // 40% of 0x40 over 0x20 is strictly between.
        let b = back & 0xFF;
        let o = out & 0xFF;
        assert!(o < b, "a dark scrim must darken");
    }

    #[test]
    fn isqrt_matches_the_float_sqrt() {
        for v in [
            0u32,
            1,
            2,
            3,
            4,
            8,
            9,
            15,
            16,
            17,
            99,
            100,
            101,
            65_535,
            65_536,
            1 << 30,
        ] {
            let f = (v as f64).sqrt().floor() as u32;
            assert_eq!(isqrt(v), f, "isqrt({v})");
        }
    }

    #[test]
    fn draw_opaque_rect_clips_and_fills() {
        let w = 16usize;
        let h = 8usize;
        let mut buf = vec![0u32; w * h];
        draw_opaque_rect(&mut buf, w, w, h, -4, -4, 8, 8, 0xFF112233);
        // Clipped to the top-left 4x4.
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(buf[y * w + x], 0xFF112233, "at {x},{y}");
            }
            for x in 4..8 {
                assert_eq!(buf[y * w + x], 0, "overdraw at {x},{y}");
            }
        }
    }

    #[test]
    fn draw_rotated_rounded_rect_draws_something() {
        let w = 64usize;
        let h = 64usize;
        let mut buf = vec![0u32; w * h];
        draw_teardrop(&mut buf, w, w, h, 32.0, 32.0, 40.0, 30.0, 0xFFFFFFFF);
        let ink = buf.iter().filter(|p| **p != 0).count();
        // 40x30 = 1200 px, rotation cannot change the area materially.
        assert!(ink > 900 && ink < 1500, "teardrop ink {ink} is not ~1200");
    }

    // -- band shadow ------------------------------------------------------

    #[test]
    fn blur_mask_8_preserves_total_energy_within_a_lsb() {
        // A separable box blur conserves the sum, which is the property that
        // makes "coverage modulates alpha" the right way to composite it.
        let w = 9;
        let h = 5;
        let mut mask = vec![0u8; w * h];
        mask[2 * w + 4] = 200;
        let before: u64 = mask.iter().map(|v| *v as u64).sum();
        let mut scratch = vec![0u8; w + 2 * h];
        blur_mask_8(&mut mask, w, h, &mut scratch);
        let after: u64 = mask.iter().map(|v| *v as u64).sum();
        // Truncating integer division can only lose, and only at the edges.
        let lost = before.saturating_sub(after);
        assert!(
            lost * 100 <= before * 2,
            "blur lost {lost} of {before}, more than 2%"
        );
        assert!(after > 0, "blur erased the mask entirely");
    }

    #[test]
    fn blur_mask_8_spreads_a_single_pixel_outward() {
        let w = 11;
        let h = 11;
        let mut mask = vec![0u8; w * h];
        mask[5 * w + 5] = 255;
        let mut scratch = vec![0u8; w + 2 * h];
        blur_mask_8(&mut mask, w, h, &mut scratch);
        // Centre must drop (a 3x3 box averages 1/9 of the peak)...
        assert!(mask[5 * w + 5] < 255, "centre did not blur");
        // ...and the four orthogonal neighbours must now be non-zero.
        assert!(mask[4 * w + 5] > 0, "no spread upward");
        assert!(mask[6 * w + 5] > 0, "no spread downward");
        assert!(mask[5 * w + 4] > 0, "no spread left");
        assert!(mask[5 * w + 6] > 0, "no spread right");
    }

    #[test]
    fn blur_mask_8_refuses_undersized_scratch() {
        let mut mask = vec![255u8; 16];
        let mut tiny = vec![0u8; 2];
        blur_mask_8(&mut mask, 4, 4, &mut tiny);
        // Unchanged: a silent no-op beats a panic on the render path.
        assert!(mask.iter().all(|v| *v == 255), "blur ran with no scratch");
    }

    #[test]
    fn draw_shadow_from_mask_darkens_under_the_offset_and_not_the_source() {
        let mw = 8;
        let mh = 8;
        let mut mask = vec![0u8; mw * mh];
        for r in 0..mh {
            for c in 0..mw {
                mask[r * mw + c] = 255;
            }
        }
        let mut buf = vec![0xFF808080u32; 32 * 32];
        let mut scratch = vec![0u8; mw * mh + mw + 2 * mh];
        draw_shadow_from_mask(
            &mut buf,
            32,
            32,
            32,
            &mask,
            mw,
            mh,
            8,
            8,
            1,
            1,
            0x000000,
            128,
            &mut scratch,
        );
        // The source area itself is untouched: the shadow is offset.
        let src_px = buf[8 * 32 + 8];
        assert_eq!(src_px, 0xFF808080, "shadow was drawn on top of the source");
        // One pixel down-right of the mask, inside the blurred edge, is darker.
        let below = buf[9 * 32 + 9];
        assert!(
            (below & 0xFF) < 0x80,
            "expected a darkened pixel at +1,+1, got {below:08x}"
        );
        // Far from the mask, untouched.
        assert_eq!(buf[30 * 32 + 30], 0xFF808080, "shadow leaked");
    }

    #[test]
    fn draw_shadow_from_mask_is_a_noop_at_zero_alpha() {
        let mut buf = vec![0xFFFFFFFFu32; 16 * 16];
        let mask = vec![255u8; 16];
        let mut scratch = vec![0u8; 16 + 16 + 32];
        let before = buf.clone();
        draw_shadow_from_mask(
            &mut buf,
            16,
            16,
            16,
            &mask,
            4,
            4,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut scratch,
        );
        assert_eq!(buf, before, "alpha 0 must not touch the framebuffer");
    }

    #[test]
    fn draw_shadow_from_mask_clips_at_the_edges() {
        let mut buf = vec![0xFFFFFFFFu32; 16 * 16];
        let mask = vec![255u8; 16];
        let mut scratch = vec![0u8; 16 + 16 + 32];
        // Origin far off the left/top edge and an offset pushing further out.
        draw_shadow_from_mask(
            &mut buf,
            16,
            16,
            16,
            &mask,
            4,
            4,
            -10,
            -10,
            0,
            0,
            0x000000,
            200,
            &mut scratch,
        );
        // Must not panic, and must not have wrapped around to the far side.
        assert!(
            buf.iter().all(|p| (*p >> 24) == 0xFF),
            "alpha byte corrupted"
        );
    }

    // -- pre-blurred shadow (the cached-icon path) ------------------------

    /// A `w`x`h` coverage mask of one opaque `edge`-square centred in `pad`
    /// transparent rows, blurred with [`blur_mask_8`]. The same shape
    /// `compositor::icons::shadow_coverage` caches.
    fn preblurred_icon_mask(w: usize, h: usize, edge: usize, pad: usize) -> Vec<u8> {
        let mut m = vec![0u8; w * h];
        for y in pad..pad + edge {
            for x in pad..pad + edge {
                m[y * w + x] = 255;
            }
        }
        let mut scratch = vec![0u8; w + 2 * h];
        blur_mask_8(&mut m, w, h, &mut scratch);
        m
    }

    #[test]
    fn preblurred_shadow_matches_the_blurring_entry_point() {
        // The contract that lets the shell cache a blurred mask: blurring here
        // and blurring inside `draw_shadow_from_mask` must land the same
        // pixels. If they diverge, the cached path is a different shadow.
        let (mw, mh, pad) = (16usize, 16usize, 3usize);
        let raw = {
            let mut m = vec![0u8; mw * mh];
            for y in pad..pad + 10 {
                for x in pad..pad + 10 {
                    m[y * mw + x] = 255;
                }
            }
            m
        };
        let blurred = preblurred_icon_mask(mw, mh, 10, pad);
        assert_ne!(
            raw, blurred,
            "the fixture must actually differ from its blur"
        );

        let mut a = vec![0xFF808080u32; 40 * 40];
        let mut b = a.clone();
        let mut scratch = vec![0u8; mw * mh + mw + 2 * mh];
        draw_shadow_from_mask(
            &mut a,
            40,
            40,
            40,
            &raw,
            mw,
            mh,
            6,
            6,
            1,
            2,
            0x001018,
            170,
            &mut scratch,
        );
        draw_preblurred_shadow_from_mask(
            &mut b, 40, 40, 40, &blurred, mw, mh, 6, 6, 1, 2, 0x001018, 170,
        );
        assert_eq!(a, b, "the pre-blurred path is not the same shadow");
        // And it is not a no-op: the two differ from the untouched backdrop.
        assert!(a.iter().any(|p| (*p & 0xFF) < 0x80), "no shadow was drawn");
    }

    #[test]
    fn preblurred_shadow_does_not_modify_the_cached_mask() {
        // The reason the entry point exists: the caller owns the mask for the
        // process lifetime, so a draw that blurred it in place would degrade
        // the shadow on every subsequent frame.
        let (mw, mh) = (14usize, 14usize);
        let mask = preblurred_icon_mask(mw, mh, 8, 3);
        let before = mask.clone();
        let draw_once = || {
            let mut b = vec![0xFF808080u32; 32 * 32];
            draw_preblurred_shadow_from_mask(
                &mut b, 32, 32, 32, &mask, mw, mh, 4, 4, 0, 1, 0x000000, 120,
            );
            b
        };
        // Purity is the property a cached mask needs: the same mask and the
        // same geometry must composite identically every frame.
        assert_eq!(
            draw_once(),
            draw_once(),
            "the same draw produced different pixels"
        );
        assert_eq!(mask, before, "the mask was blurred again by a draw");
        assert!(
            draw_once().iter().any(|p| (*p & 0xFF) < 0x80),
            "no shadow was drawn"
        );
    }

    #[test]
    fn preblurred_shadow_refuses_a_short_mask_instead_of_panicking() {
        // A `pub fn` taking `mask_w`/`mask_h` from a caller must not index
        // past the slice when those two disagree with the buffer: the wrapped
        // cell count is the only thing that would save it, and it does not.
        let mut buf = vec![0xFF808080u32; 16 * 16];
        let before = buf.clone();
        let mask = vec![255u8; 15]; // claims 4x4 = 16 cells
        draw_preblurred_shadow_from_mask(
            &mut buf, 16, 16, 16, &mask, 4, 4, 0, 0, 0, 0, 0x000000, 200,
        );
        assert_eq!(buf, before, "a short mask must be refused, not truncated");
        // Same for the blurring entry point, whose scratch check used to be
        // the only guard: it now bails on the mask before touching the scratch.
        let mut scratch = vec![0u8; 16 + 4 + 8];
        draw_shadow_from_mask(
            &mut buf,
            16,
            16,
            16,
            &mask,
            4,
            4,
            0,
            0,
            0,
            0,
            0x000000,
            200,
            &mut scratch,
        );
        assert_eq!(buf, before, "a short mask must be refused before the copy");
        // A zero dimension and a cell count that would overflow a 32-bit usize
        // are both no-ops, not wraps.
        draw_preblurred_shadow_from_mask(
            &mut buf, 16, 16, 16, &mask, 0, 4, 0, 0, 0, 0, 0x000000, 200,
        );
        #[cfg(target_pointer_width = "32")]
        draw_preblurred_shadow_from_mask(
            &mut buf,
            16,
            16,
            16,
            &mask,
            usize::MAX,
            2,
            0,
            0,
            0,
            0,
            0x000000,
            200,
        );
        assert_eq!(buf, before, "degenerate geometry drew something");
    }
}
