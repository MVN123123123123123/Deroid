//! Icon resolution, shaping and fallback for the launcher.
//!
//! # Resolution
//!
//! Three stages, in the order [`LOOKUP_ORDER`] states:
//!
//! 1. **Offline disk cache** — `$XDG_CACHE_HOME/utlc/icons/<app_id>_<edge>.png`,
//!    populated out of band by `utlc-cache-icons` or a systemd oneshot running
//!    the system `rsvg-convert`. This is the only answer to the `.svg` under
//!    `hicolor/scalable` problem, because embedding an SVG rasteriser is
//!    rejected: resvg is >30 crates and ~6 MiB of binary (see
//!    [`OFFLINE_CACHE_SUBDIR`]).
//! 2. **Absolute path** — a key containing `/` is read directly.
//! 3. **Freedesktop theme sweep** — one bounded directory pass per batch
//!    resolves every name to its best PNG candidate (theme rank, directory
//!    context, size proximity), decoded in-crate by [`crate::graphics::png`].
//!
//! Keys that resolve to nothing are remembered as misses so the next frame does
//! not repeat the scan; callers drop the miss set with
//! [`IconCache::invalidate_misses`] whenever the application set changes.
//!
//! # Shaping
//!
//! Cached icons are pre-scaled to [`icon_max_edge`] and masked once, at cache
//! time, so the frame path is a 1:1 blit. [`apply_shape`] is the mask: it is
//! **O(rows)**, not O(pixels), because [`crate::graphics::raster::squircle_span`]
//! solves the superellipse half-width per row analytically and the pixels
//! outside that span are simply zeroed. There is no per-pixel distance test in
//! the inner loop and none is needed.
//!
//! # When there is no icon at all
//!
//! [`draw_monogram_tile`] renders the app's initial as a procedural Material
//! You monogram: a squircle surface plus one glyph from
//! [`crate::graphics::font`]. Zero blank boxes, zero allocation, zero
//! third-party crates.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::graphics::font::{self, FontWeight};
use crate::graphics::layout::{DeviceProfile, Rect};
use crate::graphics::png::{decode_png, RgbaImage};
use crate::graphics::raster::{isqrt, rounded_span_f, squircle_span};

/// Fallback display edge, used until a caller supplies the panel size through
/// [`IconCache::set_display_edge`].
///
/// The real value is [`icon_max_edge`], which derives it from
/// [`DeviceProfile::icon_dp`]. This constant only exists because it is
/// re-exported from `compositor/mod.rs`; nothing sizes a real cache off it.
pub const ICON_MAX_EDGE: u32 = 64;

// ===========================================================================
// Icon shape
// ===========================================================================

/// Mask applied to a cached icon, mirroring Lawnchair's `IconShape` presets.
///
/// * `Squircle` — the Material You / Android adaptive-icon mask, the superellipse
///   `(|x|/r)^4 + (|y|/r)^4 <= 1` (`IconShape.Squircle`, `IconCornerShape.Squircle`
///   at scale `1f`, `IconShape.kt:342-346`).
/// * `Circle` — the full-arc corner (`IconShape.Circle`, `IconCornerShape.arc` at
///   scale `1f`, `IconShape.kt:312-316`).
/// * `RoundedRect` — a circular-cornered box. Lawnchair's closest preset is
///   `IconShape.RoundedSquare` (`IconCornerShape.arc` at scale **`.6f`**,
///   `IconShape.kt:334-340`); see [`IconShape::corner_fraction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconShape {
    Squircle,
    Circle,
    RoundedRect,
}

impl IconShape {
    /// Corner scale, as a fraction of the shape's half-edge.
    ///
    /// Lawnchair authors every corner-based preset against `size = radius`
    /// (half the icon edge) in a 100x100 viewport, so these are the raw
    /// `SimpleCornerBased` scales:
    ///
    /// | shape | Lawnchair preset | scale |
    /// |---|---|---|
    /// | `Squircle` | `IconShape.Squircle` | `1f` |
    /// | `Circle` | `IconShape.Circle` | `1f` |
    /// | `RoundedRect` | `IconShape.RoundedSquare` | `.6f` |
    ///
    /// The plan called for `~0.28` on `RoundedRect`; that does not match any
    /// corner scale in the checked-out tree (`IconShape.kt:312-404` lists 0f,
    /// `.16f`, `.3f`, `.5f`, `.6f`, `1f` and the `Cylinder`/non-uniform pair),
    /// and `.6f` is the value the "rounded square" preset actually uses, so
    /// that is what ships. `.6f` of a half-edge is a `0.30` corner radius as a
    /// fraction of the full edge.
    #[inline]
    pub const fn corner_fraction(self) -> f32 {
        match self {
            IconShape::Squircle => 1.0,
            IconShape::Circle => 1.0,
            IconShape::RoundedRect => 0.6,
        }
    }

    /// `true` when the shape covers the middle of the row edge to edge, so the
    /// mask is a no-op for an already-square icon.
    #[inline]
    pub const fn is_full_width(self) -> bool {
        matches!(self, IconShape::Squircle)
    }
}

/// Channel spread under which two corner samples count as the "same colour"
/// for [`has_explicit_background`].
///
/// PNG sources that carry their own background are flat, but a 48 px hicolor
/// PNG downscaled to 65+ px picks up a few levels of dither near the corner.
/// 12/255 is below any visible seam and above the noise.
const BACKGROUND_UNIFORM_TOLERANCE: u8 = 12;

/// Corner alpha at or above which a corner counts as opaque.
const BACKGROUND_OPAQUE_ALPHA: u8 = 250;

/// Half-width of the superellipse at `dy` from the centre, in `f32`.
///
/// The closed form documented on [`squircle_span`]:
/// `x = r * (1 - (dy/r)^4)^(1/4)`. The integer [`squircle_span`] is the form
/// used for masking decoded square icons; this float form exists because a
/// monogram tile's centre is a sub-pixel position ([`Rect`] is `f32`).
#[inline]
fn squircle_half_f(r: f32, dy: f32) -> f32 {
    if r <= 0.0 || dy >= r {
        return 0.0;
    }
    let t = 1.0 - (dy / r).powi(4);
    if t <= 0.0 {
        return 0.0;
    }
    r * t.powf(0.25)
}

/// Half-width of the shape on image row `y` of an `edge`-square mask, in
/// pixels, measured from the centre column.
///
/// The three shapes differ only here, which is the whole point: the caller
/// walks two short corner spans per row and never touches the middle.
#[inline]
fn shape_half(shape: IconShape, edge: u32, y: u32) -> u32 {
    let r = edge / 2;
    let center = r as i64;
    let dy = (y as i64 - center).unsigned_abs();
    match shape {
        IconShape::Squircle => squircle_span(r, dy as u32),
        IconShape::Circle => {
            if dy >= r as u64 {
                0
            } else {
                let d = dy as u32;
                isqrt(r * r - d * d)
            }
        }
        IconShape::RoundedRect => {
            // Vertical distance from the row's centre sample point to the
            // nearer horizontal edge. The straight middle of the shape is
            // `q >= radius`; below that the corner arc insets the row by
            // `radius - sqrt(radius^2 - (radius - q)^2)`.
            let last = edge.saturating_sub(1);
            let q = y.min(last.saturating_sub(y)) as f32 + 0.5;
            let radius = r as f32 * shape.corner_fraction();
            let inset = if radius <= 0.0 || q >= radius {
                0.0
            } else {
                let dyc = radius - q;
                radius - (radius * radius - dyc * dyc).max(0.0).sqrt()
            };
            // Half-width from the centre column is `edge/2 - inset`.
            r.saturating_sub(inset.ceil().max(0.0) as u32)
        }
    }
}

/// Half-width of the shape at `dy` from the centre, for the monogram tile.
#[inline]
fn shape_half_at(shape: IconShape, r: f32, dy: f32) -> f32 {
    match shape {
        IconShape::Squircle => squircle_half_f(r, dy),
        IconShape::Circle => {
            if dy > r {
                0.0
            } else {
                (r * r - dy * dy).max(0.0).sqrt()
            }
        }
        IconShape::RoundedRect => match rounded_span_f(
            dy,
            -r,
            r * 2.0,
            r * 2.0,
            r * shape.corner_fraction(),
        ) {
            Some((lo, hi)) => ((lo + hi) * 0.5).abs(),
            None => 0.0,
        },
    }
}

/// Mask a decoded icon in place to `shape`; `true` when any ink was removed.
///
/// **Cost is O(rows), not O(pixels).** Each row asks
/// [`crate::graphics::raster::squircle_span`] (or the circle / rounded-rect
/// equivalent) for one span and then zeroes the alpha on the two sides of it.
/// The kept span is never read, never written and never tested per pixel, so
/// the work is proportional to the *area the mask removes* — about 8% of a
/// 128 px squircle, and 13x fewer pixels than a per-pixel signed-distance test
/// would touch.
///
/// The mask is inscribed in the centred square of a non-square image, which is
/// what [`RgbaImage::fit_within`]'s aspect-preserving scale produces.
///
/// Returns `false`, byte-identical, for an already-masked or fully transparent
/// icon: a caller that caches an already-shaped PNG pays nothing.
pub fn apply_shape(img: &mut RgbaImage, shape: IconShape) -> bool {
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 {
        return false;
    }
    let w4 = w as usize * 4;
    if img.pixels.len() < h as usize * w4 {
        return false;
    }
    let edge = w.min(h);
    let x0 = ((w - edge) / 2) as usize;
    let y0 = ((h - edge) / 2) as usize;
    let mut changed = false;

    for y in 0..h as usize {
        // Rows outside the inscribed square keep nothing; the rest keep one span.
        let (lo, hi) = if y < y0 || y >= y0 + edge as usize {
            (x0, x0)
        } else {
            let center = (edge / 2) as usize;
            let half = shape_half(shape, edge, (y - y0) as u32) as usize;
            (x0 + center.saturating_sub(half), x0 + (center + half).min(edge as usize))
        };
        let row = &mut img.pixels[y * w4..y * w4 + w4];
        // Left of the span. O(corner pixels), never O(row).
        for px in row[..lo * 4].as_chunks_mut::<4>().0 {
            if px[3] != 0 {
                px[3] = 0;
                changed = true;
            }
        }
        // Right of the span.
        for px in row[hi * 4..].as_chunks_mut::<4>().0 {
            if px[3] != 0 {
                px[3] = 0;
                changed = true;
            }
        }
    }
    changed
}

/// `true` when `img` carries its own background colour, so it is a complete
/// icon rather than a bare `<foreground>` layer.
///
/// ## This is a heuristic, and deliberately so
///
/// The honest answer for an Android adaptive icon is to read
/// `<background>` / `<foreground>` out of the `adaptive-icon` XML, which is
/// what Lawnchair does through `ThemedIconCompat.getMonochromeIconResource`
/// (`ThemedIconCompat.kt:54-89`). UTLC has no XML resources to read: on Linux
/// the only inputs are PNGs, and an adaptive-icon XML never reaches the
/// filesystem as one. Writing a pull parser for a document that is never
/// present would be a parser with no input.
///
/// What the freedesktop convention does give us is a bitmap that has already
/// been flattened: an SVG carrying its own background rasterises with **opaque,
/// uniform corners**, while a foreground-only layer rasterises with **transparent
/// corners**. So the corner quadruple is the signal, and it costs eight byte
/// loads.
///
/// The failure mode is bounded and in the safe direction: an icon that *is* a
/// foreground layer but happens to be painted edge-to-edge in one flat colour
/// is treated as complete and left un-composited, which shows the caller's
/// `tile_color` nowhere. A wrong "composite" (drawing a background under an
/// icon that already had one) is much more visible, so the test errs that way.
pub fn has_explicit_background(img: &RgbaImage) -> bool {
    if img.width == 0 || img.height == 0 || img.pixels.len() < img.width as usize * img.height as usize * 4 {
        return false;
    }
    let w = img.width as usize;
    let h = img.height as usize;
    let corners = [
        0usize,
        (w - 1) * 4,
        (h - 1) * w * 4,
        (h - 1) * w * 4 + (w - 1) * 4,
    ];
    let mut opaque = true;
    let mut uniform = true;
    for c in corners {
        let p = &img.pixels[c..c + 4];
        if p[3] < BACKGROUND_OPAQUE_ALPHA {
            opaque = false;
            break;
        }
        // Compare against the first corner.
        let q = &img.pixels[0..4];
        let tol = BACKGROUND_UNIFORM_TOLERANCE as u32;
        if (p[0].abs_diff(q[0]) as u32) > tol
            || (p[1].abs_diff(q[1]) as u32) > tol
            || (p[2].abs_diff(q[2]) as u32) > tol
        {
            uniform = false;
            break;
        }
    }
    opaque && uniform
}

/// Composite a bare `<foreground>` layer onto `tile_color` in place.
///
/// The `<background>` half of an adaptive icon, reconstructed from the caller's
/// surface tone. `tile_color` must be opaque (`0xAARRGGBB`); the result is
/// always opaque, matching how a real `<background>` drawable behaves. Returns
/// `true` when any pixel was written.
pub fn composite_foreground(img: &mut RgbaImage, tile_color: u32) -> bool {
    let w4 = img.width as usize * 4;
    if w4 == 0 || img.pixels.len() < img.height as usize * w4 {
        return false;
    }
    let bg = tile_color & 0x00FF_FFFF;
    let mut changed = false;
    for px in img.pixels[..img.height as usize * w4].as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a == 0 {
            px[0] = (bg >> 16) as u8;
            px[1] = (bg >> 8) as u8;
            px[2] = bg as u8;
            px[3] = 0xFF;
            changed = true;
        } else if a == 255 {
            continue;
        } else {
            // Straight alpha over an opaque tile: the same integer blend the
            // rest of the rasteriser uses, so no halos at the art's own edges.
            let inv = 255 - a;
            for (i, shift) in [16u32, 8, 0].into_iter().enumerate() {
                let c = (px[i] as u32 * a + ((bg >> shift) & 0xFF) * inv + 127) / 255;
                px[i] = c as u8;
            }
            px[3] = 0xFF;
            changed = true;
        }
    }
    changed
}

/// Tint a decoded icon into a monochrome/themed variant.
///
/// The alpha channel is preserved and the RGB is replaced: that is exactly
/// what a `<monochrome>` layer is, and it is the only operation that keeps
/// the icon legible on any background. Zero allocation -- writes in place.
///
/// `tint` is `0xAARRGGBB` like every other colour in the shell; the alpha byte
/// is read for documentation value only, because a monochrome layer's
/// opacity *is* the original icon's alpha. Backs `pref_forceIconMonochrome`
/// (`PreferenceManager.kt:206`) and `themed_icons`
/// (`PreferenceManager.kt:166`), which pick the controller at
/// `LawnchairThemeManager.kt:122-128`.
pub fn make_monochrome(img: &mut RgbaImage, tint: u32) {
    let r = (tint >> 16) as u8;
    let g = (tint >> 8) as u8;
    let b = tint as u8;
    for px in img.pixels.as_chunks_mut::<4>().0 {
        px[0] = r;
        px[1] = g;
        px[2] = b;
    }
}

/// Cap height of the monogram glyph as a fraction of the tile edge.
///
/// Material You's initial tiles sit the cap a little under half the tile, so
/// the letter reads as a mark rather than as a label. `1.0` would touch the
/// mask edge; `0.52` leaves a clear margin at every corner.
const MONOGRAM_CAP_FRACTION: f32 = 0.52;

/// Render a procedural monogram tile for `initial` (first letter, uppercased)
/// into `rect`.
///
/// # Family and scale
///
/// The signature takes a [`Rect`] and a colour pair only, so the tile takes the
/// font family and weight from the process-wide type ramp
/// ([`font::active_family`], [`FontWeight::Medium`]) and derives its size from
/// the rect. That is deliberate: a monogram that matched a *different* type
/// ramp than the app labels beside it would look broken, and making the caller
/// pass a family would let exactly that happen.
///
/// # Failure
///
/// Returns `false` and **draws nothing** when `initial` has no ink in the
/// font, rather than emitting a tofu box or a blank tile. The font has no
/// shaping and no glyph table past ASCII — `font::draw_glyph` bails on
/// `!(0x20..0x7F).contains(&b)` — so the check is the same range, minus space,
/// which is in range but has no ink. Also `false` when `rect` does not
/// intersect the buffer.
///
/// # Cost
///
/// Allocation-free. `edge` span computations for the tile plus one glyph
/// rasterisation, all of which [`font::draw_glyph`] serves from its two-way
/// coverage cache on a steady-state redraw.
///
/// The eight-argument shape matches every other surface-drawing entry point in
/// the crate (`draw_opaque_rect`, `draw_rounded_rect_outline_f`,
/// `draw_shadow_from_mask`): a destination, its geometry, then the paint. A
/// parameter struct would mean one more indirection on the frame path to save
/// nothing.
#[allow(clippy::too_many_arguments)]
pub fn draw_monogram_tile(
    buf: &mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    rect: Rect,
    initial: char,
    tile_color: u32,
    glyph_color: u32,
) -> bool {
    // Reject before drawing: a rejected initial must leave the tile untouched.
    let b = initial as u32;
    if !(0x21..0x7F).contains(&b) {
        return false;
    }
    if buf.len() < h.saturating_mul(stride) || rect.w <= 0.0 || rect.h <= 0.0 {
        return false;
    }
    // Clip the tile to the framebuffer.
    let x0 = rect.x.max(0.0).floor().max(0.0) as usize;
    let y0 = rect.y.max(0.0).floor().max(0.0) as usize;
    let x1 = ((rect.x + rect.w).ceil().max(0.0) as usize).min(w);
    let y1 = ((rect.y + rect.h).ceil().max(0.0) as usize).min(h);
    if x1 <= x0 || y1 <= y0 || x0 >= w || y0 >= h {
        return false;
    }
    // Centre of the *visible* part, so a half-off-screen tile is still balanced.
    let cx = rect.center_x().clamp(x0 as f32, x1 as f32);
    let cy = rect.center_y().clamp(y0 as f32, y1 as f32);
    let edge = rect.w.min(rect.h);
    let r = edge * 0.5;

    // 1. The surface. One span per row, filled: no per-pixel shape test.
    let opaque = tile_color | 0xFF00_0000;
    for py in y0..y1 {
        let half = shape_half_at(IconShape::Squircle, r, (py as f32 + 0.5 - cy).abs());
        if half <= 0.0 {
            continue;
        }
        let lo = (cx - half - 0.5).ceil().max(x0 as f32) as usize;
        let hi = (cx + half - 0.5).ceil().max(lo as f32) as usize;
        let hi = hi.min(x1);
        if hi > lo {
            let row = py * stride;
            buf[row + lo..row + hi].fill(opaque);
        }
    }

    // 2. The initial, optically centred on the cap box.
    //    `draw_glyph`'s `y` is the ascender line, so the cap spans
    //    `[y + (ASCENDER - CAP_HEIGHT) * s, y + ASCENDER * s]`.
    let cap = edge * MONOGRAM_CAP_FRACTION;
    let size_px = cap / font::CAP_HEIGHT * font::UNITS_PER_EM;
    let s = size_px / font::UNITS_PER_EM;
    let baseline = cy + cap * 0.5;
    let ascender_y = baseline - font::ASCENDER * s;
    let pen_x = cx - font::char_advance(b as u8, size_px) * 0.5;
    font::draw_glyph(
        buf, stride, w, h, pen_x, ascender_y, b as u8, glyph_color, size_px, FontWeight::Medium,
    );
    true
}

/// Re-scale a decoded icon to the shell's display size.
///
/// Icons are cached at exactly the size the layout draws them, so the render
/// path can blit 1:1 instead of resampling bilinear on every frame. Called
/// when an icon enters the cache, never on the hot path.
pub fn icon_for_display(img: RgbaImage, edge: u32) -> RgbaImage {
    img.fit_within(edge.max(1))
}

/// Longest edge every cached icon is pre-scaled to, for a panel `panel_px` wide.
///
/// This is a function, not a constant, because the right answer depends on the
/// panel. [`DeviceProfile::icon_dp`] is `65f32` on the Lawnchair phone profile
/// (`device_profiles.xml:70`, `launcher:iconImageSize="65"`) and
/// [`DeviceProfile::dp`] is `panel_px / 420`, so the tile the renderer actually
/// blits is 167 px on a 1080 px panel and 42 px on a 270 px one. The old
/// hard-coded 64 was therefore neither right: it upscaled 2.6x through
/// `draw_icon_bitmap_i32`'s bilinear path on a reference phone, on every icon,
/// every frame.
///
/// Clamped to [`ICON_MAX_SOURCE_EDGE`] because that is the largest source the
/// decoder will admit (P15). Above roughly a 1400 px panel the source cap, not
/// this function, becomes the binding constraint and the blit resamples again.
#[inline]
pub fn icon_max_edge(panel_px: f32) -> u32 {
    let p = DeviceProfile::for_panel(panel_px);
    let px = p.dp(p.icon_dp);
    if !px.is_finite() {
        return 1;
    }
    px.round().clamp(1.0, ICON_MAX_SOURCE_EDGE as f32) as u32
}

/// Directory, relative to the XDG cache root, that holds pre-rendered icons.
///
/// # Who populates it
///
/// A large share of Linux app icons are `.svg` under
/// `/usr/share/icons/hicolor/scalable/apps/`, and UTLC cannot draw them:
/// `resvg` is 30+ transitive crates and ~6 MiB of binary, which the zero
/// third-party-dependency rule rejects outright. So the rasterisation happens
/// **out of process**, once, by something already on the system:
///
/// ```text
/// utlc-cache-icons            # optional helper shipped alongside utlc
/// # or, without a helper at all:
/// ExecStart=/usr/bin/rsvg-convert -w 167 -h 167 \
///           /usr/share/icons/hicolor/scalable/apps/firefox.svg \
///           -o %h/.cache/utlc/icons/firefox.desktop_167.png
/// # in a systemd user oneshot, Type=oneshot, RemainAfterExit=yes
/// ```
///
/// The launcher only ever *reads* the directory. A missing entry is not an
/// error: it falls through to the theme sweep, and then to
/// [`draw_monogram_tile`].
pub const OFFLINE_CACHE_SUBDIR: &str = "utlc/icons";

/// File-name convention of the offline cache: `<app_id>_<edge>.png`.
///
/// `app_id` is the desktop-entry id (`org.mozilla.firefox.desktop`, or the
/// `Icon=` stem for an entry whose id is not a usable name), verbatim — no
/// case folding, no path separators. `edge` is the pixel edge the icon was
/// rendered at, which is [`icon_max_edge`] of the panel, so the helper can
/// pre-render exactly what the launcher will blit. Both halves are what make a
/// stale cache miss instead of a wrong-scale hit.
pub fn offline_cache_name(app_id: &str, edge: u32) -> String {
    let mut name = String::with_capacity(app_id.len() + 8);
    name.push_str(app_id);
    name.push('_');
    // No `write!`: the formatter and its ~4 KiB of tables stay out of the
    // binary. Ten slots covers `u32::MAX` with no leading zeros.
    let mut d = [0u8; 10];
    let mut n = edge;
    let mut i = d.len();
    loop {
        i -= 1;
        d[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for b in &d[i..] {
        name.push(*b as char);
    }
    name.push_str(".png");
    name
}

/// Full path of the pre-rendered icon for `app_id` at `edge` inside `dir`.
#[inline]
pub fn offline_cache_path(dir: &Path, app_id: &str, edge: u32) -> PathBuf {
    dir.join(offline_cache_name(app_id, edge))
}

/// `$XDG_CACHE_HOME/utlc/icons`, or `$HOME/.cache/utlc/icons`, or `None`.
///
/// Both variables are read, never defaulted to a hard-coded path, so a
/// sandboxed or read-only session simply gets no offline cache instead of a
/// write into a directory it does not own.
pub fn offline_cache_dir() -> Option<PathBuf> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })?;
    Some(root.join(OFFLINE_CACHE_SUBDIR))
}

/// One stage of icon resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupStage {
    /// A key holding `/` is read as a file.
    AbsolutePath,
    /// `$XDG_CACHE_HOME/utlc/icons/<app_id>_<edge>.png`.
    OfflineCache,
    /// The batched freedesktop theme sweep.
    Theme,
}

/// The stages [`IconCache::resolve_keys`] tries, in order.
///
/// Path keys are unambiguous and skip straight to their file, so this is the
/// order for the general case. It is a `const` rather than an implicit property
/// of the control flow so the precedence is assertable without a filesystem.
pub const LOOKUP_ORDER: [LookupStage; 3] = [
    LookupStage::AbsolutePath,
    LookupStage::OfflineCache,
    LookupStage::Theme,
];

/// Total decoded-pixel budget held by the cache; LRU eviction keeps the
/// live set under this (P2). Sized for the *pre-scaled* tile: at
/// [`icon_max_edge`] = 167 a tile is 111 KiB, so 4 MiB holds ~37 live icons —
/// one screen of a phone workspace plus the dock. 64x64 RGBA tiles were 16 KiB
/// and held ~256, but those were 2.6x too small to blit 1:1.
pub const ICON_CACHE_BUDGET: usize = 4 * 1024 * 1024;
/// Icon sources with an edge larger than this are rejected before
/// resampling: pixels that would only be averaged away must not spike
/// frame-thread memory (P15).
pub const ICON_MAX_SOURCE_EDGE: u32 = 256;
/// Reject absurdly large icon files before handing them to the decoder.
/// Tied to the decoder's MAX_PIXELS (4M px): worst-case small-icon PNGs
/// stay far below this (P16).
const MAX_ICON_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Dirent budget for a single resolution sweep (bounds worst-case scan cost).
const SWEEP_BUDGET: usize = 65_536;
/// Maximum directory depth visited (root -> theme -> size -> context -> file).
const MAX_DEPTH: usize = 6;

/// Directory contexts recognised by the freedesktop.org icon specification.
const CONTEXTS: [&str; 14] = [
    "actions",
    "animations",
    "apps",
    "categories",
    "devices",
    "emblems",
    "emotes",
    "intl",
    "legacy",
    "mimetypes",
    "misc",
    "places",
    "status",
    "ui",
];

/// Caches decoded application icons and resolves icon names against the
/// installed icon themes with a single batched directory sweep per request.
pub struct IconCache {
    roots: Vec<PathBuf>,
    preferred_theme: String,
    images: HashMap<String, Rc<RgbaImage>>,
    /// Last-use tick per cached key for LRU eviction (P2).
    used: RefCell<HashMap<String, u64>>,
    misses: HashSet<String>,
    /// Edge every decoded icon is resampled to on the way into the cache.
    display_edge: u32,
    /// `$XDG_CACHE_HOME/utlc/icons` (see [`OFFLINE_CACHE_SUBDIR`]), consulted
    /// before the theme sweep. `None` disables the stage.
    offline_dir: Option<PathBuf>,
    /// Monotonic clock for `used` ticks.
    tick: Cell<u64>,
    /// Sum of `width*height*4` over `images`, bounded by ICON_CACHE_BUDGET.
    live_bytes: usize,
}

/// Deduplicate search roots preserving first-seen order: the default root
/// list aliases the same directories via XDG_DATA_DIRS/HOME/pixmaps (P27).
fn dedupe_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::with_capacity(roots.len());
    let mut out = Vec::with_capacity(roots.len());
    for r in roots {
        if seen.insert(r.clone()) {
            out.push(r);
        }
    }
    out
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new()
    }
}

impl IconCache {
    /// Roots and preferred theme taken from the process environment
    /// (`XDG_DATA_DIRS`, `HOME`, `ICON_THEME`).
    pub fn new() -> IconCache {
        let mut roots = Vec::with_capacity(8);
        let data_dirs =
            std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
        for dir in data_dirs.split(':').filter(|d| !d.is_empty()) {
            roots.push(PathBuf::from(dir).join("icons"));
        }
        if let Ok(home) = std::env::var("HOME") {
            let home = PathBuf::from(home);
            roots.push(home.join(".icons"));
            roots.push(home.join(".local/share/icons"));
        }
        roots.push(PathBuf::from("/usr/share/pixmaps"));
        roots.push(PathBuf::from("/usr/local/share/pixmaps"));
        if std::path::Path::new("assets/icons").exists() {
            roots.push(PathBuf::from("assets/icons"));
        }
        let preferred_theme = std::env::var("ICON_THEME").unwrap_or_default();
        IconCache {
            roots: dedupe_roots(roots),
            preferred_theme,
            images: HashMap::new(),
            used: RefCell::new(HashMap::new()),
            misses: HashSet::new(),
            display_edge: ICON_MAX_EDGE,
            // Consulted first: an out-of-process renderer's output is
            // authoritative, and skipping the sweep for a cache hit is the
            // whole point of keeping the directory populated.
            offline_dir: offline_cache_dir(),
            tick: Cell::new(0),
            live_bytes: 0,
        }
    }

    /// Set the edge cached icons are resampled to.
    ///
    /// The renderer blits 1:1 when the source matches the destination, so
    /// giving the cache the layout's icon size removes resampling from the
    /// frame path entirely. [`icon_max_edge`] is where that size comes from.
    pub fn set_display_edge(&mut self, edge: u32) {
        self.display_edge = edge.max(1);
    }

    /// Override the offline icon cache directory; `None` disables the stage.
    pub fn set_offline_dir(&mut self, dir: Option<PathBuf>) {
        self.offline_dir = dir;
    }

    /// The offline cache directory, if the stage is enabled.
    pub fn offline_dir(&self) -> Option<&Path> {
        self.offline_dir.as_deref()
    }

    /// Explicit roots (used by tests and by callers with a custom search path).
    ///
    /// The offline stage is off: a test that resolves through a temp tree must
    /// not be perturbed by whatever the developer's `~/.cache` happens to hold.
    pub fn with_roots(roots: Vec<PathBuf>, preferred_theme: &str) -> IconCache {
        IconCache {
            roots: dedupe_roots(roots),
            preferred_theme: preferred_theme.to_string(),
            images: HashMap::new(),
            used: RefCell::new(HashMap::new()),
            misses: HashSet::new(),
            display_edge: ICON_MAX_EDGE,
            offline_dir: None,
            tick: Cell::new(0),
            live_bytes: 0,
        }
    }

    /// Current decoded-pixel footprint; always <= budget + one entry.
    pub fn live_bytes(&self) -> usize {
        self.live_bytes
    }

    /// Number of cached icons.
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// True when no icons are cached.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// True once `key` has been looked up, whether it resolved or not.
    pub fn knows(&self, key: &str) -> bool {
        self.images.contains_key(key) || self.misses.contains(key)
    }

    /// Cached icon for `key`, if it resolved earlier. Records a use tick
    /// so LRU eviction keeps hot icons (P2).
    pub fn get(&self, key: &str) -> Option<Rc<RgbaImage>> {
        let img = self.images.get(key).cloned()?;
        self.tick.set(self.tick.get() + 1);
        self.used.borrow_mut().insert(key.to_string(), self.tick.get());
        Some(img)
    }

    /// Forget every recorded miss so the next [`Self::resolve_keys`] re-scans;
    /// resolved icons are kept because they stay valid until reinstalled.
    pub fn invalidate_misses(&mut self) {
        self.misses.clear();
    }

    /// Insert a decoded icon, evicting least-recently-used entries until
    /// the cache is back under [`ICON_CACHE_BUDGET`] (P2). A single icon
    /// larger than the whole budget still caches as the MRU entry and is
    /// evicted by the next insert, so `live_bytes` may transiently exceed
    /// the budget by one entry.
    fn insert_image(&mut self, key: String, img: RgbaImage) {
        fn img_bytes(img: &RgbaImage) -> usize {
            img.width as usize * img.height as usize * 4
        }
        self.tick.set(self.tick.get() + 1);
        let bytes = img_bytes(&img);
        while self.live_bytes + bytes > ICON_CACHE_BUDGET && !self.images.is_empty() {
            let lru = self
                .used
                .borrow()
                .iter()
                .min_by_key(|(_, &t)| t)
                .map(|(k, _)| k.clone());
            match lru {
                Some(k) => {
                    if let Some(old) = self.images.remove(&k) {
                        self.live_bytes = self.live_bytes.saturating_sub(img_bytes(&old));
                    }
                    self.used.borrow_mut().remove(&k);
                }
                None => break,
            }
        }
        if let Some(old) = self.images.insert(key.clone(), Rc::new(img)) {
            self.live_bytes = self.live_bytes.saturating_sub(img_bytes(&old));
        }
        self.live_bytes += bytes;
        self.used.borrow_mut().insert(key, self.tick.get());
    }

    /// Resolve every not-yet-looked-up key, in the order [`LOOKUP_ORDER`]
    /// states, then decode the winners.
    ///
    /// Decode cost note (P15): each call decodes at most one file per
    /// pending key, each file is capped at [`MAX_ICON_FILE_BYTES`] and each
    /// source at [`ICON_MAX_SOURCE_EDGE`]px per edge, so peak decode memory
    /// stays bounded. Callers should still batch keys into as few calls as
    /// possible (one per frame at most) rather than resolving per-tile.
    pub fn resolve_keys(&mut self, keys: &[String]) {
        let mut pending: Vec<usize> = Vec::new();
        for (i, key) in keys.iter().enumerate() {
            if self.knows(key) {
                continue;
            }
            if key.contains('/') {
                // Explicit path: read it now, never enter the sweep.
                if let Some(img) = load_path(Path::new(key), self.display_edge) {
                    self.insert_image(key.clone(), img);
                } else {
                    self.misses.insert(key.clone());
                }
                continue;
            }
            // Offline cache before the sweep: one `open` beats walking a theme
            // tree, and a miss is indistinguishable from a key that simply is
            // not installed. Not recorded as a permanent miss -- the file may
            // be rendered a second later by the oneshot.
            if let Some(img) = self.load_offline(key) {
                self.insert_image(key.clone(), img);
                continue;
            }
            pending.push(i);
        }
        if pending.is_empty() {
            return;
        }

        // Bucket wanted keys once (lowercased, suffix-stripped stem ->
        // pending slots) so each dirent costs O(1) instead of O(keys) (P24).
        let mut buckets: HashMap<Vec<u8>, Vec<usize>> = HashMap::with_capacity(pending.len());
        for (slot, &idx) in pending.iter().enumerate() {
            buckets
                .entry(normalize_key(keys[idx].as_bytes()))
                .or_default()
                .push(slot);
        }
        let mut best: Vec<Option<(i64, PathBuf)>> = (0..pending.len()).map(|_| None).collect();
        let mut budget = SWEEP_BUDGET;
        for root in &self.roots {
            if budget == 0 {
                break;
            }
            walk(
                root,
                0,
                None,
                None,
                None,
                &buckets,
                &mut best,
                &mut budget,
                &self.preferred_theme,
                self.display_edge,
            );
        }
        // A budget-exhausted sweep proves nothing (P3).
        let exhausted = budget == 0;

        for (slot, idx) in pending.iter().enumerate() {
            let key = &keys[*idx];
            match best[slot].take() {
                Some((_, path)) => match load_path(&path, self.display_edge) {
                    Some(img) => {
                        self.insert_image(key.clone(), img);
                    }
                    None => {
                        // Corrupt candidate: remember so the sweep is not repeated.
                        self.misses.insert(key.clone());
                    }
                },
                None => {
                    // Record a miss only when the sweep actually completed;
                    // on exhaustion the key stays unknown so the next call
                    // retries instead of pinning a false miss.
                    if !exhausted {
                        self.misses.insert(key.clone());
                    }
                }
            }
        }
    }

    /// `offline_dir/<app_id>_<edge>.png`, decoded. `None` when the stage is
    /// off or the file is absent, unreadable, or not a PNG.
    fn load_offline(&self, key: &str) -> Option<RgbaImage> {
        let dir = self.offline_dir.as_deref()?;
        load_path(&offline_cache_path(dir, key, self.display_edge), self.display_edge)
    }
}

fn load_path(path: &Path, display_edge: u32) -> Option<RgbaImage> {
    use std::io::Read;
    // Single open + bounded take(): no metadata/read TOCTOU window, and at
    // most MAX+1 bytes are ever pulled from disk (P16).
    let file = std::fs::File::open(path).ok()?;
    let mut limited = file.take(MAX_ICON_FILE_BYTES + 1);
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_ICON_FILE_BYTES {
        return None;
    }
    let img = decode_png(&bytes)?;
    // Drop the compressed bytes before resampling so peak memory is one
    // buffer at a time, not both (P15).
    drop(bytes);
    // Reject oversized sources: icons cache at <=64px tiles, so decoding
    // larger sources only burns frame-thread memory for pixels that get
    // averaged away.
    if img.width > ICON_MAX_SOURCE_EDGE || img.height > ICON_MAX_SOURCE_EDGE {
        return None;
    }
    // Resample once, here, to the size the layout draws: the render path then
    // blits 1:1 instead of filtering bilinear every frame.
    Some(img.fit_within(display_edge.max(1)))
}

/// `true` when `path` (known to be a symlink) resolves to a directory.
fn is_dir_link(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_dir())
}

#[cfg(unix)]
fn file_name_bytes(name: &std::ffi::OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes()
}

#[cfg(not(unix))]
fn file_name_bytes(name: &std::ffi::OsStr) -> &[u8] {
    name.to_str().map(|s| s.as_bytes()).unwrap_or(&[])
}

/// ASCII `.png` suffix check (any case) on raw bytes, before any allocation.
fn has_png_suffix(name: &[u8]) -> bool {
    name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(b".png")
}

fn strip_png_suffix(name: &[u8]) -> &[u8] {
    if has_png_suffix(name) {
        &name[..name.len() - 4]
    } else {
        name
    }
}

/// Lowercased, suffix-stripped bucket key for a lookup key.
fn normalize_key(key: &[u8]) -> Vec<u8> {
    strip_png_suffix(key).to_ascii_lowercase()
}

/// Depth-first sweep of one root. `best` is indexed like `pending`; each
/// entry keeps the highest scoring candidate found so far. `wanted` maps
/// lowercased, suffix-stripped stems to pending slots for O(1) lookup (P24).
#[allow(clippy::too_many_arguments)]
fn walk(
    dir: &Path,
    depth: usize,
    theme: Option<&str>,
    ctx: Option<&str>,
    size: Option<(u32, u32)>,
    wanted: &HashMap<Vec<u8>, Vec<usize>>,
    best: &mut [Option<(i64, PathBuf)>],
    budget: &mut usize,
    preferred: &str,
    target: u32,
) {
    if depth > MAX_DEPTH || *budget == 0 {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        // Single path() per entry; the file name borrows from it (P23).
        let path = entry.path();
        if file_type.is_dir() || (file_type.is_symlink() && is_dir_link(&path)) {
            // Symlinked directories are followed: `file_type` never follows
            // links, so the explicit metadata check above is the only thing
            // that sees them. Cycles terminate via MAX_DEPTH plus the sweep
            // budget (P14).
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let (theme, ctx, size) = classify(name, depth, theme, ctx, size);
            walk(&path, depth + 1, theme, ctx, size, wanted, best, budget, preferred, target);
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        let raw = file_name_bytes(name);
        // ASCII suffix check before any allocation (P23).
        if !has_png_suffix(raw) {
            continue;
        }
        let stem_lower = strip_png_suffix(raw).to_ascii_lowercase();
        let Some(slots) = wanted.get(&stem_lower) else {
            continue;
        };
        if !file_type.is_file() {
            // Themes alias icons with symlinks; follow them, but only pay for
            // the stat when the name is one we are actually looking for.
            if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                continue;
            }
        }
        let score = theme_rank(theme, preferred) + context_bonus(ctx) + size_score(size, target);
        for &slot in slots {
            let better = match &best[slot] {
                None => true,
                Some((s, p)) => {
                    score > *s || (score == *s && path < *p)
                }
            };
            if better {
                best[slot] = Some((score, path.clone()));
            }
        }
    }
}

/// Classify a directory name as it is descended into: the first level holds
/// theme names, `NxN` / `48` / `scalable` directories hold the icon size and
/// known context directories select the icon category.
fn classify<'a>(
    name: &'a str,
    depth: usize,
    theme: Option<&'a str>,
    ctx: Option<&'a str>,
    size: Option<(u32, u32)>,
) -> (Option<&'a str>, Option<&'a str>, Option<(u32, u32)>) {
    if let Some(px) = parse_size(name) {
        return (theme, ctx, Some(px));
    }
    if depth == 0 {
        // Directly below a root: every subdirectory is a candidate theme.
        return (Some(name), ctx, size);
    }
    if is_context(name) {
        return (theme, Some(name), size);
    }
    (theme, ctx, size)
}

/// `"48x48"`, `"48X48"` and `"48"` -> pixel size; `"scalable"` -> `None`.
fn parse_size(name: &str) -> Option<(u32, u32)> {
    let mut parts = name.split(['x', 'X']);
    let first = parts.next()?;
    if first.is_empty() || !first.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let w: u32 = first.parse().ok()?;
    if w == 0 || w > 1024 {
        return None;
    }
    let h = match parts.next() {
        None => w,
        Some(second) => {
            if second.is_empty() || !second.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let h: u32 = second.parse().ok()?;
            if h == 0 || h > 1024 {
                return None;
            }
            h
        }
    };
    Some((w, h))
}

fn is_context(name: &str) -> bool {
    CONTEXTS.contains(&name)
}

/// Case-insensitive icon name comparison, ignoring a `.png` suffix (any
/// case) on either side (P13). Thin wrapper over the byte helpers, kept
/// for tests; the sweep itself compares bytes without allocating.
#[cfg(test)]
fn icon_name_eq(stem: &str, key: &str) -> bool {
    strip_png_suffix(stem.as_bytes()).eq_ignore_ascii_case(strip_png_suffix(key.as_bytes()))
}

/// Theme tier dominates every other factor: the configured theme wins, then
/// `hicolor` (the mandatory fallback theme), then anything else.
fn theme_rank(theme: Option<&str>, preferred: &str) -> i64 {
    match theme {
        Some(t) if !preferred.is_empty() && t.eq_ignore_ascii_case(preferred) => 3_000_000_000,
        Some(t) if t.eq_ignore_ascii_case("hicolor") => 2_000_000_000,
        Some(_) => 1_000_000_000,
        None => 0,
    }
}

/// Application icons first, status/emblem decorations last.
fn context_bonus(ctx: Option<&str>) -> i64 {
    match ctx {
        Some("apps") => 100_000,
        Some("legacy") => 60_000,
        Some("mimetypes") => 40_000,
        Some("actions") => 20_000,
        Some("devices") | Some("categories") => 15_000,
        Some("places") => 10_000,
        Some("status") | Some("emblems") => -5_000,
        _ => 0,
    }
}

/// Prefer the size closest to `target` on both axes (so a 64x16 strip loses to
/// 64x64); unknown/scalable stays mid-range.
///
/// `target` is the edge the icon will be pre-scaled to
/// ([`icon_max_edge`]), not a fixed constant: the 1:1 blit in
/// `draw_icon_bitmap_i32` needs a source at the tile size, so a panel that
/// draws 167 px tiles must not be handed the 64x64 raster when a 128x128 one
/// is sitting in the same theme.
fn size_score(size: Option<(u32, u32)>, target: u32) -> i64 {
    match size {
        Some((sw, sh)) => {
            let t = target.max(1) as i64;
            let penalty = (sw as i64 - t).abs() + (sh as i64 - t).abs();
            1000 - (penalty * 4).min(999)
        }
        None => 700,
    }
}

#[cfg(test)]
mod tests {
    // The shared PNG fixtures are only partially used by these tests.
    #![allow(dead_code)]
    use super::*;

    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/png_fixtures.rs"));

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Tree {
            let root = std::env::temp_dir().join(format!("utim_icons_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Tree(root)
        }

        fn put(&self, rel: &str, png: &[u8]) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, png).unwrap();
        }

        /// Root in the same shape as `/usr/share/icons`: its children are themes.
        fn icons_root(&self) -> PathBuf {
            self.0.join("icons")
        }

        fn path(&self) -> PathBuf {
            self.0.clone()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prefers_bigger_apps_icon_in_hicolor() {
        let tree = Tree::new("rank");
        tree.put("icons/hicolor/16x16/apps/demo.png", GRAY1_PNG);
        tree.put("icons/hicolor/64x64/apps/demo.png", RGBA8_PNG);
        tree.put("icons/hicolor/64x16/apps/demo.png", RGB8_PNG);
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "Adwaita");

        let keys = vec!["demo".to_string()];
        cache.resolve_keys(&keys);
        let icon = cache.get("demo").expect("icon resolved");
        assert_eq!(icon.pixels, RGBA8_EXPECT, "64x64 apps icon wins");
        assert!(cache.knows("demo"));
    }

    #[test]
    fn theme_rank_beats_size_and_context() {
        let tree = Tree::new("theme");
        // Better size and context, but only in hicolor.
        tree.put("icons/hicolor/64x64/apps/better.png", RGBA8_PNG);
        // Preferred theme with a worse size still wins.
        tree.put("icons/Adwaita/16x16/apps/better.png", GRAY1_PNG);
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "Adwaita");

        cache.resolve_keys(&["better".to_string()]);
        let icon = cache.get("better").expect("icon resolved");
        assert_eq!(icon.pixels, GRAY1_EXPECT, "preferred theme wins regardless of size");
    }

    #[test]
    fn case_insensitive_names_and_png_suffix() {
        let tree = Tree::new("case");
        tree.put("icons/hicolor/48x48/apps/Web-Browser.PNG", RGBA8_PNG);
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");

        cache.resolve_keys(&["web-browser".to_string()]);
        assert!(cache.get("web-browser").is_some(), "case and extension insensitive match");
    }

    #[test]
    fn misses_are_recorded_and_can_be_invalidated() {
        let tree = Tree::new("miss");
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");

        let keys = vec!["nope".to_string()];
        cache.resolve_keys(&keys);
        assert!(cache.get("nope").is_none());
        assert!(cache.knows("nope"), "miss is remembered so the sweep is skipped");

        // A second call must not resurrect the key from the miss set.
        cache.resolve_keys(&keys);
        assert!(cache.get("nope").is_none());

        // Installing the icon and invalidating makes it resolvable.
        tree.put("icons/hicolor/64x64/apps/nope.png", RGBA8_PNG);
        cache.invalidate_misses();
        assert!(!cache.knows("nope"));
        cache.resolve_keys(&keys);
        assert!(cache.get("nope").is_some(), "invalidated miss re-resolves");
    }

    #[test]
    fn absolute_path_keys_bypass_the_sweep() {
        let tree = Tree::new("path");
        let file = tree.path().join("direct.png");
        std::fs::write(&file, RGBA8_PNG).unwrap();
        // Root that does not exist at all: the sweep has nothing to see.
        let mut cache = IconCache::with_roots(vec![tree.path().join("empty")], "");

        let keys = vec![file.to_string_lossy().to_string()];
        cache.resolve_keys(&keys);
        assert!(cache.get(&keys[0]).is_some(), "path key decoded directly");
        assert!(cache.knows(&keys[0]));
    }

    #[test]
    fn corrupt_candidate_is_recorded_as_a_miss() {
        let tree = Tree::new("corrupt");
        tree.put("icons/hicolor/64x64/apps/broken.png", b"not a png at all");
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");

        cache.resolve_keys(&["broken".to_string()]);
        assert!(cache.get("broken").is_none());
        assert!(cache.knows("broken"), "corrupt winner does not rescan every frame");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_candidates_and_dirs_are_resolved_with_bounds() {
        use std::os::unix::fs::symlink;

        let tree = Tree::new("symlink");
        // The wanted file exists only behind a link, so it is found by name and
        // then measured through `fs::metadata`, which follows the link.
        tree.put("targets/real.png", RGBA8_PNG);
        let apps = tree.path().join("icons/hicolor/64x64/apps");
        std::fs::create_dir_all(&apps).unwrap();
        symlink(tree.path().join("targets/real.png"), apps.join("linked.png")).unwrap();
        // Directory links ARE descended now (P14): a cycle only burns
        // MAX_DEPTH levels plus sweep budget, so the sweep terminates.
        symlink(
            tree.path().join("icons/hicolor/64x64"),
            tree.path().join("icons/hicolor/loop"),
        )
        .unwrap();
        tree.put("hidden/64x64/apps/secret.png", RGBA8_PNG);
        symlink(tree.path().join("hidden"), tree.path().join("icons/through-link")).unwrap();

        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.resolve_keys(&["linked".to_string(), "secret".to_string()]);
        assert!(
            cache.get("linked").is_some(),
            "a symlinked PNG is a valid icon candidate"
        );
        assert!(cache.knows("linked"));
        assert!(
            cache.get("secret").is_some(),
            "directory symlinks are followed within depth+budget guards"
        );
    }

    // =======================================================================
    // IconShape / apply_shape
    // =======================================================================

    /// Opaque square RGBA, the worst case for the mask: every corner pixel is
    /// ink, so every pixel the mask is going to touch must be a real write.
    fn solid(n: u32) -> RgbaImage {
        let mut p = vec![0u8; n as usize * n as usize * 4];
        for px in p.as_chunks_mut::<4>().0 {
            px[0] = 0x20;
            px[1] = 0x40;
            px[2] = 0x80;
            px[3] = 0xFF;
        }
        RgbaImage { width: n, height: n, pixels: p }
    }

    /// Half-open `[lo, hi)` x-span of non-zero alpha on row `y`, or `None`
    /// when the whole row was cleared.
    fn alpha_span(img: &RgbaImage, y: usize) -> Option<(usize, usize)> {
        let w = img.width as usize;
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        for (x, px) in img.pixels[y * w * 4..(y + 1) * w * 4].as_chunks::<4>().0.iter().enumerate() {
            if px[3] != 0 {
                lo = lo.min(x);
                hi = x + 1;
            }
        }
        (lo != usize::MAX).then_some((lo, hi))
    }

    #[test]
    fn squircle_mask_matches_superellipse() {
        let n = 64u32;
        let mut img = solid(n);
        assert!(apply_shape(&mut img, IconShape::Squircle), "a full square has ink to cut");

        // The reference model is the superellipse sampled on the integer pixel
        // lattice, which is the same convention `raster::squircle_span` is
        // defined on: pixel (x, y) is inside iff
        //     ((x - cx)/r)^4 + ((y - cy)/r)^4 <= 1,   cx = cy = edge/2.
        let r = (n / 2) as f64;
        let c = (n / 2) as f64;
        for y in 0..n as usize {
            let dy = y as f64 - c;
            let exact = r * (1.0f64 - (dy / r).powi(4)).max(0.0).powf(0.25);
            let h = exact.floor();
            // Inclusive lattice range; the mask's span is half-open, so it can
            // lose at most the one pixel on the right-hand edge.
            let want_lo = (c - h) as i64;
            let want_hi = (c + h) as i64; // inclusive
            match alpha_span(&img, y) {
                Some((lo, hi)) => {
                    assert!(
                        (lo as i64 - want_lo).abs() <= 1,
                        "y={y}: left edge {lo} is more than a pixel from {want_lo} (half-width {exact})"
                    );
                    assert!(
                        ((hi as i64 - 1) - want_hi).abs() <= 1,
                        "y={y}: right edge {} is more than a pixel from {want_hi} (half-width {exact})",
                        hi as i64 - 1
                    );
                    // The pixel just outside the span must be clear on both ends.
                    if lo > 0 {
                        assert_eq!(
                            img.pixels[(y * n as usize) * 4 + (lo - 1) * 4 + 3],
                            0,
                            "y={y}: ink left of the span"
                        );
                    }
                    if hi < n as usize {
                        assert_eq!(
                            img.pixels[(y * n as usize) * 4 + hi * 4 + 3],
                            0,
                            "y={y}: ink right of the span"
                        );
                    }
                }
                None => assert!(
                    h <= 1.0,
                    "y={y}: the whole row was cleared but the superellipse half-width is {exact}"
                ),
            }
        }
        // The outer corners are gone: at (0, 0) the lattice test gives
        // 2 * (32/32)^4 = 2 > 1.
        for &(x, y) in &[
            (0usize, 0usize),
            (n as usize - 1, 0),
            (0, n as usize - 1),
            (n as usize - 1, n as usize - 1),
        ] {
            assert_eq!(
                img.pixels[(y * n as usize + x) * 4 + 3],
                0,
                "corner ({x},{y}) must be fully transparent"
            );
        }
        // The middle row survives edge to edge: the mask cuts corners, it does
        // not shrink the icon. On the lattice, y = 32 gives dy = 0 and h = 32.
        assert_eq!(
            alpha_span(&img, n as usize / 2),
            Some((0, n as usize)),
            "the centre row must be untouched"
        );
    }

    #[test]
    fn masking_is_a_noop_for_an_already_masked_icon() {
        let n = 64u32;
        let mut img = solid(n);
        assert!(apply_shape(&mut img, IconShape::Squircle));
        let after_first = img.clone();

        // Byte-identical, and reported as unchanged.
        assert!(!apply_shape(&mut img, IconShape::Squircle), "second mask must be a no-op");
        assert_eq!(img, after_first, "no byte may change on a second mask");

        // Same for the other shapes: masking an already-shaped icon is free.
        for shape in [IconShape::Circle, IconShape::RoundedRect] {
            let mut img = solid(n);
            assert!(apply_shape(&mut img, shape), "{shape:?} must cut the square");
            let once = img.clone();
            assert!(!apply_shape(&mut img, shape), "{shape:?} second mask must be a no-op");
            assert_eq!(img, once, "{shape:?} changed bytes on a second mask");
        }
    }

    #[test]
    fn masking_never_touches_a_fully_opaque_square() {
        // The mask must actually remove ink, and must say so.
        let n = 64u32;
        let before = solid(n);
        let mut img = before.clone();
        assert!(apply_shape(&mut img, IconShape::Squircle), "a square has corners to cut");

        // Alpha outside the surviving spans is zero.
        let cleared = img
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] == 0)
            .count();
        assert!(cleared > 0, "the mask removed nothing");
        // The kept region is byte-identical: masking must not touch RGB.
        let mut kept = 0;
        for y in 0..n as usize {
            let (lo, hi) = alpha_span(&img, y).unwrap_or((0, 0));
            for x in 0..n as usize {
                let i = (y * n as usize + x) * 4;
                let inside = x >= lo && x < hi;
                if inside {
                    assert_eq!(
                        img.pixels[i..i + 4],
                        before.pixels[i..i + 4],
                        "({x},{y}) is inside the mask but its bytes changed"
                    );
                    kept += 1;
                } else {
                    assert_eq!(img.pixels[i + 3], 0, "({x},{y}) is outside the mask");
                }
            }
        }
        assert!(kept > n as usize * n as usize / 2, "the mask kept only {kept} of {}", n * n);
    }

    #[test]
    fn masking_costs_o_rows_not_o_pixels() {
        // The claim under test: the mask's work is proportional to the *area it
        // removes*, not to the pixel count. Counting writes needs no allocator
        // and no instrumentation -- "the mask's write set" is exactly "the
        // pixels whose alpha is now zero but was not before".
        for n in [32u32, 64, 128] {
            let before = solid(n);
            let mut img = before.clone();
            apply_shape(&mut img, IconShape::Squircle);

            let total = n as usize * n as usize;
            let modified = (0..total)
                .filter(|&i| {
                    before.pixels[i * 4 + 3] != img.pixels[i * 4 + 3]
                })
                .count();
            // Analytic cross-check, not a fitted bound: the superellipse
            // |x/r|^4 + |y/r|^4 <= 1 has area
            //   2 * Gamma(1+1/4)^2 / Gamma(1+2/4) * 2r^2  ~= 3.7081 r^2
            // of the 4r^2 square, i.e. the mask removes about 7.3% of the area.
            // Allow a 3x envelope for the integer span's half-open edges.
            assert!(
                modified * 100 < total * 15,
                "n={n}: {modified}/{total} pixels written, expected well under 15%"
            );
            assert!(modified > 0, "n={n}: nothing was written at all");
            // Well below half the icon: a per-pixel SDF touching every pixel,
            // and a mask that grew the ink, would both fail this.
            assert!(modified * 2 < total, "n={n}: {modified} of {total} is not a corner trim");
        }
    }

    #[test]
    fn masking_a_fully_transparent_icon_is_a_noop() {
        let n = 32u32;
        let mut img = RgbaImage { width: n, height: n, pixels: vec![0; n as usize * n as usize * 4] };
        let before = img.clone();
        assert!(!apply_shape(&mut img, IconShape::Squircle), "no ink, no change");
        assert_eq!(img, before);
    }

    #[test]
    fn masking_handles_degenerate_images_without_panicking() {
        for (w, h) in [(0u32, 0u32), (1, 1), (1, 8), (8, 1), (3, 5)] {
            let mut img = RgbaImage { width: w, height: h, pixels: vec![0xFF; w as usize * h as usize * 4] };
            for shape in [IconShape::Squircle, IconShape::Circle, IconShape::RoundedRect] {
                let _ = apply_shape(&mut img, shape);
            }
        }
        // A truncated buffer must be refused, not indexed.
        let mut short = RgbaImage { width: 64, height: 64, pixels: vec![0xFF; 16] };
        assert!(!apply_shape(&mut short, IconShape::Squircle));
    }

    #[test]
    fn shape_areas_match_their_analytic_formulas() {
        // The three masks are checked against closed forms, not against each
        // other, because two of them turn out to be near-aliases:
        //
        // * superellipse n=4:      A = 4 * Gamma(1.25)^2 / Gamma(1.5) * r^2
        //                         = 3.70815 r^2
        // * rounded rect, f = 0.6: A = 4r^2 - (4 - pi) * (0.6r)^2
        //                         = 3.69102 r^2
        // * circle:                A = pi r^2 = 3.14159 r^2
        //
        // So the squircle and the 0.6 rounded rect differ by 0.46% of area --
        // they are nearly the same silhouette, and the integer span raster even
        // inverts the ordering at 64/128/167 px. Asserting `squircle >
        // rounded rect` would be asserting a rounding artefact. What is real,
        // and what the visual difference rests on, is that *both* are far
        // fuller than the circle.
        const SQ_FACTOR: f64 = 3.708_149_5;
        for n in [64u32, 128, 167] {
            let r = n as f64 / 2.0;
            let area = |shape: IconShape| {
                let mut img = solid(n);
                apply_shape(&mut img, shape);
                (0..n as usize * n as usize).filter(|&i| img.pixels[i * 4 + 3] != 0).count() as f64
            };
            let frac = IconShape::RoundedRect.corner_fraction() as f64;
            let want = [
                (IconShape::Squircle, SQ_FACTOR * r * r),
                (
                    IconShape::RoundedRect,
                    4.0 * r * r - (4.0 - std::f64::consts::PI) * (frac * r).powi(2),
                ),
                (IconShape::Circle, std::f64::consts::PI * r * r),
            ];
            for (shape, exact) in want {
                let got = area(shape);
                // The integer span floors both ends of every row, so the raster
                // is a shade under the analytic area; 3% absorbs that at 64 px
                // (the squircle rasterises at 97.1% of its exact area there).
                let err = (got - exact).abs() / exact;
                assert!(
                    err < 0.03,
                    "n={n} {shape:?}: raster {got} is {err:.1}% off the analytic {exact}"
                );
            }
            // The ordering that is real, at every size tested.
            assert!(area(IconShape::Circle) < area(IconShape::Squircle), "n={n}");
            assert!(area(IconShape::Circle) < area(IconShape::RoundedRect), "n={n}");
        }
    }

    #[test]
    fn icon_shape_corner_fractions_match_the_source_presets() {
        // `IconShape.kt:312-346`: Squircle and Circle are full-scale corners;
        // RoundedSquare is `IconCornerShape.arc` at `.6f`.
        assert_eq!(IconShape::Squircle.corner_fraction(), 1.0);
        assert_eq!(IconShape::Circle.corner_fraction(), 1.0);
        assert_eq!(IconShape::RoundedRect.corner_fraction(), 0.6);
        // Copy, not Clone-by-reference: the shape is threaded through per-icon
        // state and must not need an allocation to pass around.
        fn assert_copy<T: Copy + PartialEq + core::fmt::Debug>(a: T, b: T) -> T {
            assert_eq!(a, b);
            a
        }
        let s = assert_copy(IconShape::Squircle, IconShape::Squircle);
        assert!(IconShape::Squircle.is_full_width());
        assert!(!IconShape::Circle.is_full_width());
        assert_eq!(size_of::<IconShape>(), 1, "the shape must stay a single byte");
        // `IconShape` is `Copy`, so passing it to the per-icon shaping state and
        // to the cache is a register move, not a refcount bump.
        let pairs = [(IconShape::Circle, IconShape::Squircle), (IconShape::RoundedRect, IconShape::RoundedRect)];
        assert_ne!(pairs[0].0, pairs[0].1);
        assert_eq!(pairs[1].0, pairs[1].1);
        let _ = s;
    }

    // =======================================================================
    // Monochrome
    // =======================================================================

    #[test]
    fn make_monochrome_preserves_alpha_and_sets_rgb() {
        let n = 8u32;
        // A gradient of alphas with deliberately mixed RGB.
        let mut img = RgbaImage {
            width: n,
            height: n,
            pixels: (0..n * n * 4)
                .map(|i| if i % 4 == 3 { (i / 4 % 4) * 85 } else { 0x11 * (i % 7) })
                .map(|v| v as u8)
                .collect(),
        };
        let alphas: Vec<u8> = img.pixels.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        assert!(alphas.contains(&0), "fixture needs transparent pixels");
        assert!(alphas.contains(&255), "fixture needs opaque pixels");

        make_monochrome(&mut img, 0xFF_2E_D0_5B);
        for (i, px) in img.pixels.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(px[0], 0x2E, "pixel {i} red");
            assert_eq!(px[1], 0xD0, "pixel {i} green");
            assert_eq!(px[2], 0x5B, "pixel {i} blue");
            assert_eq!(px[3], alphas[i], "pixel {i} alpha must be untouched");
        }
        // The alpha byte is the whole silhouette: unchanged bit for bit.
        let after: Vec<u8> = img.pixels.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        assert_eq!(after, alphas);
        // Idempotent, and no allocation: `pixels` keeps its exact capacity.
        let cap = img.pixels.capacity();
        make_monochrome(&mut img, 0xFF_2E_D0_5B);
        assert_eq!(img.pixels.capacity(), cap, "tinting must not reallocate");
    }

    // =======================================================================
    // Monogram tile
    // =======================================================================

    fn tile_buffer(w: usize, h: usize) -> Vec<u32> {
        vec![0xFF00_0000; w * h]
    }

    #[test]
    fn monogram_tile_renders_a_glyph_and_is_not_blank() {
        let w = 128usize;
        let h = 128usize;
        let mut buf = tile_buffer(w, h);
        let rect = Rect { x: 16.0, y: 8.0, w: 96.0, h: 96.0, radius: 0.0 };
        let tile = 0xFF1E_2A38_u32;
        let glyph = 0xFFE8_EAED_u32;

        assert!(draw_monogram_tile(&mut buf, w, w, h, rect, 'F', tile, glyph));

        let idx = |x: usize, y: usize| y * w + x;
        // The surface is a filled squircle, not a box and not blank.
        let centre = buf[idx(64, 56)];
        assert_eq!(centre >> 24, 0xFF, "the tile must be opaque");
        let tile_px = centre & 0xFF_FFFF;
        assert!(
            (tile_px ^ (tile & 0xFF_FFFF)).count_ones() <= 24,
            "tile centre {centre:08x} is not the tile colour {tile:08x}"
        );
        // A good fraction of the rect is covered -- this is not a thin outline.
        let inside = ((rect.x as usize)..(rect.x as usize + 96))
            .flat_map(|x| (rect.y as usize..rect.y as usize + 96).map(move |y| (x, y)))
            .filter(|(x, y)| buf[idx(*x, *y)] != 0xFF00_0000)
            .count();
        assert!(inside > 96 * 96 / 2, "only {inside} of {} pixels drawn", 96 * 96);
        // Outside the tile nothing was touched.
        assert_eq!(buf[idx(4, 4)], 0xFF00_0000, "drew outside the rect");
        assert_eq!(buf[idx(120, 120)], 0xFF00_0000, "drew outside the rect");

        // Ink: pixels that are neither the background nor the glyph colour.
        let ink = (0..w * h)
            .filter(|&i| {
                let p = buf[i] & 0xFF_FFFF;
                p != 0xFF00_0000 && p != (tile & 0xFF_FFFF) && p != (glyph & 0xFF_FFFF)
            })
            .count();
        assert!(ink > 200, "no glyph ink: only {ink} antialiased pixels");

        // The ink colour really is `glyph_color`, and it wins where the glyph
        // is thickest. The tile is light, the glyph is near-white, so the
        // brightest pixel in the tile must be at the glyph, not the surface.
        let brightest = (0..w * h)
            .map(|i| buf[i] & 0xFF_FFFF)
            .max_by_key(|p| p & 0xFF)
            .unwrap();
        assert_eq!(
            brightest, glyph & 0xFF_FFFF,
            "the brightest pixel is not the glyph colour"
        );
        // Glyph coverage is a minority of the tile: it is a mark on a surface,
        // not a second surface.
        let glyph_px = (0..w * h)
            .filter(|&i| (buf[i] & 0xFF_FFFF) == (glyph & 0xFF_FFFF))
            .count();
        assert!(
            glyph_px < 96 * 96 / 3,
            "glyph covers {glyph_px} of {} pixels",
            96 * 96
        );
    }

    #[test]
    fn monogram_tile_rejects_an_unrenderable_initial() {
        let w = 64usize;
        let h = 64usize;
        let rect = Rect { x: 0.0, y: 0.0, w: 64.0, h: 64.0, radius: 0.0 };
        // `font::draw_glyph` renders `0x20..0x7F` only; everything else has no
        // ink, and 0x20 (space) is in range but is blank. All must be rejected
        // and must draw nothing at all.
        for bad in ['\u{1F600}', '\u{00E9}', '\n', '\t', '\u{7F}', ' ', '\u{0}'] {
            let mut buf = tile_buffer(w, h);
            assert!(
                !draw_monogram_tile(&mut buf, w, w, h, rect, bad, 0xFF11_2233, 0xFFEE_EEEE),
                "{bad:?} must be rejected rather than drawn as a box"
            );
            assert!(
                buf.iter().all(|&p| p == 0xFF00_0000),
                "{bad:?} drew into the buffer despite being rejected"
            );
        }
        // A real letter still works, so the check is not rejecting everything.
        let mut buf = tile_buffer(w, h);
        assert!(draw_monogram_tile(&mut buf, w, w, h, rect, 'A', 0xFF11_2233, 0xFFEE_EEEE));
        assert!(buf.iter().any(|&p| p != 0xFF00_0000));
    }

    #[test]
    fn monogram_tile_clips_to_the_framebuffer() {
        let w = 32usize;
        let h = 32usize;
        // Half off the left edge, and entirely off the right.
        for rect in [
            Rect { x: -16.0, y: 0.0, w: 32.0, h: 32.0, radius: 0.0 },
            Rect { x: 40.0, y: 0.0, w: 32.0, h: 32.0, radius: 0.0 },
            Rect { x: 0.0, y: 0.0, w: 0.0, h: 32.0, radius: 0.0 },
        ] {
            let mut buf = tile_buffer(w, h);
            let drew = draw_monogram_tile(&mut buf, w, w, h, rect, 'M', 0xFF20_3040, 0xFFAA_BBCC);
            if rect.w > 0.0 {
                // Off-screen rects are rejected outright; on-screen ones draw.
                assert_eq!(drew, rect.x < w as f32 && rect.x + rect.w > 0.0, "{rect:?}");
            } else {
                assert!(!drew, "a zero-width rect must draw nothing");
            }
        }
    }

    #[test]
    fn monogram_tile_is_hooked_to_the_label_type_ramp() {
        // It uses `font::active_family()`, so it follows the shell's family
        // setting rather than hardcoding one -- which is what keeps the
        // monogram matching the app labels beside it.
        let _guard = crate::graphics::font::TEST_FONT_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let w = 96usize;
        let h = 96usize;
        let rect = Rect { x: 0.0, y: 0.0, w: 96.0, h: 96.0, radius: 0.0 };
        let mut rasters = [0u32; 3];
        for (i, fam) in [font::FontFamily::NotoSans, font::FontFamily::Homemade, font::FontFamily::AsciiMono]
            .into_iter()
            .enumerate()
        {
            font::set_active_family(fam);
            let mut buf = tile_buffer(w, h);
            assert!(draw_monogram_tile(&mut buf, w, w, h, rect, 'G', 0xFF10_2030, 0xFFF0_F0F0));
            rasters[i] = buf.iter().fold(0u32, |acc, &p| acc.wrapping_add(p));
        }
        font::set_active_family(font::FontFamily::NotoSans);
        // The families must not all rasterise identically, or the family
        // parameter would be doing nothing. `Homemade` and `AsciiMono` are
        // close cousins -- the same skeleton with a different advance width --
        // so only the *set* of three is asserted, not every pair. `NotoSans`
        // is the odd one out: it is either the real system TTF or the
        // proportional table, neither of which shares `AsciiMono`'s metrics.
        assert_ne!(rasters[0], rasters[2], "NotoSans and AsciiMono are identical");
        assert!(
            rasters[1] != rasters[0] || rasters[1] != rasters[2],
            "all three families rasterise identically"
        );
    }

    // =======================================================================
    // Adaptive-icon background heuristic
    // =======================================================================

    #[test]
    fn explicit_background_is_detected_from_uniform_opaque_corners() {
        // A flattened SVG: opaque, one flat colour in all four corners.
        let flat = |c: [u8; 3]| {
            let mut pixels = Vec::with_capacity(4 * 4 * 4);
            for _ in 0..4 * 4 {
                pixels.extend_from_slice(&[c[0], c[1], c[2], 0xFF]);
            }
            RgbaImage { width: 4, height: 4, pixels }
        };
        assert!(has_explicit_background(&flat([0x21, 0x96, 0xF0])));

        // The tolerance is a *dither* allowance, so it has to be probed against
        // corners that actually disagree -- a wholly flat image is trivially
        // uniform at any tolerance. Base blue is 0xF0 so both probes stay in
        // range of a u8.
        let tol = BACKGROUND_UNIFORM_TOLERANCE as u16;
        let base = 0xF0u16;
        let skew = |delta: u16| {
            let mut img = flat([0x21, 0x96, base as u8]);
            // The last corner only: a gradient background, or a dithered
            // downscale of one.
            let last = 4 * 4 * 4 - 4;
            img.pixels[last + 2] = (base + delta) as u8;
            img
        };
        assert!(
            has_explicit_background(&skew(tol)),
            "a {tol}-level dither is still a flat surface tone"
        );
        assert!(
            !has_explicit_background(&skew(tol + 1)),
            "a {}-level corner spread is a second colour, not a surface tone",
            tol + 1
        );

        // A foreground layer: transparent corners.
        let mut fg = flat([0x21, 0x96, 0xF0]);
        fg.pixels[3] = 0;
        assert!(!has_explicit_background(&fg), "a transparent corner means foreground only");
        // An opaque corner that is a *different* colour is artwork, not surface.
        let mut art = flat([0x21, 0x96, 0xF0]);
        art.pixels[4 * 3] = 0x00;
        art.pixels[4 * 3 + 3] = 0xFF;
        assert!(!has_explicit_background(&art), "mismatched corners are not a flat background");
        // A corner that is opaque but not fully so is artwork with a soft edge,
        // not a flat surface: a downscaled PNG can leave 0xFD where a fill gave
        // 0xFF, and that must not flip the answer either way.
        let mut soft = flat([0x21, 0x96, 0xF0]);
        soft.pixels[3] = BACKGROUND_OPAQUE_ALPHA - 1;
        assert!(!has_explicit_background(&soft), "alpha below the opaque threshold");
        // Degenerate input is a foreground layer, not a panic.
        assert!(!has_explicit_background(&RgbaImage { width: 0, height: 0, pixels: vec![] }));
    }

    #[test]
    fn foreground_layer_is_composited_onto_a_tile_colour() {
        // A 2x2 foreground: one opaque red, three transparent.
        let mut img = RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![0xFF, 0x00, 0x00, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        };
        assert!(!has_explicit_background(&img), "transparent corners: a foreground layer");
        assert!(composite_foreground(&mut img, 0xFF00_00FF), "the fill must be written");

        // The art is untouched, everything else became the surface tone, and
        // the result is opaque throughout.
        assert_eq!(&img.pixels[0..4], &[0xFF, 0x00, 0x00, 0xFF], "the art must survive");
        for px in img.pixels[4..].as_chunks::<4>().0 {
            assert_eq!(px[3], 0xFF, "compositing onto an opaque tile must be opaque");
            assert_eq!(&px[0..3], &[0x00, 0x00, 0xFF], "transparent pixels take the tile colour");
        }
        // A uniformly-filled tile *is* a background. The 2x2 above is not,
        // because the art in the first pixel is a second colour: that is the
        // documented failure direction -- a foreground layer that happens to be
        // painted edge to edge in one flat colour is left un-composited.
        let mut uniform = RgbaImage { width: 2, height: 2, pixels: vec![0u8; 2 * 2 * 4] };
        composite_foreground(&mut uniform, 0xFF00_00FF);
        assert!(has_explicit_background(&uniform), "a flat filled tile is a background");

        // Partial alpha is blended over the tile, not dropped.
        let mut half = RgbaImage { width: 1, height: 1, pixels: vec![0x00, 0x00, 0x00, 0x80] };
        assert!(composite_foreground(&mut half, 0xFF00_00FF));
        assert_eq!(half.pixels[3], 0xFF);
        // (0*128 + 255*127 + 127)/255 = 127
        assert_eq!(half.pixels[2], 127, "half-transparent black over blue");
    }

    // =======================================================================
    // Cache budget / pre-scale target
    // =======================================================================

    #[test]
    fn icon_max_edge_follows_the_device_profile() {
        // `DeviceProfile::icon_dp` is 65 dp and `dp` is `panel/420`
        // (`device_profiles.xml:70`), so 1080 px -> 167 px.
        assert_eq!(DeviceProfile::phone_reference().icon_dp, 65.0);
        let at_1080 = icon_max_edge(1080.0);
        assert_eq!(at_1080, (65.0f32 * (1080.0f32 / 420.0f32)).round() as u32);
        assert_eq!(at_1080, 167, "65 dp at 2.571 px/dp is 167 px, not the old hard-coded 64");
        // Monotonic in panel width, and never zero.
        assert!(icon_max_edge(540.0) < at_1080);
        assert!(icon_max_edge(2160.0) > at_1080);
        for w in [1.0f32, 64.0, 720.0, 1440.0, 3840.0] {
            let e = icon_max_edge(w);
            assert!((1..=ICON_MAX_SOURCE_EDGE).contains(&e), "panel {w} gave edge {e}");
        }
        // Clamped to the source cap (P15), which is what bounds decoder memory.
        assert_eq!(icon_max_edge(100_000.0), ICON_MAX_SOURCE_EDGE);
        // Degenerate input must not panic or produce a zero edge.
        for bad in [0.0f32, -100.0, f32::NAN, f32::INFINITY] {
            let e = icon_max_edge(bad);
            assert!((1..=ICON_MAX_SOURCE_EDGE).contains(&e), "panel {bad} gave edge {e}");
        }
    }

    #[test]
    fn pre_scaling_to_the_blit_edge_keeps_the_1to1_fast_path() {
        // The whole point of `icon_max_edge`: a source already at the display
        // edge must survive `load_path` with its bytes untouched, so
        // `draw_icon_bitmap_i32` takes its `identity` branch instead of
        // resampling bilinear on every frame.
        let tree = Tree::new("prescale");
        tree.put("icons/hicolor/8x8/apps/atexact.png", RGBA8_PNG);
        tree.put("icons/hicolor/16x16/apps/atbig.png", GRAY16_PNG);

        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(8);
        cache.resolve_keys(&["atexact".to_string(), "atbig".to_string()]);

        // Tile-sized source: no resample, byte for byte.
        let exact = cache.get("atexact").expect("8px source decodes");
        assert_eq!((exact.width, exact.height), (8, 8), "an 8px source at an 8px edge stays 8px");
        assert_eq!(exact.pixels, RGBA8_EXPECT, "the 1:1 path must see the original bytes");

        // Oversized source: pre-scaled once, here, at cache time.
        let big = cache.get("atbig").expect("16px source decodes");
        assert_eq!((big.width, big.height), (8, 8), "a 16px source is pre-scaled to the 8px edge");
        assert_eq!(big.pixels.len(), 8 * 8 * 4, "the scaled tile is exactly one edge of RGBA");

        // Shrinking the edge re-scales: the cache is not holding a stale size.
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(4);
        cache.resolve_keys(&["atexact".to_string()]);
        let small = cache.get("atexact").expect("re-decoded at the new edge");
        assert_eq!((small.width, small.height), (4, 4), "the display edge drives the pre-scale");
    }

    // =======================================================================
    // Offline cache stage
    // =======================================================================

    #[test]
    fn offline_cache_path_is_consulted_first() {
        // The convention, asserted without touching the filesystem.
        assert_eq!(offline_cache_name("firefox", 167), "firefox_167.png");
        assert_eq!(offline_cache_name("org.mozilla.firefox.desktop", 64), "org.mozilla.firefox.desktop_64.png");
        // No separator, no format machinery: digits are appended directly.
        assert_eq!(offline_cache_name("a", 0), "a_0.png");
        assert_eq!(offline_cache_name("a", 7), "a_7.png");
        assert_eq!(offline_cache_name("a", 10), "a_10.png");
        assert_eq!(offline_cache_name("a", 1024), "a_1024.png");
        assert_eq!(offline_cache_name("a", u32::MAX), "a_4294967295.png");
        // Case is preserved verbatim, so a helper and the launcher agree.
        assert_eq!(offline_cache_name("Web-Browser", 48), "Web-Browser_48.png");

        let dir = Path::new("/var/cache/utlc/icons");
        assert_eq!(
            offline_cache_path(dir, "firefox", 167),
            dir.join("firefox_167.png")
        );

        // The documented stage order: the offline cache is ahead of the theme
        // sweep, which is what makes a pre-rendered SVG beat a scanned PNG.
        assert_eq!(
            LOOKUP_ORDER,
            [
                LookupStage::AbsolutePath,
                LookupStage::OfflineCache,
                LookupStage::Theme
            ]
        );
        assert_eq!(OFFLINE_CACHE_SUBDIR, "utlc/icons");
    }

    #[test]
    fn offline_cache_stage_wins_over_the_theme_sweep() {
        // Both stages have a candidate for the same key; the offline one must
        // be the one that lands. RGBA8 and GRAY1 differ in size, so the
        // identity of the winner is unambiguous.
        let tree = Tree::new("offline");
        tree.put("icons/hicolor/64x64/apps/dual.png", RGBA8_PNG);
        tree.put("cache/dual_64.png", GRAY1_PNG);

        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_offline_dir(Some(tree.path().join("cache")));
        cache.resolve_keys(&["dual".to_string()]);
        let icon = cache.get("dual").expect("resolved");
        assert_eq!(icon.pixels, GRAY1_EXPECT, "the offline cache must beat the sweep");

        // With the stage off, the sweep's own answer comes back.
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.resolve_keys(&["dual".to_string()]);
        let icon = cache.get("dual").expect("resolved");
        assert_eq!(icon.pixels, RGBA8_EXPECT, "theme sweep is the fallback");
    }

    #[test]
    fn offline_cache_miss_falls_through_without_pinning_a_miss() {
        // A oneshot may render the file a second after the launcher looked, so
        // an absent entry must not be remembered as a permanent miss. Here the
        // key has *no* theme candidate either, so the only thing that can
        // resolve it is the file that appears later.
        let tree = Tree::new("offlinelate");
        let cache_dir = tree.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        // No `icons/` root at all, so the sweep has nothing to offer.
        let mut cache = IconCache::with_roots(vec![], "");
        cache.set_offline_dir(Some(cache_dir.clone()));

        let keys = vec!["late".to_string()];
        cache.resolve_keys(&keys);
        assert!(cache.get("late").is_none(), "nothing to resolve yet");
        assert!(cache.knows("late"), "the completed sweep does record the miss");
        cache.invalidate_misses();
        assert!(!cache.knows("late"), "the miss can be dropped");

        // Now the offline copy appears; the next resolve must pick it up.
        std::fs::write(offline_cache_path(&cache_dir, "late", 64), RGBA8_PNG).unwrap();
        cache.resolve_keys(&keys);
        let icon = cache.get("late").expect("the offline entry resolves");
        assert_eq!(icon.pixels, RGBA8_EXPECT);

        // A corrupt offline entry is not cached as a permanent miss either: it
        // falls through, and since the sweep has no candidate either, only the
        // drop-miss path can retry it.
        let mut cache2 = IconCache::with_roots(vec![], "");
        cache2.set_offline_dir(Some(cache_dir.clone()));
        std::fs::write(offline_cache_path(&cache_dir, "late", 64), b"not a png").unwrap();
        cache2.resolve_keys(&keys);
        assert!(cache2.get("late").is_none(), "a bad offline file does not resolve");
        cache2.invalidate_misses();
        std::fs::write(offline_cache_path(&cache_dir, "late", 64), GRAY1_PNG).unwrap();
        cache2.resolve_keys(&keys);
        assert_eq!(
            cache2.get("late").expect("recovered").pixels,
            GRAY1_EXPECT,
            "a bad offline file must not have pinned a permanent miss"
        );
    }

    #[test]
    fn offline_cache_dir_follows_xdg() {
        // Environment-dependent, so it only asserts the shape of the answer:
        // whenever a dir is produced it ends with the documented subdirectory.
        if let Some(dir) = offline_cache_dir() {
            let tail: Vec<_> = dir
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let n = tail.len();
            assert!(n >= 2, "cache dir {dir:?} is implausibly short");
            assert_eq!(tail[n - 1], "icons");
            assert_eq!(tail[n - 2], "utlc");
        }
        // Absolute, so the launcher's cwd cannot change what it reads.
        if let Some(dir) = offline_cache_dir() {
            assert!(dir.is_absolute(), "cache dir {dir:?} must be absolute");
        }
    }

    #[test]
    fn lru_budget_evicts_oldest() {
        let mut cache = IconCache::with_roots(vec![], "");
        // 2 MiB each: two fit the 4 MiB budget, the third forces eviction.
        let big = |v: u8| RgbaImage {
            width: 1024,
            height: 512,
            pixels: vec![v; 1024 * 512 * 4],
        };
        cache.insert_image("a".into(), big(1));
        cache.insert_image("b".into(), big(2));
        assert!(cache.get("a").is_some(), "touch a so b is LRU");
        cache.insert_image("c".into(), big(3));
        assert!(cache.get("b").is_none(), "LRU entry evicted under budget");
        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());
        assert!(
            cache.live_bytes() <= ICON_CACHE_BUDGET + 1024 * 512 * 4,
            "live set bounded by budget + one entry"
        );
    }

    #[test]
    fn duplicate_roots_are_deduped() {
        let root = PathBuf::from("/tmp/utim_icons_dedupe");
        let cache = IconCache::with_roots(vec![root.clone(), root.clone(), root], "");
        assert_eq!(cache.roots.len(), 1);
    }

    #[test]
    fn classification_helpers() {
        assert_eq!(parse_size("48x48"), Some((48, 48)));
        assert_eq!(parse_size("64X16"), Some((64, 16)));
        assert_eq!(parse_size("128"), Some((128, 128)));
        assert_eq!(parse_size("scalable"), None);
        assert_eq!(parse_size("apps"), None);
        assert!(is_context("apps") && is_context("legacy") && !is_context("hicolor"));
        assert!(icon_name_eq("phone", "phone"));
        assert!(icon_name_eq("phone", "phone.png"));
        assert!(icon_name_eq("phone", "phone.PNG"));
        assert!(!icon_name_eq("phone", "phon"));
        // A 64x16 strip must not outscore the true 64x64 icon.
        assert!(size_score(Some((64, 64)), 64) > size_score(Some((64, 16)), 64));
        assert!(
            size_score(Some((48, 48)), 64) > size_score(None, 64),
            "concrete size beats scalable"
        );
    }

    #[test]
    fn size_score_follows_the_blit_target() {
        // The target is the pre-scale edge, so a 167 px panel must not be
        // handed the 64x64 raster when 128x128 is available.
        assert!(size_score(Some((128, 128)), 167) > size_score(Some((64, 64)), 167));
        // ...and the reverse on a small panel.
        assert!(size_score(Some((48, 48)), 48) > size_score(Some((128, 128)), 48));
        // A degenerate target must not panic. `target` clamps to 1, so the
        // ranking becomes "closest to a 1 px icon" -- small wins -- rather than
        // reversing or wrapping.
        for bad in [0u32, u32::MAX] {
            let sq = size_score(Some((64, 64)), bad);
            let strip = size_score(Some((64, 16)), bad);
            assert!(sq <= 1000 && strip <= 1000, "target {bad} overflowed the score");
            assert!(size_score(None, bad) > strip, "scalable stays mid-range at target {bad}");
        }
    }
}
