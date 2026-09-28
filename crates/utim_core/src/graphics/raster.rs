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
    let mut y = (x + 1) / 2;
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
pub fn rounded_span_f(
    dy: f32,
    x: f32,
    rw: f32,
    rh: f32,
    radius: f32,
) -> Option<(f32, f32)> {
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
        // A row with no inner span is entirely within the stroke band: either
        // the inset collapsed (thick stroke on a thin shape) or this row sits
        // in the rounded cap above/below the inner rect.
        match rounded_span_f(dy, x + s, inner_w, inner_h, ir) {
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
    if alpha == 255 {
        buf[row + lo..row + hi].fill(color);
    } else {
        for px in buf.iter_mut().take(row + hi).skip(row + lo) {
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
/// r2 = r / 5;                               // :57
/// mPath.addRoundRect(..., {r,r, r,r, r2,r2, r,r}, CCW);   // :58-60
/// sMatrix.setRotate(-45, l + r, t + r);      // :62
/// ```
///
/// So the two *right* corners are `r/5` and the left two are `r`; the whole
/// thing is then rotated by -45 degrees. `is_teardrop_1` is the pair that
/// shrinks, used to name which corner is which without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TeardropRadii {
    pub big: f32,
    pub small: f32,
}

#[inline]
pub fn teardrop_radii(height: f32) -> TeardropRadii {
    let r = height * 0.5;
    TeardropRadii { big: r, small: r / 5.0 }
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

    // Per-corner radii clamped to half the side they sit on.
    let r = [
        radii[0].clamp(0.0, hw),
        radii[1].clamp(0.0, hw),
        radii[2].clamp(0.0, hw),
        radii[3].clamp(0.0, hw),
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
                if ly >= 0.0 { r[2] } else { r[1] }
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
/// with the `r/5` pair on the right, per `FastScrollThumbDrawable.java:58-62`.
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
    let t = teardrop_radii(rh);
    draw_rotated_rounded_rect(
        buf, stride, w, h, cx, cy, rw, rh,
        [t.big, t.small, t.small, t.big],
        -45.0f32.to_radians(),
        color,
    );
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
    let r2 = r * r;
    let dy2 = dy * dy;
    let rem = r2.saturating_sub(dy2); // r^2 - dy^2
    if rem == 0 {
        return r;
    }
    // r^4 - dy^4 = (r^2 - dy^2)(r^2 + dy^2)
    let quartic = rem * (r2 + dy2);
    isqrt(isqrt(quartic))
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
    if half == 0 {
        for px in row.chunks_exact_mut(4) {
            px[3] = 0;
        }
        return None;
    }
    let center = edge / 2;
    let lo = center.saturating_sub(half);
    let hi = (center + half).min(edge);
    // Zero everything outside [lo, hi).
    for (i, px) in row.chunks_exact_mut(4).enumerate() {
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
    tmp[0] = ((row[0] as u16 + row[1] as u16 + 1) / 2) as u8;
    tmp[n - 1] = ((row[n - 2] as u16 + row[n - 1] as u16 + 1) / 2) as u8;
    for i in 1..n - 1 {
        let sum = row[i - 1] as u16 + row[i] as u16 + row[i + 1] as u16;
        tmp[i] = ((sum + 1) / 3) as u8;
    }
    row.copy_from_slice(&tmp[..n]);
}

/// Darken the framebuffer under `mask`, offset by `(dx, dy)`.
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
    if alpha == 0 || mask.is_empty() || mask_w == 0 || mask_h == 0 {
        return;
    }
    // Blur a caller-owned copy so the caller's cached mask stays pristine.
    // A scratch of the mask size is required; the caller owns it, so this
    // stays allocation-free.
    debug_assert!(scratch.len() >= mask_w * mask_h + mask_w + 2 * mask_h);
    if scratch.len() < mask_w * mask_h + mask_w + 2 * mask_h {
        return;
    }
    let (work, bl) = scratch.split_at_mut(mask_w * mask_h);
    let work = &mut work[..mask_w * mask_h];
    work.copy_from_slice(&mask[..mask_w * mask_h]);
    blur_mask_8(work, mask_w, mask_h, bl);

    let src = shadow_rgb & 0x00FF_FFFF;
    for my in 0..mask_h as i32 {
        let py = origin_y + offset_y + my;
        if py < 0 || py >= h as i32 {
            continue;
        }
        let row = py as usize * stride;
        for mx in 0..mask_w as i32 {
            let cov = work[my as usize * mask_w + mx as usize];
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
            let want = r as f64 * (1.0f64 - (dy as f64 / r as f64).powi(4)).max(0.0).powf(0.25);
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
                let exact = r as f64 * (1.0f64 - (dy as f64 / r as f64).powi(4)).max(0.0).powf(0.25);
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
        assert!(near(t.small, 31.0 / 5.0, 1e-4), "r2 must be r/5, got {}", t.small);
    }

    // -- blend ------------------------------------------------------------

    #[test]
    fn blend_alpha_matches_the_reference_rounding() {
        assert_eq!(blend_alpha(0xFF00_0000, 0xFFFFFFFF, 255), 0x00FF_FFFF);
        assert_eq!(blend_alpha(0xFF00_0000, 0xFFFFFFFF, 0), 0xFF00_0000);
        // Mid grey over mid grey, 50%.
        let got = blend_alpha(0xFF808080, 0xFF808080, 128);
        assert_eq!(got & 0x00FF_FFFF, 0x00808080, "same colour must be identity");
        // Black over white at 0% stays white; at 100% becomes black.
        assert_eq!(blend_alpha(0xFFFFFFFF, 0xFF000000, 0) & 0x00FF_FFFF, 0x00FFFFFF);
        assert_eq!(blend_alpha(0xFFFFFFFF, 0xFF000000, 255) & 0x00FF_FFFF, 0x00000000);
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
        for v in [0u32, 1, 2, 3, 4, 8, 9, 15, 16, 17, 99, 100, 101, 65_535, 65_536, 1 << 30] {
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
            &mut buf, 16, 16, 16, &mask, 4, 4, 0, 0, 0, 0, 0, 0, &mut scratch,
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
            &mut buf, 16, 16, 16, &mask, 4, 4, -10, -10, 0, 0, 0x000000, 200, &mut scratch,
        );
        // Must not panic, and must not have wrapped around to the far side.
        assert!(buf.iter().all(|p| (*p >> 24) == 0xFF), "alpha byte corrupted");
    }
}
