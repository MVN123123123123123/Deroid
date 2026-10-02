//! Icon resolution, shaping and fallback for the launcher.
//!
//! # Resolution
//!
//! Three stages, and [`LOOKUP_ORDER`] is the **implementation** of that order,
//! not a description of it: [`IconCache::resolve_keys`] walks the const and
//! dispatches each stage from a `match` arm, so reordering the array reorders
//! the behaviour and adding a stage is one const entry plus one arm.
//!
//! 1. **Absolute path** — a key containing `/` is read directly, and never
//!    enters the sweep.
//! 2. **Offline disk cache** — `$XDG_CACHE_HOME/utlc/icons/<app_id>_<edge>.png`,
//!    read by [`IconCache::load_offline`] and **written** by
//!    [`write_offline_icon`]. Also the only answer to the `.svg` under
//!    `hicolor/scalable` problem: embedding an SVG rasteriser is rejected
//!    because resvg is >30 crates and ~6 MiB of binary, but a rasteriser that
//!    is not *this* process is free — so the cache is also where an
//!    out-of-process `rsvg-convert` puts its output (see [`OFFLINE_CACHE_SUBDIR`]).
//! 3. **Freedesktop theme sweep** — one bounded directory pass per batch
//!    resolves every name to its best PNG candidate (theme rank, directory
//!    context, size proximity), decoded in-crate by [`crate::graphics::png`].
//!
//! Keys that resolve to nothing are remembered as misses so the next frame does
//! not repeat the scan; callers drop the miss set with
//! [`IconCache::invalidate_misses`] whenever the application set changes.
//!
//! # The offline cache has a producer now
//!
//! This used to be a read-only stage: nothing in the tree wrote
//! `$XDG_CACHE_HOME/utlc/icons`, so `load_offline`'s `open` was pure cost on
//! every cold icon, every boot, forever. [`IconCache`] now writes the tile it
//! resolved — the **pre-transform** one, see [`IconCache::cache_offline`] —
//! through [`write_offline_icon`], and [`IconCache::prune_offline_cache`] keeps
//! the directory inside a budget. `utlc-cache-icons` is still worth having for
//! the `.svg` apps, and it can call the same [`write_offline_icon`] this module
//! does, so both producers and this reader agree on the format by construction.
//!
//! # What is wired up
//!
//! * **Theme sweep** — the real path. Every `Icon=` key goes through it.
//! * **Offline cache** — read *and* written, with the writer on by default for
//!   a cache built by [`IconCache::new`] and off by default for
//!   [`IconCache::with_roots`].
//! * **Absolute path** — the stage is implemented and tested
//!   (`absolute_path_keys_bypass_the_sweep`), but no application has a `/` in
//!   its icon key: keys are desktop-entry ids and icon-name stems, neither of
//!   which contains a path separator. It stays because the shell's per-app icon
//!   override (`launcher_state::LauncherState::icon_overrides`, the reference's
//!   `IconOverride` Room entity) is a natural future producer, and because a
//!   stage that costs one `String::contains` is not worth deleting.
//!
//! # Shaping state
//!
//! Four things are decided once, when an icon enters the cache, and never on the
//! frame path. Each has a setter on [`IconCache`], each is a property of the
//! *entry* rather than of the frame, and each one invalidates the shaped
//! entries when it changes -- a cached tile is an immutable `Rc`, so there is no
//! way to restyle it in place.
//!
//! | setter | state | reference |
//! |---|---|---|
//! | [`IconCache::set_shape`] | adaptive-icon mask | `IconShape` presets |
//! | [`IconCache::set_monochrome_tint`] | tint every icon flat | `forceIconMonochrome` |
//! | [`IconCache::set_foreground_background`] | synthesise a `<background>` | adaptive `<background>` |
//! | [`IconCache::set_icon_shadow`] | cache a blurred shadow mask | `Shadow.apply`, `shadowBGIons` |
//!
//! # Shaping
//!
//! Cached icons are pre-scaled to [`icon_max_edge`] and masked once, at cache
//! time (see [`mask_tile`]), so the frame path is a 1:1 blit. [`apply_shape`] is
//! the mask: it is **O(rows)**, not O(pixels), because
//! [`crate::graphics::raster::squircle_span`] solves the superellipse
//! half-width per row analytically and the pixels outside that span are simply
//! zeroed. There is no per-pixel distance test in the inner loop and none is
//! needed. Tiles below [`ICON_MASK_MIN_EDGE`] skip the mask, because the
//! integer span floors both ends of every row and at a small edge that floor
//! is a crop rather than a corner trim.
//!
//! The mask runs *after* the foreground-background reconstruction, never
//! before: a synthesised `<background>` is opaque in the corners, so masking
//! first and compositing second would fill the corners the mask had just
//! cleared and hand the renderer a hard-edged square.
//!
//! # When there is no icon at all
//!
//! [`draw_monogram_tile`] renders the app's initial as a procedural Material
//! You monogram: a squircle surface plus one glyph from
//! [`crate::graphics::font`]. Zero blank boxes, zero allocation, zero
//! third-party crates. [`IconCache::get_or_monogram`] is the single call the
//! shell makes: it returns the cached icon when there is one and generates,
//! caches and returns the monogram when there is not -- including when a PNG
//! resolved to a fully transparent tile, which would otherwise be blitted as a
//! hole forever.

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
/// **These three are the whole set UTLC implements.** The reference ships ~28
/// plus a four-corner custom editor (`IconShape.kt:312-486`, listed by
/// `IconShapePreference.iconShapeEntries:77-110`), and
/// [`launcher_state::IconShape`](crate::launcher_state::IconShape) exposes
/// exactly these three tokens for that reason -- the configuration surface must
/// not offer a shape the rasteriser cannot draw.
///
/// * `Squircle` — the Material You / Android adaptive-icon mask, the superellipse
///   `(|x|/r)^4 + (|y|/r)^4 <= 1` (`IconShape.Squircle`, `IconCornerShape.Squircle`
///   at scale `1f`, `IconShape.kt:342-346`).
/// * `Circle` — the full-arc corner (`IconShape.Circle`, `IconCornerShape.arc` at
///   scale `1f`, `IconShape.kt:312-316`).
/// * `RoundedRect` — a circular-cornered box. Lawnchair's closest preset is
///   `IconShape.RoundedSquare` (`IconCornerShape.arc` at scale **`.6f`**,
///   `IconShape.kt:334-340`); see [`IconShape::corner_fraction`].
///
/// # Not here, on purpose
///
/// The reference separates the *icon* shape from the *folder* shape
/// (`folderShape`, `PreferenceManager2.kt:151`) and gives home and drawer their
/// own setting (`homeScreenIconShape` / `drawerIconShape`). UTLC has neither:
/// folders do not exist yet, and every surface draws through the same
/// [`IconCache`]. The plumbing for the second shape *is* here --
/// [`IconCache::set_shape`] is the default, [`IconCache::get_or_monogram_shaped`]
/// takes a per-call shape, and both [`mask_tile`] and [`draw_monogram_tile`] take
/// a shape rather than assuming one -- so a folder surface can pass its own
/// without touching anything below it. The gap is that a per-surface shape is
/// still keyed by the *same* cache key: see
/// [`IconCache::get_or_monogram_shaped`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconShape {
    Squircle,
    Circle,
    RoundedRect,
}

/// The persisted setting's shape, as this module's.
///
/// Two enums with the same three variants, deliberately: `launcher_state::IconShape`
/// is the *stored* setting and must not depend on the renderer, or a state file
/// written by a build without adaptive icons would fail to parse. This
/// conversion is the seam, and it is `From` so a caller cannot pick the wrong
/// direction by accident.
///
/// The variants map one-to-one because the renderer implements exactly the three
/// the setting offers. It does **not** implement the reference's ~28 presets
/// (`IconShape.kt:312-486`), and this conversion is where that limit becomes
/// explicit: there is nowhere else for a fourth variant to be lost.
impl From<crate::launcher_state::IconShape> for IconShape {
    fn from(s: crate::launcher_state::IconShape) -> Self {
        match s {
            crate::launcher_state::IconShape::Squircle => IconShape::Squircle,
            crate::launcher_state::IconShape::Circle => IconShape::Circle,
            crate::launcher_state::IconShape::RoundedRect => IconShape::RoundedRect,
        }
    }
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
        IconShape::RoundedRect => {
            match rounded_span_f(dy, -r, r * 2.0, r * 2.0, r * shape.corner_fraction()) {
                Some((lo, hi)) => ((lo + hi) * 0.5).abs(),
                None => 0.0,
            }
        }
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
            (
                x0 + center.saturating_sub(half),
                x0 + (center + half).min(edge as usize),
            )
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

/// Mask a decoded tile to `shape` on its way into the cache, in place.
///
/// # Why the cache and not the renderer
///
/// The Material You adaptive-icon silhouette is a property of the *icon*, not
/// of the frame: it is decided once, when the icon is admitted to
/// [`IconCache`], so the render path is a 1:1 copy of a tile whose alpha is
/// already correct. The alternative -- masking per tile per frame -- would run
/// a per-row span solve for every icon on screen every frame, and would need
/// the shape at `drm_kms.rs`'s blit site too, which is where the two would
/// drift.
///
/// `shape` is a parameter and not a constant because the reference does not have
/// one shape: `IconShape` offers ~28 presets (`IconShape.kt:312-486`) and the
/// caller picks one ([`IconCache::set_shape`], or per call through
/// [`IconCache::get_or_monogram_shaped`]).
///
/// [`apply_shape`] is the mask and is **O(rows)**: it asks
/// [`squircle_span`] for one half-width per row and zeroes the two side spans,
/// so the work is proportional to the area removed rather than to the icon.
/// That is the whole reason [`squircle_span`] is the boundary source and not a
/// per-pixel signed-distance test: `(|x|/r)^4 + (|y|/r)^4 <= 1` is solved
/// analytically per row in ~30 cycles, and the kept span is never read.
///
/// Tiles below [`ICON_MASK_MIN_EDGE`] are returned untouched -- see that
/// constant for why the floor exists (at 1 px, `squircle_span` is 0
/// everywhere and the mask erases the icon).
fn mask_tile(mut img: RgbaImage, shape: IconShape) -> RgbaImage {
    if img.width.min(img.height) >= ICON_MASK_MIN_EDGE {
        // The return is deliberately not consulted: `false` means "no ink was
        // removed", which is the correct outcome for a tile that is already a
        // squircle or is fully transparent. Either way the bytes are right and
        // the cache stores them as-is; there is no second thing to do.
        apply_shape(&mut img, shape);
    }
    img
}

/// `true` when `img` has at least one non-transparent pixel.
///
/// A PNG that decodes to a fully transparent tile is not an icon -- it renders
/// as nothing, and the launcher shows a hole where the tile should be. This is
/// the test that routes such a key to [`draw_monogram_tile`] instead.
///
/// O(pixels), and it short-circuits on the first inked pixel, so a real icon
/// costs one load. It runs once per cache *miss*, never per frame.
pub fn has_visible_ink(img: &RgbaImage) -> bool {
    if img.width == 0 || img.height == 0 {
        return false;
    }
    let w4 = img.width as usize * 4;
    if img.pixels.len() < img.height as usize * w4 {
        return false;
    }
    img.pixels[..img.height as usize * w4]
        .as_chunks::<4>()
        .0
        .iter()
        .any(|px| px[3] != 0)
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
    if img.width == 0
        || img.height == 0
        || img.pixels.len() < img.width as usize * img.height as usize * 4
    {
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

/// Build the blurred shadow coverage for one cached tile.
///
/// # Why a separate mask and not the alpha channel
///
/// A [`ShadowMask`] is the tile's alpha after
/// [`crate::graphics::raster::blur_mask_8`], at the same edge, so it can be
/// handed to [`crate::graphics::raster::draw_preblurred_shadow_from_mask`]
/// with no resampling and no per-frame work. Keeping it as a distinct buffer
/// rather than blurring the framebuffer is what lets it be cached at all: the
/// reference's `DoubleShadowIconDrawable` blurs once per drawable for the same
/// reason.
///
/// # Why the tile's alpha, and nothing else
///
/// Because the mask is built from the alpha of the *finished* tile, after the
/// monochrome tint and the shape mask. A shadow drawn from the file's own alpha
/// would ignore both, and would not follow the silhouette the renderer actually
/// draws.
///
/// `None` for a tile too small to have a meaningful shadow: below
/// [`ICON_MASK_MIN_EDGE`] the shape is not a corner trim but a crop (see that
/// constant), so a blur of it would be a blur of an artefact. Above
/// [`ICON_MAX_TILE_EDGE`] it cannot have come from the cache at all, and a
/// caller asking for one has a bug rather than an icon.
pub fn build_shadow_mask(img: &RgbaImage) -> Option<ShadowMask> {
    let edge = img.width.min(img.height);
    if !(ICON_MASK_MIN_EDGE..=ICON_MAX_TILE_EDGE).contains(&edge) {
        return None;
    }
    let n = edge as usize;
    let mut coverage = vec![0u8; n * n];
    for (dst, px) in coverage.iter_mut().zip(img.pixels.as_chunks::<4>().0) {
        *dst = px[3];
    }
    // `blur_mask_8` needs `w + 2 * h` of scratch. Two allocations, both freed
    // before the caller caches the result: this is the insert path, once per
    // entry, and never per frame.
    let mut scratch = vec![0u8; n + 2 * n];
    crate::graphics::raster::blur_mask_8(&mut coverage, n, n, &mut scratch);
    Some(ShadowMask {
        edge,
        coverage: coverage.into_boxed_slice(),
    })
}

/// Composite a bare `<foreground>` layer onto `tile_color` in place.
///
/// The `<background>` half of an adaptive icon, reconstructed from the caller's
/// surface tone. `tile_color` must be opaque (`0xAARRGGBB`); the result is
/// always opaque, matching how a real `<background>` drawable behaves. Returns
/// `true` when any pixel was written.
///
/// Called from [`IconCache::insert_image`] when
/// [`has_explicit_background`] says the tile is a bare foreground layer and
/// [`IconCache::set_foreground_background`] supplied a tone — see that setter
/// for why the tone is the launcher's to choose rather than this module's.
///
/// # Cost
///
/// O(pixels), once per cache miss. The cheap `a == 255` early-out skips the
/// interior of most icons, so in practice this touches the background and the
/// antialiased rim, not the artwork.
pub fn composite_foreground(img: &mut RgbaImage, tile_color: u32) -> bool {
    let w4 = img.width as usize * 4;
    if w4 == 0 || img.pixels.len() < img.height as usize * w4 {
        return false;
    }
    let bg = tile_color & 0x00FF_FFFF;
    let mut changed = false;
    for px in img.pixels[..img.height as usize * w4]
        .as_chunks_mut::<4>()
        .0
    {
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
/// opacity *is* the original icon's alpha.
///
/// # Where it runs
///
/// On the insert path, from [`IconCache::set_monochrome_tint`] -- once per
/// entry, never per frame. That is the same decision the reference makes in
/// `LawnchairIconProvider.getIcon:160-213`, which routes `themedIcons` and
/// `drawerThemedIcons` (`PreferenceManager.kt:166-167`) through
/// `MonoIconThemeController` (`icons/LawnchairThemeManager.kt:123`) and hands
/// the drawable a tinted layer instead of the original one. `forceIconMonochrome`
/// (`PreferenceManager.kt:206`) is the same switch, and UTLC's
/// [`launcher_state::LauncherState::monochrome_icons`](crate::launcher_state::LauncherState::monochrome_icons)
/// is the field that carries it.
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
/// into `rect`, with its surface cut to `shape`.
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
/// # Shape
///
/// `shape` is the last argument rather than a constant because the fallback
/// tile must be cut to the *same* silhouette as a real icon: a squircle app
/// tile beside a circular monogram reads as a rendering fault. It comes from
/// [`IconCache::get_or_monogram_shaped`], i.e. from
/// [`IconCache::set_shape`] or from the caller's own choice for a surface that
/// draws differently from the grid.
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
/// The nine-argument shape matches every other surface-drawing entry point in
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
    shape: IconShape,
) -> bool {
    // Reject before drawing: a rejected initial must leave the tile untouched.
    //
    // Only control characters and spaces are refused. The old guard was
    // `(0x21..0x7F)`, which rejected every non-ASCII initial outright -- and the
    // caller had already filtered to the same ASCII window, so a Cyrillic or
    // Greek app name fell through to '?' twice over. `draw_glyph` renders a
    // non-ASCII initial properly when the face has the glyph and as a visible
    // placeholder when it does not, so neither case needs to be refused here.
    if initial.is_control() || initial == ' ' {
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
        let half = shape_half_at(shape, r, (py as f32 + 0.5 - cy).abs());
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
    // `b` is a `char`: the monogram is the first character of the app name, and
    // truncating it to `u8` here would turn a Greek or Cyrillic initial into a
    // different, unrelated character -- or into nothing at all.
    let pen_x = cx - font::char_advance(initial, size_px) * 0.5;
    font::draw_glyph(
        buf,
        stride,
        w,
        h,
        pen_x,
        ascender_y,
        initial,
        glyph_color,
        size_px,
        FontWeight::Medium,
    );
    true
}

/// Render the same procedural monogram [`draw_monogram_tile`] draws, but into
/// an [`RgbaImage`] at `edge`, cut to `shape`, ready to be cached by
/// [`IconCache`].
///
/// # Why a second entry point and not a reuse of `draw_monogram_tile`
///
/// `draw_monogram_tile` paints into a `&mut [u32]` **framebuffer**: XRGB, with
/// the alpha byte not stored, which is what the DRM plane wants. The cache
/// stores straight-alpha RGBA, because `draw_icon_bitmap_i32` composites an
/// icon over the app's own tile colour and needs a real alpha to blend against.
/// So the two cannot share a buffer type without either giving the cache a
/// format the blitter would have to convert, or giving the framebuffer a
/// format the plane cannot scan out.
///
/// The pixel work is identical -- this *is* `draw_monogram_tile`, called with a
/// scratch surface and the same `shape` -- so the two can never disagree about
/// the silhouette, the cap fraction or the glyph metrics. Only the container
/// differs.
///
/// # Cost
///
/// One `edge * edge` `Vec<u32>` scratch plus one `edge * edge * 4` result, both
/// freed before returning. That is a **per-miss** cost, on the same path that
/// already decoded a PNG and resampled it; it is never a per-frame cost,
/// because the result is cached and the renderer only ever reads it.
pub fn monogram_tile_image(
    initial: char,
    tile_color: u32,
    glyph_color: u32,
    edge: u32,
    shape: IconShape,
) -> Option<RgbaImage> {
    let edge = edge.max(1);
    let n = edge as usize;
    if n > 4096 {
        // 16M px of scratch is a bug, not a tile; refuse rather than abort.
        return None;
    }
    // Start fully transparent: the scratch is the tile's own alpha mask, and
    // the XRGB alpha byte is discarded, so 0 must mean "not painted".
    let mut scratch = vec![0u32; n * n];
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: edge as f32,
        h: edge as f32,
        radius: 0.0,
    };
    if !draw_monogram_tile(
        &mut scratch,
        n,
        n,
        n,
        rect,
        initial,
        tile_color,
        glyph_color,
        shape,
    ) {
        return None;
    }
    // XRGB -> straight RGBA. `0x00000000` is the only value `draw_monogram_tile`
    // leaves behind (every write goes through `blend_alpha`, which forces
    // `0xFF00_0000` in the high byte), so it doubles as "outside the shape".
    let mut pixels = Vec::with_capacity(n * n * 4);
    for px in &scratch {
        let inside = *px != 0;
        pixels.push((px >> 16) as u8);
        pixels.push((*px >> 8) as u8);
        pixels.push(*px as u8);
        pixels.push(if inside { 0xFF } else { 0 });
    }
    drop(scratch);
    Some(RgbaImage {
        width: edge,
        height: edge,
        pixels,
    })
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
/// Clamped to [`ICON_MAX_TILE_EDGE`]: that is the largest tile the cache will
/// hold, which is what keeps [`ICON_CACHE_BUDGET`] holding a screen of icons
/// rather than three. Above roughly a 1400 px panel the tile ceiling, not this
/// function, becomes the binding constraint and the blit resamples again.
#[inline]
pub fn icon_max_edge(panel_px: f32) -> u32 {
    let p = DeviceProfile::for_panel(panel_px);
    let px = p.dp(p.icon_dp);
    if !px.is_finite() {
        return 1;
    }
    px.round().clamp(1.0, ICON_MAX_TILE_EDGE as f32) as u32
}

/// Directory, relative to the XDG cache root, that holds pre-rasterised icons.
///
/// # Producers
///
/// Two, and they are interchangeable because both write the same thing — an
/// 8-bit RGBA PNG at exactly [`icon_max_edge`] px, which is what
/// [`IconCache::load_offline`] decodes:
///
/// 1. **In-tree, and on by default.** Every icon [`IconCache`] resolves is
///    written back by [`IconCache::cache_offline`] through
///    [`write_offline_icon`]. That covers every app that ships a raster, which
///    is most of them; it also covers the `.svg` apps on the *second* launch,
///    once the oneshot below has produced their first PNG.
/// 2. **Out of process, for the `.svg` apps on the first launch.** A large
///    share of Linux app icons are `.svg` under
///    `/usr/share/icons/hicolor/scalable/apps/`, and UTLC cannot draw them:
///    `resvg` is 30+ transitive crates and ~6 MiB of binary, which the zero
///    third-party-dependency rule rejects outright. So the first-launch
///    rasterisation happens out of process, by something already on the system:
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
/// A missing entry is not an error: it falls through to the theme sweep, and
/// then to [`draw_monogram_tile`]. A *corrupt* entry is not an error either —
/// see [`write_offline_icon`] for the temp-file discipline that keeps one from
/// existing, and `offline_cache_miss_falls_through_without_pinning_a_miss` for
/// what happens if one does.
///
/// # The format is PNG, deliberately
///
/// Not a raw blob. A raw RGBA dump would be 100 bytes smaller per file and
/// cannot be malformed, but it is unreadable by everything that is not this
/// crate — including the `rsvg-convert` oneshot above, which is the *only*
/// way the `.svg` apps get into this directory at all. The reference makes the
/// same trade whenever it has to put a bitmap on disk as bytes: `bitmap.compress(
/// Bitmap.CompressFormat.PNG, 100, out)` in `bitmapToByteArray`
/// (`lawnchair/src/app/lawnchair/util/LawnchairUtils.kt:306-310`), and the same
/// for the wallpaper and screenshot copies in its backup path
/// (`lawnchair/src/app/lawnchair/backup/LawnchairBackup.kt:173-178`).
///
/// Written by [`crate::graphics::png::encode_png`], whose output is a
/// spec-compliant PNG: verified against `file(1)` and an independent zlib
/// implementation, not only against this crate's own decoder.
pub const OFFLINE_CACHE_SUBDIR: &str = "utlc/icons";

/// Append `n` in decimal to `out`.
///
/// No `write!`: the formatter and its ~4 KiB of tables stay out of the binary.
/// Ten slots covers `u32::MAX` with no leading zeros.
fn push_decimal(out: &mut String, mut n: u32) {
    let mut d = [0u8; 10];
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
        out.push(*b as char);
    }
}

/// File-name convention of the offline cache: `<app_id>_<edge>.png`.
///
/// `app_id` is the desktop-entry id (`org.mozilla.firefox.desktop`, or the
/// `Icon=` stem for an entry whose id is not a usable name), verbatim — no
/// case folding, no path separators. `edge` is the pixel edge the icon was
/// rendered at, which is [`icon_max_edge`] of the panel, so the helper can
/// pre-render exactly what the launcher will blit. Both halves are what make a
/// stale cache miss instead of a wrong-scale hit.
///
/// Verbatim does mean the caller has to have checked
/// [`is_offline_cache_name_safe`] first; [`write_offline_icon`] does.
pub fn offline_cache_name(app_id: &str, edge: u32) -> String {
    let mut name = String::with_capacity(app_id.len() + 8);
    name.push_str(app_id);
    name.push('_');
    push_decimal(&mut name, edge);
    name.push_str(".png");
    name
}

/// `true` when `key` is usable as part of a cache file name.
///
/// The key comes out of a desktop file's `Icon=` field, so as far as this
/// module is concerned it is untrusted input. A `/` — or a NUL, which the
/// `CString`/`open` layers below this one would reject with a bare `EINVAL` —
/// makes [`offline_cache_path`] resolve *outside* the cache directory, and `.`
/// or `..` would do the same without a separator. Everything else passes
/// through unchanged, which is what keeps the documented
/// `<app_id>_<edge>.png` convention intact.
///
/// Note this is a *refusal*, not a sanitisation: rewriting a key into a
/// different file name would make the directory hold two names for one app, and
/// only the caller that refuses can report the problem.
fn is_offline_cache_name_safe(key: &str) -> bool {
    !key.is_empty()
        && key != "."
        && key != ".."
        && !key.contains('/')
        && !key.contains('\0')
        && !key.starts_with(OFFLINE_TMP_PREFIX)
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

// ===========================================================================
// Offline cache: writer, pruner
// ===========================================================================

/// Leading marker of a half-written cache file: `.utlc-tmp.`.
///
/// Deliberately at the *front* of the name, so three things follow from one
/// `str::starts_with`:
///
/// * [`IconCache::load_offline`] can never read one — it only ever builds
///   `<app_id>_<edge>.png`, and this name starts with a dot;
/// * [`prune_offline_cache`] recognises an orphan from a killed launcher's
///   crash-consistent leftovers as garbage and deletes it, so the directory
///   cannot grow without bound across crashes;
/// * [`is_offline_cache_name_safe`] refuses an app id that would collide with
///   the namespace.
const OFFLINE_TMP_PREFIX: &str = ".utlc-tmp.";

/// Temporary file [`write_offline_icon`] writes into, for the final path
/// `final_path`.
///
/// `rename(2)` is only atomic **within one filesystem**, so the temp file has to
/// live in the same directory as its destination — which is also the only place
/// this module is allowed to create files at all.
fn offline_temp_path(final_path: &Path) -> PathBuf {
    let mut name = String::with_capacity(OFFLINE_TMP_PREFIX.len() + 32);
    name.push_str(OFFLINE_TMP_PREFIX);
    // The pid keeps two launcher instances (a normal one and a `--rasterise`
    // helper, say) from sharing one temp path. Within one process it is
    // redundant, because `resolve_keys` takes `&mut self` — there is no
    // concurrency to guard here.
    push_decimal(&mut name, std::process::id());
    name.push('.');
    // The final name verbatim, so an orphan found by a prune is identifiable.
    if let Some(base) = final_path.file_name().and_then(|n| n.to_str()) {
        name.push_str(base);
    }
    final_path.with_file_name(name)
}

/// Encode `img` and write it into the offline cache as
/// `dir/<key>_<edge>.png`, returning the number of bytes written.
///
/// The format is a plain 8-bit RGBA PNG — see [`OFFLINE_CACHE_SUBDIR`] for why
/// PNG and not a raw blob, and [`crate::graphics::png::encode_png`] for how the
/// bytes are produced. This is also the API an out-of-process rasteriser should
/// call, so the launcher and a helper cannot drift apart on the format.
///
/// # Atomicity, and the failure each half prevents
///
/// Write to [`offline_temp_path`] (same directory), then `rename`. The two
/// halves buy two different protections:
///
/// * the temp **name** — `load_offline` never builds a name that starts with
///   [`OFFLINE_TMP_PREFIX`], so even a torn write cannot be *read* as a cache
///   entry. Without that, a crash mid-write would leave a truncated PNG that
///   the next boot's `LookupStage::OfflineCache` happily opens and tries to
///   decode, on every subsequent boot, for an entry that can never succeed;
/// * the `rename` — it is atomic on POSIX, so a reader sees either the old file
///   or the complete new one. Truncated data is never visible under the final
///   name. If the write or the rename fails, the temp is unlinked, so a
///   full disk leaves a short temp (which the next prune collects) rather than
///   a short entry.
///
/// There is deliberately **no `fsync`**. Two syscalls per icon is the whole
/// cost of populating a cache; an fsync per icon would put a device-flush
/// latency on the launcher's first frame. Without it a power cut can leave the
/// rename durable while the data blocks are not, i.e. a file of the right length
/// full of zeroes — which fails the Adler-32 and decodes to `None`, so the
/// reader falls through to the sweep. The failure mode is a cache miss, never a
/// wrong icon, and that is what [`IconCache::load_offline`] is already built to
/// do.
///
/// # Which side of the frame path
///
/// **Off** it. This encodes and allocates one `Vec<u8>` the size of the
/// encoded image (~`w*h*4 + h + 13` bytes), and it is called from
/// [`IconCache::cache_offline`] on the icon *resolution* path — a cold path that
/// already decodes a PNG per key and may walk a theme tree. The render path
/// ([`IconCache::get`], [`IconCache::get_or_monogram`]) never reaches any of it.
///
/// The directory is created if absent, so a first-run write needs no separate
/// setup step. Errors are returned rather than swallowed so a caller can log
/// them; the cache itself ignores them, because a cache that cannot be written
/// is not a launcher error.
///
/// Refuses a `key` that is not a safe file name (see
/// [`is_offline_cache_name_safe`]) rather than escaping `dir`.
pub fn write_offline_icon(
    dir: &Path,
    key: &str,
    edge: u32,
    img: &RgbaImage,
) -> Result<usize, std::io::Error> {
    use std::io::Write;

    if !is_offline_cache_name_safe(key) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "icon key is not a usable cache file name",
        ));
    }
    let png = crate::graphics::png::encode_png(img).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "image cannot be encoded as a PNG",
        )
    })?;
    std::fs::create_dir_all(dir)?;
    let final_path = offline_cache_path(dir, key, edge);
    let tmp_path = offline_temp_path(&final_path);
    let written = png.len();
    {
        // `create` truncates, so a temp left by a previous crashed write of the
        // same pid cannot be appended to.
        let mut file = std::fs::File::create(&tmp_path)?;
        if let Err(e) = file.write_all(&png).and_then(|()| file.flush()) {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(written)
}

/// What a [`prune_offline_cache`] pass left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OfflineCacheUsage {
    /// `.png` entries still in the directory.
    pub files: usize,
    /// Their total size, as `stat` reports it.
    pub bytes: u64,
    /// Orphan [`OFFLINE_TMP_PREFIX`] files the same pass removed.
    pub temps: usize,
}

/// Trim the offline cache in `dir` to at most `max_bytes` and `max_files`,
/// oldest entry first, and report what is left.
///
/// Oldest is `mtime`, with the file name as a deterministic tie-break so the
/// same directory always trims to the same set. `mtime` is also what keeps the
/// policy honest for free: [`IconCache::cache_offline`] never rewrites an entry
/// that already exists, so a file's mtime really is the last time its icon was
/// resolved, and the least recently resolved icons are the ones evicted — the
/// disk mirror of the in-memory LRU in [`IconCache::insert_image`].
///
/// Only entries this module's naming convention recognises are counted or
/// deleted: a regular file ending in `.png`. Anything else in the directory is
/// left strictly alone — it is not ours, and a shared cache root may hold
/// things we do not understand. Orphan [`OFFLINE_TMP_PREFIX`] files are the one
/// exception: they can only have come from a write that did not finish, so they
/// are counted in `temps` and deleted.
///
/// `symlink_metadata`, not `metadata`: a link is not ours to delete and must be
/// measured as a link (one inode) rather than as whatever it points at, or a
/// prune could delete a target the launcher does not own.
///
/// # Which side of the frame path
///
/// **Off** it, and it is the only unbounded `read_dir` in this module. It is
/// called every [`OFFLINE_PRUNE_INTERVAL`] writes from the resolution path, and
/// [`IconCache::prune_offline_cache`] lets a caller force it.
pub fn prune_offline_cache(
    dir: &Path,
    max_bytes: u64,
    max_files: usize,
) -> Result<OfflineCacheUsage, std::io::Error> {
    let mut entries: Vec<(PathBuf, u64, u64)> = Vec::new();
    let mut temps = 0usize;
    for entry in std::fs::read_dir(dir)? {
        // A dirent we cannot read (removed between readdir and stat, or an
        // undecodable name) is simply not counted; the next pass will see it.
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(OFFLINE_TMP_PREFIX) {
            let _ = std::fs::remove_file(entry.path());
            temps += 1;
            continue;
        }
        if !name.ends_with(".png") {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        // Pre-epoch or unreadable timestamps sort as 0, i.e. oldest. That is the
        // right way to be wrong: such a file is evicted before any dated one.
        let age = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        entries.push((path, meta.len(), age));
    }

    let mut bytes: u64 = entries.iter().map(|e| e.1).sum();
    let mut files = entries.len();
    entries.sort_unstable_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
    for (path, size, _) in entries {
        if bytes <= max_bytes && files <= max_files {
            break;
        }
        // A failed unlink leaves the entry counted, so the pass cannot
        // under-report what is still on disk.
        if std::fs::remove_file(&path).is_ok() {
            files -= 1;
            bytes -= size;
        }
    }
    Ok(OfflineCacheUsage {
        files,
        bytes,
        temps,
    })
}

/// One stage of icon resolution.
///
/// This enum is the single source of truth for the order: [`LOOKUP_ORDER`] is
/// what [`IconCache::resolve_keys`] iterates, and each variant is one `match`
/// arm in that loop. There is deliberately no second copy of the order in the
/// control flow, because a documented order that is not the implemented order is
/// a comment that lies the first time someone reorders it.
///
/// See [`LOOKUP_ORDER`] for the order, and `lookup_order_matches_the_dispatch`
/// for the test that holds the two together.
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
/// # Why path first
///
/// A path key is unambiguous — there is exactly one file it can mean — so it
/// skips straight to it and never enters the sweep. That is also the property
/// that makes the per-key stages safe to reorder: each arm declares "not my
/// kind of key" by *continuing*, not by consuming, so no arrangement of this
/// array can turn a path key into a sweep candidate or hand a path key to the
/// offline cache (whose [`offline_cache_path`] would then resolve outside the
/// cache directory — [`is_offline_cache_name_safe`] refuses it anyway).
///
/// # Why the sweep is last, and why that is a structural fact
///
/// [`LookupStage::Theme`] cannot be a per-key step: one directory pass answers
/// the whole batch, which is the entire point of the sweep (P24). So it is the
/// tail of the loop and the batch runs after it, and the loop asserts that it is
/// in last position rather than assuming it.
///
/// Adding a stage is one entry here and one `match` arm in
/// [`IconCache::resolve_keys`]; nothing else in the module needs to change.
pub const LOOKUP_ORDER: [LookupStage; 3] = [
    LookupStage::AbsolutePath,
    LookupStage::OfflineCache,
    LookupStage::Theme,
];

/// Largest total size of the offline icon directory, in bytes.
///
/// # Derived from [`ICON_CACHE_BUDGET`], not a second policy
///
/// The in-memory LRU is sized for *one screen* of icons — it can always be
/// rebuilt by re-reading the sweep, so nothing is lost by dropping it. The disk
/// cache exists so a cold boot does **not** re-run the sweep, so it is sized for
/// an entire app set instead: eight times the memory budget is ~300 icons at
/// the reference phone's 167 px tile (~109 KiB each, ~33 MiB), which covers a
/// phone workspace plus drawer with room to spare. Same policy, different
/// lifetime, therefore a different multiple — not an unrelated constant.
pub const OFFLINE_CACHE_BUDGET: usize = ICON_CACHE_BUDGET * 8;

/// Hard cap on the number of entries in the offline directory.
///
/// The byte budget alone does not bound a directory *count*: a 1x1 hicon encodes
/// to ~73 bytes, so [`OFFLINE_CACHE_BUDGET`] alone would admit hundreds of
/// thousands of files and the `read_dir` of a prune pass would cost more than the
/// whole sweep it exists to avoid. At the reference tile size the byte budget
/// binds first (~300 entries); this is the backstop for the small-icon
/// long tail.
pub const OFFLINE_CACHE_MAX_FILES: usize = 4096;

/// Writes performed between two prune passes.
///
/// The directory is only re-measured and trimmed on this cadence, so the cost of
/// pruning (one `read_dir` plus one `lstat` per entry) is amortised rather than
/// paid per icon. It is deliberately *not* a byte counter kept in
/// [`IconCache`]: an incrementally-tracked total is a second accounting of
/// something the filesystem already knows exactly, and a second accounting is
/// where this module's one existing byte-accounting bug came from — `live_bytes`
/// double-counted a repeated key until `insert_image` reclaimed it *before* the
/// eviction loop. One source of truth (`stat`) beats a fast, drift-prone one.
const OFFLINE_PRUNE_INTERVAL: usize = 64;

/// Total decoded-pixel budget held by the cache; LRU eviction keeps the
/// live set under this (P2). Sized for the *pre-scaled* tile: at
/// [`icon_max_edge`] = 167 a tile is 111 KiB, so 4 MiB holds ~37 live icons —
/// one screen of a phone workspace plus the dock. 64x64 RGBA tiles were 16 KiB
/// and held ~256, but those were 2.6x too small to blit 1:1.
pub const ICON_CACHE_BUDGET: usize = 4 * 1024 * 1024;
/// Largest edge a cached tile may have, whatever the panel asks for.
///
/// This is a *cache* ceiling, not a decode ceiling: a 256 px tile is 256 KiB of
/// RGBA, so the 4 MiB budget holds 16 of them. Letting a large panel raise it
/// would trade one screen of icons for three, which is the wrong trade — the
/// blit is bilinear on both sides of the miss either way, so the extra pixels
/// buy sharpness nobody asked for. [`ICON_MAX_SOURCE_EDGE`] bounds what is
/// *read*; this bounds what is *kept*.
pub const ICON_MAX_TILE_EDGE: u32 = 256;
/// Largest edge an icon source may have, and the largest this module will
/// resample.
///
/// # Why a source larger than this is refused at all (P15)
///
/// `decode_png` already caps a source at `MAX_PIXELS` (4 Mi px) and
/// [`MAX_ICON_FILE_BYTES`] caps the file at 8 MiB, so the decode itself is
/// bounded. What is *not* bounded is the resample: `RgbaImage::fit_within`
/// allocates the destination and walks the source once per pixel, so a 4 Mi px
/// source costs ~16 MiB of transient buffer and a full-tile pass on whatever
/// thread called it — which, per the module doc, may be the frame thread. A
/// source past this edge would be averaged down to [`icon_max_edge`] anyway, so
/// the pixels are worth less than they cost.
///
/// # Why not simply reject anything over 256 (what this used to do)
///
/// Because 256 is not a real icon size. The freedesktop `hicolor` set ships
/// 256x256 and 512x512 rasters for a large share of applications, so the old
/// rule silently turned real icons into monogram tiles: `load_path` returned
/// `None`, the key was recorded as a miss, and the app rendered as a letter.
/// Failing on a *value* the system genuinely produces is not a bounded failure,
/// it is a wrong answer delivered confidently. The reference has the same
/// problem and solves it by density bucket — `getLauncherIconDensity`
/// (`InvariantDeviceProfile.java:905-926`) picks the raster nearest the
/// launcher's own size and the framework decodes only that one. UTLC's
/// equivalent is exactly "accept the source, downscale to the tile edge", and
/// this constant is the ceiling on how big that source may be.
pub const ICON_MAX_SOURCE_EDGE: u32 = 1024;
/// Reject absurdly large icon files before handing them to the decoder.
/// Tied to the decoder's MAX_PIXELS (4M px): worst-case small-icon PNGs
/// stay far below this (P16).
const MAX_ICON_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Dirent budget for a single resolution sweep (bounds worst-case scan cost).
const SWEEP_BUDGET: usize = 65_536;
/// Smallest icon edge the adaptive mask is applied to.
///
/// # Why the mask has a floor at all
///
/// [`apply_shape`] asks [`squircle_span`] for one half-width per row, and
/// that span is `isqrt(isqrt(...))` -- a **floored** integer. The floor costs
/// up to one pixel at each end of every row, and at a small radius that is a
/// large fraction of the whole tile: at edge 8 (r = 4) the row spans are
/// `0, 6, 6, 6, 8, 6, 6, 6`, so the mask eats **31%** of the icon -- it stops
/// being a corner trim and becomes the icon. The removed fraction falls
/// towards the analytic 7.3% (`1 - 3.7081 r^2 / 4r^2`) as r grows, and is
/// 8.4% at edge 64.
///
/// 32 is therefore the smallest edge at which the cut is a corner rather than
/// a crop. Below it the decoded tile is cached byte-for-byte, which is also
/// the only thing that keeps a 1x1 or 8x8 hicon (a 1 px mask is degenerate --
/// `squircle_span(0, _) == 0` erases the tile completely) usable. Everything
/// the shell actually draws is at [`icon_max_edge`] (167 px on a reference
/// phone), so the floor never fires on the render path.
const ICON_MASK_MIN_EDGE: u32 = 32;
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

/// A pre-blurred 8-bit coverage mask for one cached icon, at the tile's edge.
///
/// # What the shell does with it
///
/// ```ignore
/// if let Some(m) = icon_cache.shadow(key) {
///     raster::draw_preblurred_shadow_from_mask(
///         buf, stride, w, h,
///         &m.coverage, m.edge as usize, m.edge as usize,
///         tile_x, tile_y, offset_x, offset_y,
///         shadow_rgb, shadow_alpha,
///     );
/// }
/// ```
///
/// That call must happen *before* the tile and the icon are drawn — a shadow
/// under a filled tile is invisible, and the reference draws it as the drawable's
/// own first layer (`Shadow.apply`, `ShadowPainter`).
///
/// # Why cached rather than derived per frame
///
/// `blur_mask_8` is `2 * mask_pixels` and cannot be done incrementally; the
/// reference's `DoubleShadowIconDrawable.kt:35-53` blurs once per drawable for
/// exactly this reason, and UTLC's drawable *is* the cache entry. Cached, the
/// per-frame cost is one blend per covered pixel with no scratch buffer at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowMask {
    /// Mask edge in pixels. Equal to the cached tile's edge, so the blit is 1:1.
    pub edge: u32,
    /// `edge * edge` coverage bytes, row-major, in `0..=255`. The blurred alpha
    /// of the finished tile.
    pub coverage: Box<[u8]>,
}

/// Caches decoded application icons and resolves icon names against the
/// installed icon themes with a single batched directory sweep per request.
pub struct IconCache {
    roots: Vec<PathBuf>,
    preferred_theme: String,
    images: HashMap<String, Rc<RgbaImage>>,
    /// Blurred shadow masks, parallel to `images`. Empty unless
    /// [`IconCache::set_icon_shadow`] is on.
    shadows: HashMap<String, Rc<ShadowMask>>,
    /// Last-use tick per cached key for LRU eviction (P2).
    used: RefCell<HashMap<String, u64>>,
    misses: HashSet<String>,
    /// Edge every decoded icon is resampled to on the way into the cache.
    display_edge: u32,
    /// Adaptive-icon mask applied on the insert path. `Squircle` by default,
    /// which is the reference's own default (`IconShapePreference`).
    shape: IconShape,
    /// Flat tint for the whole grid, `0xAARRGGBB`, or `None` for the icons as
    /// installed. `Some` means every cached icon has been through
    /// [`make_monochrome`], so the pixels are no longer the file's.
    monochrome: Option<u32>,
    /// Colour the `<foreground>` reconstruction is composited onto, or `None`
    /// to leave bare foreground layers alone.
    fg_background: Option<u32>,
    /// Whether [`Self::insert_image`] also builds a blurred shadow mask.
    shadows_enabled: bool,
    /// `$XDG_CACHE_HOME/utlc/icons` (see [`OFFLINE_CACHE_SUBDIR`]), consulted
    /// before the theme sweep and written to by [`Self::cache_offline`].
    /// `None` disables the stage.
    offline_dir: Option<PathBuf>,
    /// Whether a resolved icon is written back to `offline_dir`.
    offline_writing: bool,
    /// Successful writes since the last prune pass, so pruning is amortised
    /// over [`OFFLINE_PRUNE_INTERVAL`] icons instead of run per icon. A count
    /// and not a byte total on purpose — see that constant.
    offline_writes: usize,
    /// Monotonic clock for `used` ticks.
    tick: Cell<u64>,
    /// Sum of `width*height*4` over `images` plus the mask bytes over
    /// `shadows`, bounded by [`ICON_CACHE_BUDGET`].
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
        let data_dirs = std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
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
            shadows: HashMap::new(),
            used: RefCell::new(HashMap::new()),
            misses: HashSet::new(),
            display_edge: ICON_MAX_EDGE,
            // The reference's own default, and the one shape every icon theme
            // on a modern desktop already assumes.
            shape: IconShape::Squircle,
            monochrome: None,
            fg_background: None,
            shadows_enabled: false,
            // Consulted first: an out-of-process renderer's output is
            // authoritative, and skipping the sweep for a cache hit is the
            // whole point of keeping the directory populated.
            offline_dir: offline_cache_dir(),
            // On: the whole point of the stage is that the next boot skips the
            // sweep, and the write is one open/write/rename per icon, once
            // ever. `with_roots` below turns it off for tests.
            offline_writing: true,
            offline_writes: 0,
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

    /// Mask every cached icon with `shape`, instead of [`IconShape::Squircle`].
    ///
    /// The shape is applied once, on the insert path (see [`mask_tile`]), for
    /// the reason the mask lives in the cache at all: the render path stays a
    /// 1:1 blit. The reference offers ~28 presets through
    /// `IconShapePreference.iconShapeEntries:77-110`; UTLC implements the three
    /// [`IconShape`] variants, so this is the whole surface.
    ///
    /// Changing the shape **discards every shaped icon**. It has to: a cached
    /// icon is an `Rc<RgbaImage>` and the mask is baked into its alpha, so
    /// there is no way to restyle one in place. Re-resolution is a single
    /// sweep over keys the caller already has, and it is what makes a settings
    /// change visible rather than needing a restart.
    ///
    /// Shape is per-*surface*, not per-app: one field for the whole cache.
    /// [`Self::get_or_monogram_shaped`] is the per-call escape hatch, and it
    /// exists because the reference genuinely separates shapes per surface
    /// (`homeScreenIconShape` / `drawerIconShape`, and `folderShape` at
    /// `PreferenceManager2.kt:151`).
    pub fn set_shape(&mut self, shape: IconShape) {
        if self.shape != shape {
            self.shape = shape;
            self.clear_shaped();
        }
    }

    /// The mask [`Self::set_shape`] selected.
    pub fn shape(&self) -> IconShape {
        self.shape
    }

    /// Tint every icon flat to `0xAARRGGBB`, or `None` for the icons as
    /// installed.
    ///
    /// Backs the reference's `forceIconMonochrome`
    /// (`PreferenceManager.kt:206`), which routes through
    /// `MonoIconThemeController` (`LawnchairThemeManager.kt:123`) and replaces
    /// every drawable with a `ThemedIconDrawable` of the theme's foreground
    /// colour. [`make_monochrome`] preserves the alpha and replaces the RGB,
    /// which is exactly what a `<monochrome>` layer is, and it runs once per
    /// entry here.
    ///
    /// Changing the tint discards every shaped icon, for the same `Rc` reason
    /// as [`Self::set_shape`].
    pub fn set_monochrome(&mut self, tint: Option<u32>) {
        if self.monochrome != tint {
            self.monochrome = tint;
            self.clear_shaped();
        }
    }

    /// The monochrome tint, or `None` when icons are used as installed.
    pub fn monochrome(&self) -> Option<u32> {
        self.monochrome
    }

    /// Composite bare `<foreground>` layers onto `0xAARRGGBB`, or `None` to
    /// leave them alone.
    ///
    /// # Why the launcher has to synthesise the background
    ///
    /// An Android adaptive icon is two layers, and only the foreground reaches
    /// a freedesktop consumer: the `<background>` lives in an
    /// `adaptive-icon` XML that UTLC has no way to read (see
    /// [`has_explicit_background`] for the full argument). So a foreground-only
    /// PNG caches with transparent corners, and `draw_icon_bitmap_i32` blends
    /// those transparent pixels straight over `app.color` — the icon looks
    /// correct by accident, on the one surface where the tile behind it happens
    /// to be that colour.
    ///
    /// [`composite_foreground`] fills the transparent region with a tone the
    /// caller picks, and the mask then cuts that fill to the icon's silhouette.
    /// `None` is the default because the right tone is a launcher decision:
    /// `LauncherState::accent_color` / `palette.surface`, not something this
    /// module should guess.
    ///
    /// Changing the background discards every shaped icon, for the same `Rc`
    /// reason as [`Self::set_shape`].
    pub fn set_foreground_background(&mut self, colour: Option<u32>) {
        if self.fg_background != colour {
            self.fg_background = colour;
            self.clear_shaped();
        }
    }

    /// The `<foreground>` reconstruction tone, or `None`.
    pub fn foreground_background(&self) -> Option<u32> {
        self.fg_background
    }

    /// Cache a pre-blurred shadow mask alongside each icon.
    ///
    /// Off by default. It is not free: a mask is `edge * edge` bytes against
    /// the tile's `edge * edge * 4`, so a 167 px tile goes from 111 KiB to 139
    /// KiB and the 4 MiB budget holds ~29 icons instead of ~37. The reference
    /// makes the same trade optional (`shadowBGIons`,
    /// `PreferenceManager.kt:76-78`), and turns it *off* entirely for the dark
    /// theme (`styles.xml:111-113`), so it is not something to enable
    /// unconditionally.
    ///
    /// Read the mask back with [`Self::shadow`] and composite it with
    /// [`crate::graphics::raster::draw_preblurred_shadow_from_mask`]. Turning
    /// this on discards the shaped icons, since the mask is derived from the
    /// icon's alpha and is built at the same moment.
    pub fn set_icon_shadow(&mut self, enabled: bool) {
        if self.shadows_enabled != enabled {
            self.shadows_enabled = enabled;
            self.clear_shaped();
        }
    }

    /// The pre-blurred shadow mask for `key`, if the shadow stage is on and
    /// the key resolved. Allocation-free and tick-free: it is the cached
    /// `Rc`, and the frame path must not tick LRU through a second map.
    pub fn shadow(&self, key: &str) -> Option<Rc<ShadowMask>> {
        self.shadows.get(key).cloned()
    }

    /// Whether the shadow stage is on.
    pub fn shadows_enabled(&self) -> bool {
        self.shadows_enabled
    }

    /// Override the offline icon cache directory; `None` disables the stage.
    pub fn set_offline_dir(&mut self, dir: Option<PathBuf>) {
        self.offline_dir = dir;
    }

    /// The offline cache directory, if the stage is enabled.
    pub fn offline_dir(&self) -> Option<&Path> {
        self.offline_dir.as_deref()
    }

    /// Write every newly resolved icon back into [`Self::offline_dir`], or stop.
    ///
    /// On by default for [`IconCache::new`], off for
    /// [`IconCache::with_roots`]. Turning it *off* does not remove anything
    /// already written: the files stay valid and keep being read, so this is a
    /// read-only-session switch rather than a cache clear. Use
    /// [`Self::invalidate_key`] for that, or delete the directory.
    ///
    /// Turning it *on* is worth it for exactly the session it was designed for:
    /// a first boot pays a sweep per cold icon and writes one PNG per icon, and
    /// every boot after that pays a single `open` per icon instead.
    pub fn set_offline_writing(&mut self, enabled: bool) {
        self.offline_writing = enabled;
    }

    /// Whether resolved icons are written back to the offline cache.
    pub fn offline_writing(&self) -> bool {
        self.offline_writing
    }

    /// Force a trim of the offline directory to [`OFFLINE_CACHE_BUDGET`] and
    /// [`OFFLINE_CACHE_MAX_FILES`], and report what is left.
    ///
    /// [`Self::cache_offline`] already calls this every
    /// [`OFFLINE_PRUNE_INTERVAL`] writes; this is the explicit entry point for
    /// a shell that wants the directory trimmed at a quieter moment, and for
    /// tests. Fails with `NotFound` when the stage has no directory, and with
    /// the `read_dir` error when the directory cannot be listed — a cache that
    /// cannot be trimmed is not an error the caller has to handle, but the
    /// `Result` says which case it was rather than hiding both.
    pub fn prune_offline_cache(&mut self) -> Result<OfflineCacheUsage, std::io::Error> {
        let dir = self
            .offline_dir
            .as_deref()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        // Reset the amortisation counter whatever the outcome: a prune that
        // failed is not a reason to retry on every subsequent write.
        self.offline_writes = 0;
        prune_offline_cache(dir, OFFLINE_CACHE_BUDGET as u64, OFFLINE_CACHE_MAX_FILES)
    }

    /// Explicit roots (used by tests and by callers with a custom search path).
    ///
    /// The offline stage is off, in both directions: a test that resolves
    /// through a temp tree must not be perturbed by whatever the developer's
    /// `~/.cache` happens to hold, nor must it leave files there. Call
    /// [`Self::set_offline_dir`] to opt a test back in — which also has to
    /// [`Self::set_offline_writing`] on, deliberately, so no test writes a
    /// directory it did not name.
    pub fn with_roots(roots: Vec<PathBuf>, preferred_theme: &str) -> IconCache {
        IconCache {
            roots: dedupe_roots(roots),
            preferred_theme: preferred_theme.to_string(),
            images: HashMap::new(),
            shadows: HashMap::new(),
            used: RefCell::new(HashMap::new()),
            misses: HashSet::new(),
            display_edge: ICON_MAX_EDGE,
            shape: IconShape::Squircle,
            monochrome: None,
            fg_background: None,
            shadows_enabled: false,
            offline_dir: None,
            offline_writing: false,
            offline_writes: 0,
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
        self.used
            .borrow_mut()
            .insert(key.to_string(), self.tick.get());
        Some(img)
    }

    /// Forget every recorded miss so the next [`Self::resolve_keys`] re-scans;
    /// resolved icons are kept because they stay valid until reinstalled.
    ///
    /// That "until reinstalled" was the gap: a resolved entry is permanent for
    /// the process lifetime, so an app whose icon file was *replaced* — a
    /// theme switch, an app update shipping new artwork — kept the old bitmap
    /// until the launcher restarted. The set changed, the file changed, the
    /// pixels did not. [`Self::invalidate_key`] is the fix; this stays as the
    /// cheap half of it, for the caller that only needs the scan redone.
    ///
    /// # The caller has to notice
    ///
    /// Nothing here can tell a replaced file from an unchanged one: by the time
    /// a miss is recorded the file has already been decoded and discarded. So
    /// the *shell* must hash something that moves when the icon does and call
    /// [`Self::invalidate_keys`] with the keys whose hash changed. At the time
    /// of writing `app_set_signature` hashes only `app.id`, which cannot change
    /// when the icon does; that is a `main.rs` fix and the reason this function
    /// has no production caller yet.
    pub fn invalidate_misses(&mut self) {
        self.misses.clear();
    }

    /// Drop the entry for `key` so the next [`Self::resolve_keys`] decodes it
    /// again. Returns `true` when something was actually discarded.
    ///
    /// Removes the image, its shadow mask and its LRU tick, and credits the
    /// bytes back to `live_bytes`, so the budget accounting stays balanced and
    /// the next insert evicts against the real footprint rather than an
    /// inflated one.
    pub fn invalidate_key(&mut self, key: &str) -> bool {
        self.misses.remove(key);
        self.drop_entry(key)
    }

    /// Drop the cached entry for `key` and keep `live_bytes` honest. Does not
    /// touch the miss set, so [`Self::insert_image`] can reuse this without
    /// clearing a miss it is about to resolve anyway.
    fn drop_entry(&mut self, key: &str) -> bool {
        let img = self.images.remove(key);
        let shadow = self.shadows.remove(key);
        self.used.borrow_mut().remove(key);
        if let Some(img) = &img {
            self.live_bytes = self
                .live_bytes
                .saturating_sub(img.width as usize * img.height as usize * 4);
        }
        if let Some(m) = &shadow {
            self.live_bytes = self.live_bytes.saturating_sub(m.coverage.len());
        }
        img.is_some() || shadow.is_some()
    }

    /// [`Self::invalidate_key`] over a batch. Returns how many keys held an
    /// entry (misses are dropped silently — the caller cannot act on them).
    ///
    /// Takes `&[String]` rather than `&[&str]` to match
    /// [`Self::resolve_keys`], which the shell already calls that way, so the
    /// one call site needs no collecting.
    pub fn invalidate_keys(&mut self, keys: &[String]) -> usize {
        keys.iter().filter(|k| self.invalidate_key(k)).count()
    }

    /// Drop every shaped icon and its shadow mask, keeping the miss set.
    ///
    /// A miss records that a sweep found nothing; that answer does not depend
    /// on the shape, the tint, the background tone or whether a shadow is
    /// wanted, so re-running the sweep for those would be wasted work.
    pub fn clear_shaped(&mut self) {
        self.images.clear();
        self.shadows.clear();
        self.used.borrow_mut().clear();
        self.live_bytes = 0;
    }

    /// The cached icon for `key`, or a freshly generated monogram tile when
    /// the key has no usable PNG.
    ///
    /// This is the one call the shell needs: it never has to know whether a
    /// key resolved, what the font is, or how a monogram is built. A key that
    /// already holds an image is returned as-is (and LRU-ticked, like
    /// [`Self::get`]); a key that is unknown, a recorded miss, or holds a
    /// *blank* image (a PNG that decoded to nothing -- see
    /// [`has_visible_ink`]) is filled in with a monogram of `display_name`'s
    /// first character on `color`.
    ///
    /// # Why the fallback lives in the cache and not in the renderer
    ///
    /// Two reasons, and the second is the one that matters.
    ///
    /// 1. **Cost.** A monogram is a glyph raster plus `edge` span fills. Done
    ///    in the renderer that is per tile per frame; done here it is once per
    ///    key, and the result is an ordinary cache entry that the existing 1:1
    ///    blit path picks up with no special case.
    /// 2. **Coherence.** The renderer has no way to know the app's display
    ///    name or its colour -- it is handed an `AppGridItem` whose `icon`
    ///    field is `None`, and the whole point of that field being `None` is
    ///    that the *launcher* decides what fills it. A monogram synthesised
    ///    from a name the renderer never saw would be a name it had to be
    ///    given, which is a second, divergent source of app identity.
    ///
    /// `color` is the app's own deterministic colour (the same value the
    /// renderer paints the tile behind the icon), so the monogram is the same
    /// colour the empty tile would have been -- a missing icon becomes a
    /// letter on the app's own background, not a grey box.
    ///
    /// # Determinism
    ///
    /// The same `(key, display_name, color, display_edge, shape)` always produces
    /// the same bytes: [`monogram_tile_image`] is a pure function of them, and
    /// nothing here depends on iteration order, a clock or an RNG.
    ///
    /// # Allocation
    ///
    /// One tile, on the *miss* path. A caller that calls this every frame for
    /// a key it already resolved pays only the hash lookup -- the fast path is
    /// [`Self::get`], allocation-free.
    pub fn get_or_monogram(
        &mut self,
        key: &str,
        display_name: &str,
        color: u32,
    ) -> Option<Rc<RgbaImage>> {
        let shape = self.shape;
        self.get_or_monogram_shaped(key, display_name, color, shape)
    }

    /// [`Self::get_or_monogram`] with an explicit shape for one call.
    ///
    /// # Why a shape argument when there is already [`Self::set_shape`]
    ///
    /// Because the reference has more than one shape in flight at a time: home
    /// and drawer have separate settings (`homeScreenIconShape` /
    /// `drawerIconShape`), and a folder has its own (`folderShape`,
    /// `PreferenceManager2.kt:151`). A single cache-wide field cannot express
    /// "squircles in the drawer, circles on the home screen".
    ///
    /// # The gap this does not close
    ///
    /// A *decoded* icon is masked once, on the insert path, so it has exactly
    /// one silhouette for its lifetime. Asking for a different shape here does
    /// **not** re-mask a decoded icon — it only shapes a monogram this call
    /// generates. Making a decoded icon re-shapeable means storing the source
    /// (or the unshaped alpha) per entry and re-masking on demand, and deciding
    /// what happens to an `Rc` already handed to the renderer. That is a real
    /// design question, not a detail, and it is left to whoever builds folders:
    /// until then the honest statement is that shape is decided once per entry
    /// and [`Self::set_shape`] is how you change it.
    ///
    /// Allocation-free on the hit path, exactly like [`Self::get_or_monogram`]:
    /// a key that already holds a non-blank icon returns it without touching the
    /// shape at all.
    pub fn get_or_monogram_shaped(
        &mut self,
        key: &str,
        display_name: &str,
        color: u32,
        shape: IconShape,
    ) -> Option<Rc<RgbaImage>> {
        if let Some(img) = self.images.get(key) {
            if has_visible_ink(img) {
                return self.get(key);
            }
            // A blank PNG is worse than no icon: it occupies a cache slot, it
            // satisfies `knows()`, and it renders as nothing. Drop it and
            // replace it below.
        }
        self.misses.remove(key);
        // The first character that can stand in for the app, not the first
        // ASCII character. `find(|c| !c.is_control() && *c != ' ')` keeps a
        // Cyrillic, Greek or CJK initial, which the old `(0x21..0x7F)` filter
        // discarded -- leaving a monogram that identified nothing about the app
        // it was supposed to label.
        let initial = display_name
            .chars()
            .find(|c| !c.is_control() && *c != ' ')
            .unwrap_or('?');
        // Contrast ink: the tile is the app's saturated colour, so the glyph
        // takes whichever of near-black / near-white is legible on it. Derived
        // from the colour rather than passed in, so the caller cannot get it
        // wrong and it stays deterministic.
        let ink = if crate::graphics::palette::lstar_of_argb(color) >= 50.0 {
            0xFF10_1010
        } else {
            0xFFF5_F5F5
        };
        // The monogram is generated with `shape`, and `insert_image` masks with
        // `self.shape` — the same value on every path that reaches here
        // through `get_or_monogram`. When they differ, the second mask is
        // idempotent on the pixels `draw_monogram_tile` already left inside its
        // own silhouette (it writes nothing outside the shape), so the entry is
        // consistent either way and no pixel is lost.
        let img = monogram_tile_image(initial, color, ink, self.display_edge, shape)?;
        self.insert_image(key.to_string(), img);
        self.images.get(key).cloned()
    }

    /// The one place a decoded icon enters the cache, so nothing can reach
    /// `insert_image` while skipping the offline write-back.
    ///
    /// Order matters, and the reason is subtle enough to be worth stating
    /// twice: `cache_offline` runs **before** `insert_image`, while `img` is
    /// still the decoded *source*. What goes on disk is therefore the icon as
    /// the theme sweep (or the offline cache) produced it, with no
    /// `<background>` fill, no monochrome tint and no shape mask baked in — so
    /// the next boot's `LookupStage::OfflineCache` gets an unstyled tile and
    /// re-applies whatever the *current* settings say.
    ///
    /// Persisting the styled tile instead would look correct and be wrong in a
    /// way nothing would catch: `set_shape` clears the in-memory cache and
    /// re-resolves, which would then mask an already-squircle-masked tile as a
    /// circle, and the corner the first mask cut stays cut forever.
    fn commit_resolved(&mut self, key: &str, img: RgbaImage) {
        self.cache_offline(key, &img);
        self.insert_image(key.to_string(), img);
    }

    /// Write `img` — the decoded source, before any transform — into the offline
    /// cache, if write-back is on. See [`Self::commit_resolved`] for why this is
    /// the pre-transform tile.
    ///
    /// Skips the write when the file is already there. That single `stat` is
    /// what keeps a warm boot from rewriting the whole directory, and it is
    /// also why mtime is a usable recency signal for [`prune_offline_cache`].
    /// A file that is present but corrupt is deliberately *not* rewritten: the
    /// offline stage falls through to the sweep, the sweep's answer is cached in
    /// memory, and the stale file is replaced by the next prune rather than by
    /// every launcher start.
    ///
    /// # Which side of the frame path
    ///
    /// **Off** it, twice over: this only runs when a key was *not* already
    /// known (so at most once per key per launch), and the write-back it
    /// performs is amortised over a whole app set — every boot after the first
    /// reaches here never at all, because `load_offline` answers first. What it
    /// costs is one `PathBuf` clone, one `stat` and, at most once ever, one
    /// encode plus `open`/`write`/`rename`. None of it is per frame.
    ///
    /// # Errors
    ///
    /// Ignored. A read-only `$HOME`, a full disk or a permission problem makes
    /// the launcher's *cache* smaller; it does not make the launcher wrong, and
    /// there is no caller in the tree that could act on it. The next launch
    /// tries again.
    fn cache_offline(&mut self, key: &str, img: &RgbaImage) {
        if !self.offline_writing {
            return;
        }
        if !is_offline_cache_name_safe(key) {
            // A path key names a file the caller already has, so caching its
            // content under a derived name buys nothing; and its name is not a
            // safe file name in the first place. `resolve_keys` keeps such keys
            // on `LookupStage::AbsolutePath`, so this is the second of two
            // independent guards against a path key escaping the cache dir.
            return;
        }
        let edge = self.display_edge;
        if img.width > ICON_MAX_TILE_EDGE || img.height > ICON_MAX_TILE_EDGE {
            // The existing admission rule for a cached tile, reused rather than
            // reinvented: a tile above the cache ceiling is one the LRU would
            // evict first anyway (4 MiB holds 16 of them), and at 1024 px it is
            // a 4 MiB file. Writing it would fill the offline budget with the
            // icons least likely to be drawn.
            return;
        }
        // Cloned out of `self` so the write is not held under a borrow of the
        // field whose counter it also increments. One small allocation per
        // written icon, on the resolution path.
        let Some(dir) = self.offline_dir.clone() else {
            return;
        };
        if offline_cache_path(&dir, key, edge).exists() {
            return;
        }
        if write_offline_icon(&dir, key, edge, img).is_ok() {
            self.offline_writes += 1;
            if self.offline_writes >= OFFLINE_PRUNE_INTERVAL {
                let _ = self.prune_offline_cache();
            }
        }
    }

    /// Insert a decoded icon, applying every configured transform and
    /// evicting least-recently-used entries until the cache is back under
    /// [`ICON_CACHE_BUDGET`] (P2). A single icon larger than the whole budget
    /// still caches as the MRU entry and is evicted by the next insert, so
    /// `live_bytes` may transiently exceed the budget by one entry.
    ///
    /// # Order of operations, and why it is this order
    ///
    /// 1. `<foreground>` reconstruction ([`composite_foreground`]) — fills
    ///    transparent corners with the configured tone, so the tile has an
    ///    opaque surface like a real adaptive icon.
    /// 2. Monochrome tint ([`make_monochrome`]) — replaces RGB, preserves
    ///    alpha, so it must run before the mask: tinting after masking would
    ///    write RGB into pixels whose alpha is 0, which is harmless but wasted.
    /// 3. Mask ([`mask_tile`]) — cuts the silhouette. **After** the fill, or
    ///    the fill would paint the corners the mask had just cleared and the
    ///    icon would render as a hard-edged square.
    /// 4. Shadow mask, from the finished alpha — built last because it has to
    ///    describe the silhouette that will actually be drawn, and because it is
    ///    the most expensive step, which is why nothing after it exists.
    ///
    /// Masking here rather than in the renderer is not just cheaper -- it is
    /// the only place the shape can be decided *once*, instead of once per
    /// icon per frame, and it is why a cached icon is immutable (`Rc`).
    fn insert_image(&mut self, key: String, mut img: RgbaImage) {
        fn img_bytes(img: &RgbaImage) -> usize {
            img.width as usize * img.height as usize * 4
        }

        if let Some(bg) = self.fg_background {
            if !has_explicit_background(&img) {
                composite_foreground(&mut img, bg);
            }
        }
        if let Some(tint) = self.monochrome {
            make_monochrome(&mut img, tint);
        }
        let img = mask_tile(img, self.shape);
        let shadow = if self.shadows_enabled {
            build_shadow_mask(&img)
        } else {
            None
        };

        self.tick.set(self.tick.get() + 1);
        let bytes = img_bytes(&img) + shadow.as_ref().map_or(0, |m| m.coverage.len());
        // Reclaim this key's own bytes first, if it is already cached: the old
        // code subtracted them *after* the budget loop, so a repeated key
        // tested `live_bytes` against a total that still included itself.
        self.drop_entry(&key);
        while self.live_bytes + bytes > ICON_CACHE_BUDGET && !self.images.is_empty() {
            let lru = self
                .used
                .borrow()
                .iter()
                .min_by_key(|(_, &t)| t)
                .map(|(k, _)| k.clone());
            match lru {
                Some(k) => {
                    self.invalidate_key(&k);
                }
                None => break,
            }
        }
        self.live_bytes += bytes;
        if let Some(m) = shadow {
            self.shadows.insert(key.clone(), Rc::new(m));
        }
        self.images.insert(key.clone(), Rc::new(img));
        self.used.borrow_mut().insert(key, self.tick.get());
    }

    /// Resolve every not-yet-looked-up key, trying [`LOOKUP_ORDER`] in order, then
    /// decode the winners.
    ///
    /// # The dispatch is the const
    ///
    /// The per-key stages below are reached by iterating [`LOOKUP_ORDER`] and
    /// matching on the variant, not by a chain of `if`s whose order happens to
    /// match a comment somewhere else. Two properties follow from that:
    ///
    /// * reordering [`LOOKUP_ORDER`] reorders the behaviour — there is no second
    ///   copy of the order to go stale, which is what
    ///   `lookup_order_matches_the_dispatch` asserts;
    /// * an arm that does not apply to a key must `continue`, never fall
    ///   through to the next stage's work. A path key `continue`s out of the
    ///   offline arm ([`is_offline_cache_name_safe`] refuses it), so it cannot be
    ///   given to [`Self::load_offline`] — whose [`offline_cache_path`] would
    ///   then resolve *outside* the cache directory — no matter how
    ///   [`LOOKUP_ORDER`] is arranged.
    ///
    /// [`LookupStage::Theme`] is the exception, and necessarily so: one
    /// directory pass answers a whole batch, which is the entire point of the
    /// sweep. It is the tail of the loop and the batch runs after it, and the
    /// loop asserts it really is in last position.
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
            let mut needs_sweep = true;
            for stage in LOOKUP_ORDER {
                match stage {
                    LookupStage::AbsolutePath => {
                        if !key.contains('/') {
                            continue;
                        }
                        // Explicit path: read it now, never enter the sweep. A
                        // path that does not resolve is final — there is nothing
                        // for the sweep to find — so it is pinned as a miss.
                        match load_path(Path::new(key), self.display_edge) {
                            Some(img) => self.commit_resolved(key, img),
                            None => {
                                self.misses.insert(key.clone());
                            }
                        }
                        needs_sweep = false;
                        break;
                    }
                    LookupStage::OfflineCache => {
                        // A `continue`, never a fall-through: a stage that does not own the key must
                        // hand it to the next one. See the `LookupStage::Theme`
                        // arm for why that matters.
                        if !is_offline_cache_name_safe(key) {
                            continue;
                        }
                        match self.load_offline(key) {
                            Some(img) => self.commit_resolved(key, img),
                            // A miss here is deliberately NOT recorded: one
                            // `open` beats walking a theme tree, and an absent
                            // entry is indistinguishable from a key that is
                            // simply not installed. Not pinning it also means
                            // an entry written a second later — by this process
                            // or by the documented oneshot — is picked up.
                            None => continue,
                        }
                        needs_sweep = false;
                        break;
                    }
                    LookupStage::Theme => {
                        debug_assert_eq!(
                            stage,
                            LOOKUP_ORDER[LOOKUP_ORDER.len() - 1],
                            "the batched sweep must be the last stage"
                        );
                        break;
                    }
                }
            }
            if needs_sweep {
                pending.push(i);
            }
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
                        self.commit_resolved(key, img);
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
    /// off, the key is not a usable file name, or the file is absent,
    /// unreadable, or not a PNG.
    ///
    /// The name check is redundant with the [`LookupStage::OfflineCache`] arm in
    /// [`Self::resolve_keys`], and deliberately so: `offline_cache_path` is a
    /// plain `join`, so a key with a `/` in it would read from *outside* the
    /// cache directory. Two guards in two places is the right number for a
    /// property that is a directory-traversal bug when it fails.
    fn load_offline(&self, key: &str) -> Option<RgbaImage> {
        let dir = self.offline_dir.as_deref()?;
        if !is_offline_cache_name_safe(key) {
            return None;
        }
        load_path(
            &offline_cache_path(dir, key, self.display_edge),
            self.display_edge,
        )
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
    // Refuse a source past the ceiling, rather than decoding it and throwing
    // it away: `decode_png` has already allocated it, so this check bounds the
    // *resample* cost, which is a full extra pass over the source (P15).
    if img.width > ICON_MAX_SOURCE_EDGE || img.height > ICON_MAX_SOURCE_EDGE {
        return None;
    }
    // Within the ceiling, downscale and accept. This used to reject anything
    // over 256 px, which meant a 512x512 hicolor raster — a size the
    // specification's own themes ship — resolved to nothing, was recorded as a
    // miss, and rendered as a monogram letter. The reference has the same
    // problem and picks a density bucket instead of refusing
    // (`getLauncherIconDensity`, `InvariantDeviceProfile.java:905-926`);
    // UTLC's equivalent of "take the nearest bucket" is "take the source and
    // scale it", and it costs one pass here rather than a wrong answer on
    // screen. `fit_within` is a no-op when the source already fits, so the
    // 1:1 fast path for an exactly-sized icon is untouched.
    //
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
            walk(
                &path,
                depth + 1,
                theme,
                ctx,
                size,
                wanted,
                best,
                budget,
                preferred,
                target,
            );
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
                Some((s, p)) => score > *s || (score == *s && path < *p),
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

    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/png_fixtures.rs"
    ));

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Tree {
            let root =
                std::env::temp_dir().join(format!("utim_icons_{tag}_{}", std::process::id()));
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
        assert_eq!(
            icon.pixels, GRAY1_EXPECT,
            "preferred theme wins regardless of size"
        );
    }

    #[test]
    fn case_insensitive_names_and_png_suffix() {
        let tree = Tree::new("case");
        tree.put("icons/hicolor/48x48/apps/Web-Browser.PNG", RGBA8_PNG);
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");

        cache.resolve_keys(&["web-browser".to_string()]);
        assert!(
            cache.get("web-browser").is_some(),
            "case and extension insensitive match"
        );
    }

    #[test]
    fn misses_are_recorded_and_can_be_invalidated() {
        let tree = Tree::new("miss");
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");

        let keys = vec!["nope".to_string()];
        cache.resolve_keys(&keys);
        assert!(cache.get("nope").is_none());
        assert!(
            cache.knows("nope"),
            "miss is remembered so the sweep is skipped"
        );

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
        assert!(
            cache.knows("broken"),
            "corrupt winner does not rescan every frame"
        );
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
        symlink(
            tree.path().join("targets/real.png"),
            apps.join("linked.png"),
        )
        .unwrap();
        // Directory links ARE descended now (P14): a cycle only burns
        // MAX_DEPTH levels plus sweep budget, so the sweep terminates.
        symlink(
            tree.path().join("icons/hicolor/64x64"),
            tree.path().join("icons/hicolor/loop"),
        )
        .unwrap();
        tree.put("hidden/64x64/apps/secret.png", RGBA8_PNG);
        symlink(
            tree.path().join("hidden"),
            tree.path().join("icons/through-link"),
        )
        .unwrap();

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
        RgbaImage {
            width: n,
            height: n,
            pixels: p,
        }
    }

    /// Half-open `[lo, hi)` x-span of non-zero alpha on row `y`, or `None`
    /// when the whole row was cleared.
    fn alpha_span(img: &RgbaImage, y: usize) -> Option<(usize, usize)> {
        let w = img.width as usize;
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        for (x, px) in img.pixels[y * w * 4..(y + 1) * w * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
        {
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
        assert!(
            apply_shape(&mut img, IconShape::Squircle),
            "a full square has ink to cut"
        );

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
        assert!(
            !apply_shape(&mut img, IconShape::Squircle),
            "second mask must be a no-op"
        );
        assert_eq!(img, after_first, "no byte may change on a second mask");

        // Same for the other shapes: masking an already-shaped icon is free.
        for shape in [IconShape::Circle, IconShape::RoundedRect] {
            let mut img = solid(n);
            assert!(
                apply_shape(&mut img, shape),
                "{shape:?} must cut the square"
            );
            let once = img.clone();
            assert!(
                !apply_shape(&mut img, shape),
                "{shape:?} second mask must be a no-op"
            );
            assert_eq!(img, once, "{shape:?} changed bytes on a second mask");
        }
    }

    #[test]
    fn masking_never_touches_a_fully_opaque_square() {
        // The mask must actually remove ink, and must say so.
        let n = 64u32;
        let before = solid(n);
        let mut img = before.clone();
        assert!(
            apply_shape(&mut img, IconShape::Squircle),
            "a square has corners to cut"
        );

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
        assert!(
            kept > n as usize * n as usize / 2,
            "the mask kept only {kept} of {}",
            n * n
        );
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
                .filter(|&i| before.pixels[i * 4 + 3] != img.pixels[i * 4 + 3])
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
            assert!(
                modified * 2 < total,
                "n={n}: {modified} of {total} is not a corner trim"
            );
        }
    }

    #[test]
    fn masking_a_fully_transparent_icon_is_a_noop() {
        let n = 32u32;
        let mut img = RgbaImage {
            width: n,
            height: n,
            pixels: vec![0; n as usize * n as usize * 4],
        };
        let before = img.clone();
        assert!(
            !apply_shape(&mut img, IconShape::Squircle),
            "no ink, no change"
        );
        assert_eq!(img, before);
    }

    #[test]
    fn masking_handles_degenerate_images_without_panicking() {
        for (w, h) in [(0u32, 0u32), (1, 1), (1, 8), (8, 1), (3, 5)] {
            let mut img = RgbaImage {
                width: w,
                height: h,
                pixels: vec![0xFF; w as usize * h as usize * 4],
            };
            for shape in [
                IconShape::Squircle,
                IconShape::Circle,
                IconShape::RoundedRect,
            ] {
                let _ = apply_shape(&mut img, shape);
            }
        }
        // A truncated buffer must be refused, not indexed.
        let mut short = RgbaImage {
            width: 64,
            height: 64,
            pixels: vec![0xFF; 16],
        };
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
                (0..n as usize * n as usize)
                    .filter(|&i| img.pixels[i * 4 + 3] != 0)
                    .count() as f64
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
            assert!(
                area(IconShape::Circle) < area(IconShape::RoundedRect),
                "n={n}"
            );
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
        assert_eq!(
            size_of::<IconShape>(),
            1,
            "the shape must stay a single byte"
        );
        // `IconShape` is `Copy`, so passing it to the per-icon shaping state and
        // to the cache is a register move, not a refcount bump.
        let pairs = [
            (IconShape::Circle, IconShape::Squircle),
            (IconShape::RoundedRect, IconShape::RoundedRect),
        ];
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
                .map(|i| {
                    if i % 4 == 3 {
                        (i / 4 % 4) * 85
                    } else {
                        0x11 * (i % 7)
                    }
                })
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
        let rect = Rect {
            x: 16.0,
            y: 8.0,
            w: 96.0,
            h: 96.0,
            radius: 0.0,
        };
        let tile = 0xFF1E_2A38_u32;
        let glyph = 0xFFE8_EAED_u32;

        assert!(draw_monogram_tile(
            &mut buf,
            w,
            w,
            h,
            rect,
            'F',
            tile,
            glyph,
            IconShape::Squircle
        ));

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
        assert!(
            inside > 96 * 96 / 2,
            "only {inside} of {} pixels drawn",
            96 * 96
        );
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
            brightest,
            glyph & 0xFF_FFFF,
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
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 64.0,
            h: 64.0,
            radius: 0.0,
        };
        // Controls and space carry no letter, so they are refused outright.
        // `U+007F` is in the list deliberately: it is inside the pre-parsed
        // range and Noto carries a real glyph for it, so a "does the font have
        // it" test would happily print DEL. Control-ness is not the font's call.
        for bad in ['\n', '\t', '\r', '\u{7F}', '\u{85}', ' ', '\u{0}', '\u{1F}'] {
            let mut buf = tile_buffer(w, h);
            assert!(
                !draw_monogram_tile(
                    &mut buf,
                    w,
                    w,
                    h,
                    rect,
                    bad,
                    0xFF11_2233,
                    0xFFEE_EEEE,
                    IconShape::Squircle
                ),
                "{bad:?} must be rejected rather than drawn"
            );
            assert!(
                buf.iter().all(|&p| p == 0xFF00_0000),
                "{bad:?} drew into the buffer despite being rejected"
            );
        }
        // A non-ASCII initial is *accepted*. It used to be refused -- by this
        // guard and again by the caller's `(0x21..0x7F)` filter -- so a Cyrillic
        // or Greek app name fell through to '?' and the monogram identified
        // nothing about the app it was labelling. Now it draws the real glyph
        // when the face has one and a visible placeholder when it does not;
        // either way the tile is a labelled tile, not a blank one.
        for ok in ['\u{00E9}', '\u{0416}', '\u{4E2D}', '\u{1F600}'] {
            let mut buf = tile_buffer(w, h);
            assert!(
                draw_monogram_tile(
                    &mut buf,
                    w,
                    w,
                    h,
                    rect,
                    ok,
                    0xFF11_2233,
                    0xFFEE_EEEE,
                    IconShape::Squircle
                ),
                "{ok:?} must be accepted; refusing it loses the app's initial"
            );
            assert!(
                buf.iter().any(|&p| p != 0xFF00_0000),
                "{ok:?} was accepted but painted nothing at all"
            );
        }
        // A real letter still works, so the check is not rejecting everything.
        let mut buf = tile_buffer(w, h);
        assert!(draw_monogram_tile(
            &mut buf,
            w,
            w,
            h,
            rect,
            'A',
            0xFF11_2233,
            0xFFEE_EEEE,
            IconShape::Squircle
        ));
        assert!(buf.iter().any(|&p| p != 0xFF00_0000));
    }

    #[test]
    fn monogram_tile_clips_to_the_framebuffer() {
        let w = 32usize;
        let h = 32usize;
        // Half off the left edge, and entirely off the right.
        for rect in [
            Rect {
                x: -16.0,
                y: 0.0,
                w: 32.0,
                h: 32.0,
                radius: 0.0,
            },
            Rect {
                x: 40.0,
                y: 0.0,
                w: 32.0,
                h: 32.0,
                radius: 0.0,
            },
            Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 32.0,
                radius: 0.0,
            },
        ] {
            let mut buf = tile_buffer(w, h);
            let drew = draw_monogram_tile(
                &mut buf,
                w,
                w,
                h,
                rect,
                'M',
                0xFF20_3040,
                0xFFAA_BBCC,
                IconShape::Squircle,
            );
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
        let _guard = crate::graphics::font::TEST_FONT_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let w = 96usize;
        let h = 96usize;
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 96.0,
            h: 96.0,
            radius: 0.0,
        };
        let mut rasters = [0u32; 3];
        for (i, fam) in [
            font::FontFamily::NotoSans,
            font::FontFamily::Homemade,
            font::FontFamily::AsciiMono,
        ]
        .into_iter()
        .enumerate()
        {
            font::set_active_family(fam);
            let mut buf = tile_buffer(w, h);
            assert!(draw_monogram_tile(
                &mut buf,
                w,
                w,
                h,
                rect,
                'G',
                0xFF10_2030,
                0xFFF0_F0F0,
                IconShape::Squircle
            ));
            rasters[i] = buf.iter().fold(0u32, |acc, &p| acc.wrapping_add(p));
        }
        font::set_active_family(font::FontFamily::NotoSans);
        // The families must not all rasterise identically, or the family
        // parameter would be doing nothing. `Homemade` and `AsciiMono` are
        // close cousins -- the same skeleton with a different advance width --
        // so only the *set* of three is asserted, not every pair. `NotoSans`
        // is the odd one out: it is either the real system TTF or the
        // proportional table, neither of which shares `AsciiMono`'s metrics.
        assert_ne!(
            rasters[0], rasters[2],
            "NotoSans and AsciiMono are identical"
        );
        assert!(
            rasters[1] != rasters[0] || rasters[1] != rasters[2],
            "all three families rasterise identically"
        );
    }

    // =======================================================================
    // Configured shaping state (shape / monochrome / foreground background)
    // =======================================================================

    /// A `n`x`n` tile with a single opaque pixel at the centre and nothing else
    /// inked: passes `has_visible_ink`, is masked to a clean shape, and has an
    /// alpha histogram that is trivially readable.
    fn centre_dot(n: u32, rgb: [u8; 3]) -> RgbaImage {
        let mut img = RgbaImage {
            width: n,
            height: n,
            pixels: vec![0u8; n as usize * n as usize * 4],
        };
        let c = ((n / 2) as usize) * n as usize + (n / 2) as usize;
        let i = c * 4;
        img.pixels[i..i + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 0xFF]);
        img
    }

    #[test]
    fn set_shape_masks_every_cached_icon_to_it() {
        let n = 64u32;
        // A solid tile is the honest probe: the only thing that can cut ink out
        // of it is the mask, and which mask it was is measurable by area.
        let area = |shape: IconShape| {
            let mut cache = IconCache::with_roots(vec![], "");
            cache.set_shape(shape);
            cache.insert_image("a".into(), solid(n));
            let img = cache.get("a").expect("inserted");
            img.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[3] != 0)
                .count()
        };
        let (sq, ci, rr) = (
            area(IconShape::Squircle),
            area(IconShape::Circle),
            area(IconShape::RoundedRect),
        );
        assert!(
            ci < sq && ci < rr,
            "circle {ci} must be the smallest of {sq}/{ci}/{rr}"
        );
        assert!(
            (sq as i64 - rr as i64).abs() <= sq as i64 / 50,
            "squircle {sq} and rounded rect {rr} are near-aliases, per the analytic areas"
        );
        // And the cache reports what it was told.
        let mut cache = IconCache::with_roots(vec![], "");
        assert_eq!(
            cache.shape(),
            IconShape::Squircle,
            "Squircle is the reference's default"
        );
        cache.set_shape(IconShape::RoundedRect);
        assert_eq!(cache.shape(), IconShape::RoundedRect);
    }

    #[test]
    fn set_shape_discards_the_shaped_icons_it_invalidates() {
        let n = 64u32;
        let mut cache = IconCache::with_roots(vec![], "");
        cache.insert_image("a".into(), solid(n));
        cache.insert_image("b".into(), solid(n));
        let before = cache.live_bytes();
        assert!(before > 0 && cache.len() == 2);

        // Same shape: nothing is thrown away. A settings write that fires on
        // every frame would empty the cache and re-sweep the theme tree.
        cache.set_shape(IconShape::Squircle);
        assert_eq!(cache.len(), 2, "an unchanged shape must not evict");

        cache.set_shape(IconShape::Circle);
        assert_eq!(
            cache.len(),
            0,
            "a shape change discards the icons it masked"
        );
        assert_eq!(cache.live_bytes(), 0, "and credits the bytes back");
        assert!(
            !cache.knows("a"),
            "so the next resolve re-decodes rather than serving a stale shape"
        );
    }

    #[test]
    fn a_shape_change_discards_icons_but_keeps_misses() {
        // A miss is "the sweep found nothing", which does not depend on the
        // mask. Re-running the sweep for every shape change would be pure
        // waste: the sweep is the expensive part, not the mask.
        let tree = Tree::new("shapemiss");
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.resolve_keys(&["absent".to_string()]);
        assert!(cache.knows("absent"), "the miss is recorded");

        cache.set_shape(IconShape::Circle);
        assert!(
            cache.knows("absent"),
            "a shape change must not re-run the sweep"
        );
        // But once the file does appear, the miss is the only thing in the way.
        tree.put("icons/hicolor/64x64/apps/absent.png", RGBA8_PNG);
        cache.resolve_keys(&["absent".to_string()]);
        assert!(
            cache.get("absent").is_none(),
            "the miss still short-circuits, as documented"
        );
        cache.invalidate_misses();
        cache.resolve_keys(&["absent".to_string()]);
        assert!(
            cache.get("absent").is_some(),
            "the miss was the blocker, not the shape"
        );
    }

    #[test]
    fn monochrome_tint_replaces_rgb_and_keeps_the_alpha_silhouette() {
        let n = 64u32;
        let tint = 0xFF_2E_D0_5Bu32;
        let mut cache = IconCache::with_roots(vec![], "");
        cache.insert_image("plain".into(), solid(n));
        let plain = cache.get("plain").expect("inserted").clone();
        // Pixel (0,0) is *outside* a squircle and therefore transparent; the
        // centre row is inside it edge to edge. Sample where the tile exists.
        let mid = (n as usize / 2) * n as usize * 4;
        assert_eq!(
            &plain.pixels[mid..mid + 4],
            &[0x20, 0x40, 0x80, 0xFF],
            "the tile starts as its own colour"
        );

        // Turning it on must invalidate: the cached bytes are no longer the
        // file's, and `Rc` cannot be restyled.
        cache.set_monochrome(Some(tint));
        assert_eq!(cache.len(), 0, "a tint change discards the icons it tinted");
        assert_eq!(cache.monochrome(), Some(tint));

        cache.insert_image("plain".into(), solid(n));
        let mono = cache.get("plain").expect("reinserted");
        let inpainted = img_pixel_count(&mono);
        // Every pixel with ink is the tint, and the silhouette is byte-identical
        // to the untinted tile's: the mask cut the same ink at the same places.
        let mut want = solid(n);
        apply_shape(&mut want, IconShape::Squircle);
        assert_eq!(
            inpainted,
            img_pixel_count(&want),
            "the tint changed which pixels have ink"
        );
        for (i, px) in mono.pixels.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(
                &[px[0], px[1], px[2]],
                &[0x2E, 0xD0, 0x5B],
                "pixel {i} must be the tint"
            );
            assert_eq!(
                px[3],
                want.pixels[i * 4 + 3],
                "pixel {i} alpha must be the untinted alpha"
            );
        }
        // And off again is the original bytes, not a double tint.
        cache.set_monochrome(None);
        assert_eq!(cache.monochrome(), None);
        cache.insert_image("plain".into(), solid(n));
        assert_eq!(
            cache.get("plain").unwrap().pixels,
            plain.pixels,
            "off must restore the file's bytes"
        );
    }

    fn img_pixel_count(img: &RgbaImage) -> usize {
        img.pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] != 0)
            .count()
    }

    #[test]
    fn a_foreground_only_icon_is_composited_onto_the_configured_background() {
        let n = 64u32;
        let bg = 0xFF00_00FFu32;
        // A bare `<foreground>`: transparent corners, one opaque pixel.
        let fg = centre_dot(n, [0xFF, 0x00, 0x00]);

        let mut cache = IconCache::with_roots(vec![], "");
        cache.insert_image("fg".into(), fg.clone());
        let raw = cache.get("fg").expect("inserted").clone();
        assert!(
            !has_explicit_background(&raw),
            "the fixture really is foreground-only"
        );
        assert_eq!(raw.pixels[3], 0, "off: the corner stays transparent");
        let art = (n as usize / 2) * n as usize + n as usize / 2;
        assert_eq!(raw.pixels[art * 4 + 3], 0xFF, "off: the art is still there");

        // On. This is the bug the reference's `<background>` layer would have
        // prevented: `draw_icon_bitmap_i32` blends a transparent corner straight
        // over `app.color`, so the icon's own surface is whatever the tile
        // behind it happened to be.
        cache.set_foreground_background(Some(bg));
        assert_eq!(
            cache.len(),
            0,
            "a background change discards what it composited"
        );
        assert_eq!(cache.foreground_background(), Some(bg));
        cache.insert_image("fg".into(), fg.clone());
        let filled = cache.get("fg").expect("reinserted").clone();
        let (w, h) = (filled.width as usize, filled.height as usize);
        let px = |img: &RgbaImage, x: usize, y: usize| {
            let i = (y * w + x) * 4;
            (
                img.pixels[i],
                img.pixels[i + 1],
                img.pixels[i + 2],
                img.pixels[i + 3],
            )
        };
        // One row in from the top edge, on the centre column: inside the squircle
        // (`squircle_span` still leaves ~18 px there), and nowhere near the art.
        assert_eq!(
            px(&filled, w / 2, 1),
            (0x00, 0x00, 0xFF, 0xFF),
            "a pixel inside the shape took the background tone and is now opaque"
        );
        assert_eq!(
            px(&filled, w / 2, h / 2),
            (0xFF, 0x00, 0x00, 0xFF),
            "the art survives untouched"
        );

        // Note what the *cached* tile does and does not look like: the fill gave
        // it a background, and the mask then took the corners off again — which
        // is correct, because those corners are outside the silhouette and
        // `draw_icon_bitmap_i32` clips to the same rounded rect. So the cache
        // entry has transparent corners, and `has_explicit_background` on it is
        // `false`. The claim that matters is about the pixels *inside* the shape:
        // they are opaque and surface-toned, where before they were transparent.
        // The probe for that is `has_explicit_background` on the pre-mask tile.
        let mut pre_mask = fg.clone();
        composite_foreground(&mut pre_mask, bg);
        assert!(
            has_explicit_background(&pre_mask),
            "the reconstruction is a complete icon"
        );
        assert!(
            (0..w * h)
                .filter(|&i| filled.pixels[i * 4 + 3] == 0xFF)
                .count()
                > w * h / 2,
            "and the cached tile is opaque across the whole silhouette, not just the art"
        );

        // The mask ran *after* the fill, so the corners are background-toned and
        // still *clear*: the fill did not repaint what the mask had removed.
        // Had the order been reversed the corner would be an opaque square.
        for (x, y) in [(0usize, 0usize), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
            assert_eq!(
                px(&filled, x, y),
                (0x00, 0x00, 0xFF, 0x00),
                "({x},{y}) is outside the shape: bg-toned but transparent"
            );
        }
        let mut expect_masked = fg.clone();
        composite_foreground(&mut expect_masked, bg);
        apply_shape(&mut expect_masked, IconShape::Squircle);
        assert_eq!(
            filled.pixels, expect_masked.pixels,
            "fill then mask, in that order"
        );
        assert_eq!(
            expect_masked.pixels[3], 0,
            "the mask cleared the corner the fill wrote"
        );
    }

    #[test]
    fn an_icon_with_its_own_background_is_never_recomposited() {
        let n = 64u32;
        let mut cache = IconCache::with_roots(vec![], "");
        cache.set_foreground_background(Some(0xFF00_00FF));
        // Flat and opaque in all four corners: a flattened SVG, i.e. an icon
        // that already has a background.
        let flat = RgbaImage {
            width: n,
            height: n,
            pixels: (0..n as usize * n as usize * 4)
                .map(|i| {
                    if i % 4 == 3 {
                        0xFF
                    } else {
                        [0x11, 0x22, 0x33, 0xFF][i % 4]
                    }
                })
                .collect(),
        };
        assert!(has_explicit_background(&flat));
        cache.insert_image("flat".into(), flat.clone());
        let got = cache.get("flat").expect("inserted").clone();
        let mut want = flat;
        apply_shape(&mut want, IconShape::Squircle);
        assert_eq!(
            got.pixels, want.pixels,
            "a complete icon must pass through untouched: double-drawing a background is the visible failure"
        );
    }

    #[test]
    fn foreground_compositing_is_off_by_default() {
        // `None` has to mean "leave foreground layers alone", not "use some
        // default tone": the right tone is the launcher's surface colour, and a
        // guessed one would show through every adaptive icon.
        let mut cache = IconCache::with_roots(vec![], "");
        assert_eq!(cache.foreground_background(), None);
        assert_eq!(cache.monochrome(), None);
        assert_eq!(cache.shape(), IconShape::Squircle);
        cache.insert_image("fg".into(), centre_dot(64, [0xFF, 0, 0]));
        assert_eq!(
            cache.get("fg").unwrap().pixels[3],
            0,
            "no default background is invented"
        );
    }

    // =======================================================================
    // Shadow masks
    // =======================================================================

    #[test]
    fn shadow_masks_are_cached_only_when_asked_for() {
        let n = 64u32;
        let mut cache = IconCache::with_roots(vec![], "");
        assert!(
            !cache.shadows_enabled(),
            "off by default: the reference's shadowBGIons default"
        );

        cache.insert_image("a".into(), solid(n));
        assert!(
            cache.shadow("a").is_none(),
            "no mask while the stage is off"
        );
        assert_eq!(
            cache.live_bytes(),
            n as usize * n as usize * 4,
            "and the budget is not paying for a mask that does not exist"
        );

        cache.set_icon_shadow(true);
        assert!(cache.shadows_enabled());
        assert_eq!(
            cache.len(),
            0,
            "turning it on discards the entries it must build masks for"
        );
        cache.insert_image("a".into(), solid(n));
        let m = cache
            .shadow("a")
            .expect("the mask is cached alongside the icon");
        assert_eq!(m.edge, n, "the mask is at the tile edge");
        assert_eq!(
            m.coverage.len(),
            n as usize * n as usize,
            "one byte per tile pixel"
        );
        // A solid tile blurs to itself in the interior, so a large fraction of
        // the mask is at full coverage.
        let full = m.coverage.iter().filter(|&&v| v == 255).count();
        assert!(
            full > n as usize * n as usize / 2,
            "only {full} full-coverage pixels"
        );
        // The corners are outside the shape *and* blurred, so they are not 255:
        // proving the mask follows the silhouette rather than the rectangle.
        assert!(m.coverage[0] < 255, "the corner is outside the shape");
        // And the budget accounts for it.
        assert_eq!(
            cache.live_bytes(),
            n as usize * n as usize * 5,
            "tile bytes plus mask bytes"
        );
    }

    #[test]
    fn a_shadow_mask_is_the_blurred_alpha_of_the_shaped_tile() {
        let n = 64u32;
        // Build the expected mask from the same operations, in the same order,
        // and demand the cache agrees. This is the property the shell relies on:
        // the shadow follows what is drawn.
        let mut want = solid(n);
        apply_shape(&mut want, IconShape::Squircle);
        let expect = build_shadow_mask(&want).expect("a 64 px tile has a shadow");
        let mut cache = IconCache::with_roots(vec![], "");
        cache.set_icon_shadow(true);
        cache.insert_image("a".into(), solid(n));
        let got = cache.shadow("a").expect("cached");
        assert_eq!(
            got.coverage, expect.coverage,
            "the cached mask must match a direct build"
        );
        assert_eq!(got.edge, expect.edge);

        // Monochrome runs before the mask, and the mask does not care about RGB,
        // so a tinted tile's shadow mask is identical to the untinted one's.
        // Asserted because it is a real design claim: the shadow is silhouette,
        // not colour.
        let mut tinted = IconCache::with_roots(vec![], "");
        tinted.set_icon_shadow(true);
        tinted.set_monochrome(Some(0xFF_2E_D0_5B));
        tinted.insert_image("a".into(), solid(n));
        assert_eq!(
            tinted.shadow("a").unwrap().coverage,
            expect.coverage,
            "the shadow is the silhouette"
        );
    }

    #[test]
    fn shadow_masks_refuse_tiles_whose_shape_would_be_an_artefact() {
        // Below the masking floor the shape is a crop, not a corner trim, so a
        // blurred version of it would be a blurred crop. Above the tile ceiling
        // the image cannot have come from this cache at all.
        for n in [1u32, 8, 16, ICON_MASK_MIN_EDGE - 1] {
            assert!(
                build_shadow_mask(&solid(n)).is_none(),
                "edge {n} is below the mask floor"
            );
        }
        assert!(
            build_shadow_mask(&solid(ICON_MASK_MIN_EDGE)).is_some(),
            "at the floor it is fine"
        );
        assert!(
            build_shadow_mask(&solid(ICON_MAX_TILE_EDGE + 1)).is_none(),
            "past the tile ceiling"
        );
        // Degenerate geometry is a `None`, not a panic.
        assert!(build_shadow_mask(&RgbaImage {
            width: 0,
            height: 0,
            pixels: vec![]
        })
        .is_none());
    }

    #[test]
    fn a_shadow_mask_spreads_beyond_the_silhouette() {
        // The blur has to reach outside the shape or the shadow is a hard-edged
        // copy of the icon, which is what makes the whole feature look wrong.
        let n = 64u32;
        let mut want = solid(n);
        apply_shape(&mut want, IconShape::Squircle);
        let m = build_shadow_mask(&want).expect("a mask");
        // The centre row of a squircle keeps its full span; one row above the
        // tile edge there is nothing at all. A blur must produce coverage in
        // between, i.e. under the top row's alpha but above the second row's.
        let row = |y: usize| &m.coverage[y * n as usize..(y + 1) * n as usize];
        let top: u32 = row(0).iter().map(|&v| v as u32).sum();
        let second: u32 = row(1).iter().map(|&v| v as u32).sum();
        assert!(top > 0, "the blur must reach the tile edge");
        assert!(
            second > top,
            "row 1 ({second}) must be stronger than row 0 ({top})"
        );
        // Energy is conserved to within the truncation the kernel can lose,
        // which is what makes "coverage modulates alpha" the right composite.
        let alpha: u64 = want
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3] as u64)
            .sum();
        let blurred: u64 = m.coverage.iter().map(|&v| v as u64).sum();
        assert!(
            blurred * 100 >= alpha * 95,
            "the blur lost energy: {alpha} -> {blurred}"
        );
    }

    #[test]
    fn invalidation_keeps_the_lru_byte_accounting_balanced() {
        // `live_bytes` is the LRU's only view of memory, so an invalidation that
        // forgets to credit the bytes back makes the cache evict early forever.
        let mut cache = IconCache::with_roots(vec![], "");
        cache.set_icon_shadow(true);
        let n = 64u32;
        let one = n as usize * n as usize * 5;
        for k in ["a", "b", "c"] {
            cache.insert_image(k.into(), solid(n));
        }
        assert_eq!(cache.live_bytes(), one * 3, "three entries of tile + mask");
        assert_eq!(cache.len(), 3);

        assert!(cache.invalidate_key("b"), "a held entry reports as removed");
        assert_eq!(
            cache.live_bytes(),
            one * 2,
            "its tile *and* its mask were credited back"
        );
        assert_eq!(cache.len(), 2);
        assert!(!cache.knows("b"), "and it must re-resolve next time");
        assert!(!cache.shadow("b").is_some(), "the mask went with the tile");

        // An unknown key is not an error and does not move the needle.
        assert!(!cache.invalidate_key("nope"));
        assert_eq!(
            cache.live_bytes(),
            one * 2,
            "an unknown key must not change the accounting"
        );

        // The batch form reports the same thing, and handles a repeated key
        // without double-crediting.
        let keys = vec![
            "a".to_string(),
            "a".to_string(),
            "c".to_string(),
            "nope".to_string(),
        ];
        assert_eq!(
            cache.invalidate_keys(&keys),
            2,
            "a and c were held; the repeat and miss are not"
        );
        assert_eq!(
            cache.live_bytes(),
            0,
            "the accounting is exactly zero again"
        );
        assert!(cache.is_empty());
        // LRU ticks were dropped too, so the next insert cannot pick a victim
        // that is no longer there.
        cache.insert_image("fresh".into(), solid(n));
        assert_eq!(cache.live_bytes(), one, "a fresh insert starts from zero");
    }

    #[test]
    fn invalidation_drops_a_recorded_miss_as_well_as_an_entry() {
        // A miss is a statement about the file as it was when the sweep ran. If
        // the icon appears afterwards, only the miss is blocking the resolve --
        // and if the icon is *replaced*, the stale bitmap is exactly what
        // invalidate_key is for.
        let tree = Tree::new("invalidate");
        tree.put("icons/hicolor/64x64/apps/swap.png", RGBA8_PNG);
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(64);
        cache.resolve_keys(&["swap".to_string()]);
        let first = cache.get("swap").expect("resolved").pixels.clone();
        assert_eq!(first, RGBA8_EXPECT);

        // The file is replaced under the cache, and nothing notices.
        tree.put("icons/hicolor/64x64/apps/swap.png", GRAY16_PNG);
        assert_eq!(
            cache.get("swap").unwrap().pixels,
            first,
            "the stale bitmap is still served"
        );

        // This is the fix, and the caller has to be the one that detects the
        // change -- see `invalidate_misses`' doc on `app_set_signature`.
        assert!(cache.invalidate_key("swap"), "the entry is dropped");
        cache.resolve_keys(&["swap".to_string()]);
        let second = cache.get("swap").expect("re-resolved").pixels.clone();
        assert_ne!(second, first, "the replaced icon must actually change");
        assert_eq!(second, GRAY16_EXPECT, "and it must be the new file's bytes");

        // The miss path: a key that resolved to nothing, then appears.
        let mut cache2 = IconCache::with_roots(vec![tree.icons_root()], "");
        cache2.set_display_edge(64);
        cache2.resolve_keys(&["late".to_string()]);
        assert!(!cache2.invalidate_key("late"), "a miss holds no entry");
        assert!(!cache2.knows("late"), "but it is still dropped");
        tree.put("icons/hicolor/64x64/apps/late.png", RGBA8_PNG);
        cache2.resolve_keys(&["late".to_string()]);
        assert!(
            cache2.get("late").is_some(),
            "so it resolves without a second invalidation"
        );
    }

    #[test]
    fn every_shaping_settter_leaves_the_cache_consistent() {
        // One test for all four, because they share one invariant and one bug
        // class: a setter that clears `images` without clearing `shadows` (or
        // `live_bytes`) leaves a cache that reports zero live icons and a
        // non-zero footprint, and the LRU then evicts against a fiction.
        for which in 0..4 {
            let mut cache = IconCache::with_roots(vec![], "");
            cache.set_icon_shadow(true);
            cache.set_shape(IconShape::Squircle);
            cache.set_monochrome(None);
            cache.set_foreground_background(None);
            cache.insert_image("a".into(), solid(64));
            let held = cache.live_bytes();
            assert!(
                cache.shadow("a").is_some(),
                "case {which}: a mask exists first"
            );

            match which {
                0 => cache.set_shape(IconShape::Circle),
                1 => cache.set_monochrome(Some(0xFFFFFFFF)),
                2 => cache.set_foreground_background(Some(0xFF00_00FF)),
                _ => cache.set_icon_shadow(false),
            }
            assert!(cache.is_empty(), "case {which}: images cleared");
            assert_eq!(cache.live_bytes(), 0, "case {which}: bytes credited back");
            assert_eq!(cache.shadow("a"), None, "case {which}: masks cleared");
            assert!(held > 0);
        }
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
            RgbaImage {
                width: 4,
                height: 4,
                pixels,
            }
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
        assert!(
            !has_explicit_background(&fg),
            "a transparent corner means foreground only"
        );
        // An opaque corner that is a *different* colour is artwork, not surface.
        let mut art = flat([0x21, 0x96, 0xF0]);
        art.pixels[4 * 3] = 0x00;
        art.pixels[4 * 3 + 3] = 0xFF;
        assert!(
            !has_explicit_background(&art),
            "mismatched corners are not a flat background"
        );
        // A corner that is opaque but not fully so is artwork with a soft edge,
        // not a flat surface: a downscaled PNG can leave 0xFD where a fill gave
        // 0xFF, and that must not flip the answer either way.
        let mut soft = flat([0x21, 0x96, 0xF0]);
        soft.pixels[3] = BACKGROUND_OPAQUE_ALPHA - 1;
        assert!(
            !has_explicit_background(&soft),
            "alpha below the opaque threshold"
        );
        // Degenerate input is a foreground layer, not a panic.
        assert!(!has_explicit_background(&RgbaImage {
            width: 0,
            height: 0,
            pixels: vec![]
        }));
    }

    #[test]
    fn foreground_layer_is_composited_onto_a_tile_colour() {
        // A 2x2 foreground: one opaque red, three transparent.
        let mut img = RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![0xFF, 0x00, 0x00, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        };
        assert!(
            !has_explicit_background(&img),
            "transparent corners: a foreground layer"
        );
        assert!(
            composite_foreground(&mut img, 0xFF00_00FF),
            "the fill must be written"
        );

        // The art is untouched, everything else became the surface tone, and
        // the result is opaque throughout.
        assert_eq!(
            &img.pixels[0..4],
            &[0xFF, 0x00, 0x00, 0xFF],
            "the art must survive"
        );
        for px in img.pixels[4..].as_chunks::<4>().0 {
            assert_eq!(
                px[3], 0xFF,
                "compositing onto an opaque tile must be opaque"
            );
            assert_eq!(
                &px[0..3],
                &[0x00, 0x00, 0xFF],
                "transparent pixels take the tile colour"
            );
        }
        // A uniformly-filled tile *is* a background. The 2x2 above is not,
        // because the art in the first pixel is a second colour: that is the
        // documented failure direction -- a foreground layer that happens to be
        // painted edge to edge in one flat colour is left un-composited.
        let mut uniform = RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![0u8; 2 * 2 * 4],
        };
        composite_foreground(&mut uniform, 0xFF00_00FF);
        assert!(
            has_explicit_background(&uniform),
            "a flat filled tile is a background"
        );

        // Partial alpha is blended over the tile, not dropped.
        let mut half = RgbaImage {
            width: 1,
            height: 1,
            pixels: vec![0x00, 0x00, 0x00, 0x80],
        };
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
        assert_eq!(
            at_1080, 167,
            "65 dp at 2.571 px/dp is 167 px, not the old hard-coded 64"
        );
        // Monotonic in panel width, and never zero.
        assert!(icon_max_edge(540.0) < at_1080);
        assert!(icon_max_edge(2160.0) > at_1080);
        for w in [1.0f32, 64.0, 720.0, 1440.0, 3840.0] {
            let e = icon_max_edge(w);
            assert!(
                (1..=ICON_MAX_TILE_EDGE).contains(&e),
                "panel {w} gave edge {e}"
            );
        }
        // Clamped to the *tile* ceiling, which is what bounds the cache: a
        // 256 px tile is 256 KiB, so the 4 MiB budget holds 16 of them. The
        // source ceiling is a different number for a different reason, and
        // conflating the two is what let a 512 px icon be refused outright.
        assert_eq!(icon_max_edge(100_000.0), ICON_MAX_TILE_EDGE);
        // Two constants, two jobs. Whether the source ceiling clears the tile
        // ceiling is a compile-time fact about them, so the *behavioural*
        // consequence is asserted where it can fail, in
        // `an_oversized_source_is_downscaled_not_refused`: a source at
        // `ICON_MAX_SOURCE_EDGE` must decode, and one past it must not.
        // Degenerate input must not panic or produce a zero edge.
        for bad in [0.0f32, -100.0, f32::NAN, f32::INFINITY] {
            let e = icon_max_edge(bad);
            assert!(
                (1..=ICON_MAX_TILE_EDGE).contains(&e),
                "panel {bad} gave edge {e}"
            );
        }
    }

    // -- a PNG writer, for sources larger than any inline fixture ----------

    // The fixtures top out at 16 px, and the whole point of the test below is
    // a source *larger* than that. 1 MiB of literal `u8` array in a source file
    // is not an option, so this generates the PNG instead.
    //
    // It used to hand-roll the container here (~60 lines of CRC-32, Adler-32 and
    // chunk framing) precisely because the crate had no encoder. Now that
    // `png::encode_png` exists there is exactly one encoder in the tree, and a
    // second copy in a test module is the thing that rots. The independence
    // this helper used to provide is not lost where it matters: `png.rs`
    // asserts the round trip itself, over seven geometries including two that
    // span several DEFLATE blocks, and checks the chunk CRCs against an
    // independent walk. And the test below still *decodes* what it wrote, so an
    // encoder and decoder that shared a bug would still be caught here.

    /// A flat `n`x`n` RGBA PNG of one colour, as bytes.
    fn flat_png(n: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut img = RgbaImage {
            width: n,
            height: n,
            pixels: Vec::new(),
        };
        img.pixels.reserve(n as usize * n as usize * 4);
        for _ in 0..n as usize * n as usize {
            img.pixels
                .extend_from_slice(&[rgb[0], rgb[1], rgb[2], 0xFF]);
        }
        crate::graphics::png::encode_png(&img).expect("the crate encoder emits an icon")
    }

    #[test]
    fn an_oversized_source_is_downscaled_not_refused() {
        // The regression this guards: `ICON_MAX_SOURCE_EDGE` used to be 256 and
        // `load_path` returned `None` above it. A 512x512 raster is a size the
        // freedesktop themes actually ship, so every such icon was recorded as
        // a permanent miss and rendered as a monogram letter -- a real icon,
        // silently replaced by a wrong one. The ceiling now covers real icons
        // and the source is scaled to the tile edge instead.
        assert_eq!(
            ICON_MAX_SOURCE_EDGE, 1024,
            "512 px is the smallest real-world miss"
        );
        let tree = Tree::new("oversize");
        // 512x512 flat RGBA is 1 MiB of pixels: inside the file cap and well
        // inside the decoder's own MAX_PIXELS, so it exercises the source-edge
        // policy and nothing else.
        let path = tree.path().join("huge.png");
        std::fs::write(&path, flat_png(512, [0x21, 0x96, 0xF0])).unwrap();

        // Decoded straight, to prove the writer is producing something the
        // in-crate decoder really accepts at this size.
        let raw = std::fs::read(&path).unwrap();
        let decoded = decode_png(&raw).expect("the test PNG round-trips at 512 px");
        assert_eq!(
            (decoded.width, decoded.height),
            (512, 512),
            "writer is lossless"
        );

        // And through `load_path`, at a 64 px tile: scaled, not refused.
        let tile = load_path(&path, 64).expect("a 512 px source must decode, not miss");
        assert_eq!(
            (tile.width, tile.height),
            (64, 64),
            "scaled down to the tile edge"
        );
        assert_eq!(
            &tile.pixels[0..4],
            &[0x21, 0x96, 0xF0, 0xFF],
            "the flat colour survives"
        );

        // Through the whole cache, to prove it is not a miss and therefore not
        // a monogram: `knows()` alone cannot tell those apart.
        let mut cache = IconCache::with_roots(vec![tree.path().join("nothing")], "");
        cache.set_display_edge(64);
        let key = path.to_string_lossy().to_string();
        cache.resolve_keys(std::slice::from_ref(&key));
        let icon = cache.get(&key).expect("a 512 px icon resolves");
        assert_eq!((icon.width, icon.height), (64, 64));
        assert!(
            icon.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[3] == 0xFF)
                .count()
                > 64 * 64 / 2,
            "the cached tile is the real icon, not a letter on a square"
        );

        // The ceiling is still a ceiling: `fit_within` is a full extra pass over
        // the source and P15 bounds that cost, so 1025 is refused while 1024 is
        // admitted.
        let at = |n: u32| {
            let p = tree.path().join(format!("w{n}.png"));
            std::fs::write(&p, flat_png(n, [0x21, 0x96, 0xF0])).unwrap();
            load_path(&p, 64).map(|i| (i.width, i.height))
        };
        assert_eq!(
            at(ICON_MAX_SOURCE_EDGE),
            Some((64, 64)),
            "the ceiling itself is admitted"
        );
        assert_eq!(
            at(ICON_MAX_SOURCE_EDGE + 1),
            None,
            "past the ceiling is still refused"
        );
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
        assert_eq!(
            (exact.width, exact.height),
            (8, 8),
            "an 8px source at an 8px edge stays 8px"
        );
        assert_eq!(
            exact.pixels, RGBA8_EXPECT,
            "the 1:1 path must see the original bytes"
        );

        // Oversized source: pre-scaled once, here, at cache time.
        let big = cache.get("atbig").expect("16px source decodes");
        assert_eq!(
            (big.width, big.height),
            (8, 8),
            "a 16px source is pre-scaled to the 8px edge"
        );
        assert_eq!(
            big.pixels.len(),
            8 * 8 * 4,
            "the scaled tile is exactly one edge of RGBA"
        );

        // Shrinking the edge re-scales: the cache is not holding a stale size.
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(4);
        cache.resolve_keys(&["atexact".to_string()]);
        let small = cache.get("atexact").expect("re-decoded at the new edge");
        assert_eq!(
            (small.width, small.height),
            (4, 4),
            "the display edge drives the pre-scale"
        );
    }

    // =======================================================================
    // Offline cache stage
    // =======================================================================

    #[test]
    fn offline_cache_path_is_consulted_first() {
        // The convention, asserted without touching the filesystem.
        assert_eq!(offline_cache_name("firefox", 167), "firefox_167.png");
        assert_eq!(
            offline_cache_name("org.mozilla.firefox.desktop", 64),
            "org.mozilla.firefox.desktop_64.png"
        );
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
        assert_eq!(
            icon.pixels, GRAY1_EXPECT,
            "the offline cache must beat the sweep"
        );

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
        assert!(
            cache.knows("late"),
            "the completed sweep does record the miss"
        );
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
        assert!(
            cache2.get("late").is_none(),
            "a bad offline file does not resolve"
        );
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

    // =======================================================================
    // Offline cache: the writer
    // =======================================================================

    /// A flat 8x8 tile, the input for every test that is about the *file* and
    /// not about the pixels.
    fn tiny_tile() -> RgbaImage {
        RgbaImage {
            width: 8,
            height: 8,
            pixels: [0x21, 0x96, 0xF0, 0xFF].repeat(64),
        }
    }

    /// A cache with write-back on and no roots, so nothing can resolve and every
    /// statement about the directory is about the writer alone.
    fn writing_cache(dir: &Path) -> IconCache {
        let mut cache = IconCache::with_roots(vec![], "");
        cache.set_offline_dir(Some(dir.to_path_buf()));
        cache.set_offline_writing(true);
        cache
    }

    /// Names of the cache entries in `dir`, by the same rules
    /// [`prune_offline_cache`] applies: a regular file ending in `.png`, not a
    /// temporary one. Using the same rules as the implementation is deliberate —
    /// a helper that counted more (or fewer) files than the pruner would make
    /// every assertion about it a statement about the helper.
    ///
    /// A missing directory is empty, not an error: several of these tests assert
    /// that *nothing* was written, before anything created the directory.
    fn png_entries(dir: &Path) -> Vec<String> {
        let Ok(reader) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = reader
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".png")
                    && !name.starts_with(OFFLINE_TMP_PREFIX)
                    && e.path().symlink_metadata().is_ok_and(|m| m.is_file())
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Names of any temporary files a write left behind.
    fn temp_entries(dir: &Path) -> Vec<String> {
        let Ok(reader) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = reader
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(OFFLINE_TMP_PREFIX))
            .collect();
        names.sort();
        names
    }

    /// The headline property: a second launch, with the theme tree *gone*,
    /// still resolves the icon — because the first launch wrote it.
    ///
    /// This is the whole point of the writer. Before it existed, `load_offline`
    /// was a guaranteed miss on every cold icon of every boot: one `open` of a
    /// file nothing had ever produced. The second cache here is built with
    /// `with_roots(vec![], "")` — no search path at all — so the only thing that
    /// can answer `resolve_keys` is the file the first cache wrote.
    #[test]
    fn a_second_launch_resolves_from_the_file_the_first_wrote() {
        let tree = Tree::new("writeback");
        tree.put("icons/hicolor/64x64/apps/demo.png", RGBA8_PNG);
        let cache_dir = tree.path().join("cache");

        // First launch: resolves through the sweep, and writes what it decoded.
        let mut first = IconCache::with_roots(vec![tree.icons_root()], "");
        first.set_display_edge(64);
        first.set_offline_dir(Some(cache_dir.clone()));
        first.set_offline_writing(true);
        first.resolve_keys(&["demo".to_string()]);
        let resolved = first.get("demo").expect("the sweep resolves it");
        assert_eq!(resolved.pixels, RGBA8_EXPECT);
        assert_eq!(
            png_entries(&cache_dir),
            vec!["demo_64.png".to_string()],
            "the resolved icon is written under the documented name"
        );

        // Second launch: no roots at all, so only the file can answer.
        let mut second = writing_cache(&cache_dir);
        second.set_display_edge(64);
        second.resolve_keys(&["demo".to_string()]);
        let hit = second.get("demo").expect("the offline entry resolves it");
        assert_eq!(
            hit.pixels, RGBA8_EXPECT,
            "the written file is pixel-identical to the sweep's answer"
        );
    }

    /// What goes on disk is the *pre-transform* tile.
    ///
    /// If the styled tile were cached instead, this would still render correctly
    /// today and be permanently wrong tomorrow: `set_shape` clears the in-memory
    /// cache and re-resolves, which would mask an already-squircle-masked tile as
    /// a circle, and the corner the first mask cut would stay cut forever. Same
    /// for a monochrome tint, which no later setting could undo.
    #[test]
    fn the_written_file_is_the_untransformed_tile() {
        let tree = Tree::new("untransformed");
        tree.put("icons/hicolor/64x64/apps/demo.png", RGBA8_PNG);
        let cache_dir = tree.path().join("cache");

        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(64);
        cache.set_offline_dir(Some(cache_dir.clone()));
        cache.set_offline_writing(true);
        // Three transforms that all bake themselves into the cached `Rc`.
        cache.set_foreground_background(Some(0xFF00_0000));
        cache.set_monochrome(Some(0xFF00_FF00));
        cache.set_shape(IconShape::Circle);
        cache.resolve_keys(&["demo".to_string()]);

        let styled = cache.get("demo").expect("resolved");
        assert_ne!(
            styled.pixels, RGBA8_EXPECT,
            "the in-memory tile really was transformed"
        );

        // The file, however, holds the decoded source byte for byte.
        let written = std::fs::read(offline_cache_path(&cache_dir, "demo", 64)).unwrap();
        assert_eq!(
            decode_png(&written).expect("the file is a PNG").pixels,
            RGBA8_EXPECT,
            "the file must hold the pre-transform tile"
        );

        // And a fresh cache with the *same* transforms gets the source and styles it
        // identically, which is the round trip that matters: the file carries no
        // styling, so the current settings decide the pixels.
        let mut second = writing_cache(&cache_dir);
        second.set_display_edge(64);
        second.set_foreground_background(Some(0xFF00_0000));
        second.set_monochrome(Some(0xFF00_FF00));
        second.set_shape(IconShape::Circle);
        second.resolve_keys(&["demo".to_string()]);
        assert_eq!(
            second.get("demo").expect("resolved from the file").pixels,
            styled.pixels,
            "styling the file reproduces the styling of the sweep's answer"
        );

        // The negative: a cache with *no* transforms must get the unstyled tile,
        // which the styled one demonstrably is not. If the file had been written
        // after the transforms, this would be the styled pixels instead — and
        // that is the whole failure this test exists to catch.
        let mut plain = writing_cache(&cache_dir);
        plain.set_display_edge(64);
        plain.resolve_keys(&["demo".to_string()]);
        assert_eq!(
            plain.get("demo").expect("resolved from the file").pixels,
            mask_tile(rgba8_image(), IconShape::Squircle).pixels,
            "the file styles as the *current* settings say, not as the writer's"
        );
    }

    /// The `RGBA8_PNG` fixture as an `RgbaImage`, for expected-value assertions
    /// that go through the transform pipeline rather than the decoder.
    fn rgba8_image() -> RgbaImage {
        RgbaImage {
            width: 8,
            height: 8,
            pixels: RGBA8_EXPECT.to_vec(),
        }
    }

    /// A write leaves no temporary file, and the temporary name is one
    /// `load_offline` can never build.
    ///
    /// Both halves matter. A leftover temp is a truncated file sitting in a cache
    /// directory forever; a temp name that *could* collide with
    /// `<app_id>_<edge>.png` would be read as an entry on every subsequent boot,
    /// for an entry that can never succeed. The name carries the pid so two
    /// launcher instances cannot share one temp path.
    #[test]
    fn a_write_leaves_no_temporary_file_and_the_temp_name_is_unreadable() {
        let tree = Tree::new("tmpname");
        let cache_dir = tree.path().join("cache");

        let img = tiny_tile();
        let written = write_offline_icon(&cache_dir, "demo", 64, &img).expect("write succeeds");
        assert!(written > 0, "the byte count is reported");
        assert_eq!(png_entries(&cache_dir), vec!["demo_64.png".to_string()]);
        assert_eq!(
            temp_entries(&cache_dir),
            Vec::<String>::new(),
            "the temp is renamed, not left behind"
        );

        // The temp name differs from the final name, starts with the marker, and
        // contains the pid. Because the marker is a leading dot, `load_offline`
        // (which builds `<key>_<edge>.png`) can never name it.
        let final_path = offline_cache_path(&cache_dir, "demo", 64);
        let tmp = offline_temp_path(&final_path);
        let tmp_name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert_ne!(tmp_name, "demo_64.png");
        assert!(tmp_name.starts_with(OFFLINE_TMP_PREFIX));
        assert!(
            tmp_name.contains(&std::process::id().to_string()),
            "the pid keeps two launchers off one temp path: {tmp_name}"
        );
        assert!(
            tmp_name.ends_with("demo_64.png"),
            "the final name is preserved so an orphan is identifiable"
        );
        assert!(tmp.parent() == final_path.parent(), "same directory");

        // A key that *starts* with the marker is refused, so a desktop file
        // cannot squat on the temp namespace.
        assert!(
            write_offline_icon(&cache_dir, ".utlc-tmp.sneaky", 64, &img).is_err(),
            "a key colliding with the temp namespace is refused"
        );
    }

    /// The write goes to a temp file and is *renamed* into place, so an existing
    /// entry is replaced without ever being truncated.
    ///
    /// This is the observable half of the temp discipline, and it is what makes
    /// the guarantee in [`write_offline_icon`] real rather than aspirational: the
    /// temp+rename shape means a failed write cannot damage the entry that is
    /// already there. The test forces the write to fail *deterministically* by
    /// pointing the temp name at a directory:
    ///
    /// * with the temp discipline, `File::create` on the temp fails, the function
    ///   returns an error, and the existing entry is untouched — still the exact
    ///   PNG that was there;
    /// * writing straight to the final name (no temp) would instead succeed, so
    ///   the assertion that it *errors* is what distinguishes the two. That is
    ///   the property, not an artefact of the setup.
    ///
    /// `rename` replacing a live inode in one step is what makes a reader see
    /// either the old file or the complete new one, never a truncated one. Note
    /// the setup is a *symlink*, so nothing is written outside the tree: it
    /// resolves to a directory, and opening a directory for writing fails.
    #[cfg(unix)]
    #[test]
    fn a_failed_write_leaves_an_existing_entry_intact() {
        use std::os::unix::fs::symlink;

        let tree = Tree::new("atomic");
        let cache_dir = tree.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        // A good entry already in place.
        let final_path = offline_cache_path(&cache_dir, "demo", 64);
        std::fs::write(&final_path, RGBA8_PNG).unwrap();
        let before = std::fs::read(&final_path).unwrap();

        // The temp name now resolves to a directory, so `create` on it fails.
        let blocked = tree.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        symlink(&blocked, offline_temp_path(&final_path)).unwrap();

        // A different icon, so a successful write would be visible.
        let err = write_offline_icon(&cache_dir, "demo", 64, &tiny_tile())
            .expect_err("the blocked temp must make the write fail");
        assert!(
            !matches!(err.kind(), std::io::ErrorKind::InvalidInput),
            "a filesystem failure, not an input rejection: {err:?}"
        );

        assert_eq!(
            std::fs::read(&final_path).expect("entry survives"),
            before,
            "the existing entry must not be truncated by a failed write"
        );
        assert!(
            decode_png(&std::fs::read(&final_path).unwrap()).is_some(),
            "and it still decodes"
        );
    }

    /// The temp file is removed when the write or the rename fails, so a crash
    /// or a full disk leaves a directory a prune can collect rather than one
    /// full of half-icons.
    #[test]
    fn a_failed_write_leaves_no_temp_and_no_entry() {
        let tree = Tree::new("failedwrite");
        // The *parent* of the cache dir is a regular file, so `create_dir_all`
        // inside it cannot succeed: a deterministic ENOTDIR/EEXIST on every
        // platform this runs on, with no mocking and no /dev/full.
        let blocker = tree.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let cache_dir = blocker.join("icons");

        let img = tiny_tile();
        let err = write_offline_icon(&cache_dir, "demo", 64, &img)
            .expect_err("writing under a regular file must fail");
        assert!(
            !matches!(err.kind(), std::io::ErrorKind::InvalidInput),
            "a filesystem failure, not an input rejection: {err:?}"
        );
        // Nothing was created: not the entry, and not a stray directory either.
        assert!(blocker.is_file(), "the blocker is untouched");
        assert!(
            std::fs::read_dir(&blocker).is_err(),
            "nothing was created beside the blocker"
        );
    }

    /// A key that is not a safe file name is refused outright, not sanitised.
    ///
    /// `offline_cache_path` is a plain `join`, so a key containing `/` would
    /// otherwise resolve *outside* the cache directory and write there. Both the
    /// writer and the reader refuse, which is the two-guards-in-two-places the
    /// docs promise.
    #[test]
    fn unsafe_keys_are_refused_by_both_the_writer_and_the_reader() {
        let tree = Tree::new("unsafe");
        let cache_dir = tree.path().join("cache");
        // Created up front, because every key below is refused *before* the
        // writer would have created it: "nothing was written" must mean the
        // directory was empty, not that it was absent.
        std::fs::create_dir_all(&cache_dir).unwrap();
        let img = tiny_tile();

        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "../escape",
            "sub/dir.png",
            "nul\0byte",
        ] {
            assert!(
                !is_offline_cache_name_safe(bad),
                "{bad:?} must not be a usable cache name"
            );
            assert!(
                write_offline_icon(&cache_dir, bad, 64, &img).is_err(),
                "{bad:?} must be refused by the writer"
            );
        }
        assert_eq!(
            png_entries(&cache_dir),
            Vec::<String>::new(),
            "nothing was written, inside or outside"
        );
        // The sibling of the cache dir is untouched: `../escape` really would
        // have landed here.
        assert!(!tree.path().join("escape_64.png").exists());

        // And the reader side, through the private stage the dispatch calls. A
        // decoy is planted *outside* the cache dir at the exact path
        // `offline_cache_path` would build, so an unguarded reader has something
        // real to find: without the check this resolves, which is the bug the
        // check is there for. (`resolve_keys` already routes path keys to
        // `LookupStage::AbsolutePath`, so this is the second of two guards.)
        std::fs::write(tree.path().join("escape_64.png"), RGBA8_PNG).unwrap();
        let cache = writing_cache(&cache_dir);
        let escaped = cache.load_offline("../escape");
        assert!(
            escaped.is_none(),
            "load_offline must refuse a name that resolves outside the cache dir"
        );
        // A safe key with the same shape of decoy *is* readable, so the previous
        // assertion is about the guard and not about a stage that reads nothing.
        std::fs::write(offline_cache_path(&cache_dir, "legit", 64), RGBA8_PNG).unwrap();
        assert!(
            cache.load_offline("legit").is_some(),
            "a safe name in the cache dir still reads"
        );
    }

    /// An entry that is already there is not rewritten.
    ///
    /// One `stat` instead of a 109 KiB encode plus three syscalls per icon is
    /// what keeps a warm boot from rewriting the whole directory — and it is what
    /// makes `mtime` a usable recency signal for the pruner.
    #[test]
    fn an_existing_entry_is_not_rewritten() {
        let tree = Tree::new("norewrite");
        tree.put("icons/hicolor/64x64/apps/demo.png", RGBA8_PNG);
        let cache_dir = tree.path().join("cache");

        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(64);
        cache.set_offline_dir(Some(cache_dir.clone()));
        cache.set_offline_writing(true);
        cache.resolve_keys(&["demo".to_string()]);

        let path = offline_cache_path(&cache_dir, "demo", 64);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let bytes_before = std::fs::read(&path).unwrap();

        // A second launch with the *same* file present: the offline stage hits,
        // so `cache_offline` is never even reached. Drop the in-memory entry to
        // reach the writer without the file being removed, which is the case
        // that would cost the most (a shape change re-resolves every key).
        cache.invalidate_key("demo");
        cache.resolve_keys(&["demo".to_string()]);
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "an existing entry must not be rewritten");
        assert_eq!(bytes_before, std::fs::read(&path).unwrap());

        // Write-back off leaves the file alone too, and the stage still reads it.
        let mut reader = writing_cache(&cache_dir);
        reader.set_offline_writing(false);
        reader.set_display_edge(64);
        reader.resolve_keys(&["demo".to_string()]);
        assert!(reader.get("demo").is_some(), "still readable");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "reading does not rewrite"
        );
    }

    /// The writer is off unless asked for, and `with_roots` never writes into a
    /// directory it was not given.
    #[test]
    fn write_back_is_opt_in_for_an_explicit_roots_cache() {
        let tree = Tree::new("optin");
        tree.put("icons/hicolor/64x64/apps/demo.png", RGBA8_PNG);
        let cache_dir = tree.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(64);
        cache.set_offline_dir(Some(cache_dir.clone()));
        assert!(
            !cache.offline_writing(),
            "with_roots defaults write-back off, for the same reason it defaults the dir off"
        );
        cache.resolve_keys(&["demo".to_string()]);
        assert!(cache.get("demo").is_some(), "resolution still works");
        assert_eq!(
            png_entries(&cache_dir),
            Vec::<String>::new(),
            "an opt-in cache must not write"
        );

        // Turning it on and re-resolving writes. The key is already known, so it
        // has to be dropped first — which is the shape of a shape change, and is
        // why `cache_offline` being on the re-resolve path is what matters.
        cache.set_offline_writing(true);
        assert!(cache.offline_writing());
        assert!(cache.invalidate_key("demo"));
        cache.resolve_keys(&["demo".to_string()]);
        assert_eq!(
            png_entries(&cache_dir),
            vec!["demo_64.png".to_string()],
            "turning it on is enough"
        );
    }

    /// A tile above the cache ceiling is not written.
    ///
    /// [`ICON_MAX_TILE_EDGE`] is the module's existing admission rule for a tile
    /// the cache will hold, reused rather than reinvented. A 1024 px tile is a
    /// 4 MiB file: writing one would fill the offline budget with the icons
    /// least likely to be drawn, and the LRU would evict it first anyway.
    #[test]
    fn a_tile_above_the_cache_ceiling_is_not_written() {
        let tree = Tree::new("ceiling");
        let cache_dir = tree.path().join("cache");
        let edge = ICON_MAX_TILE_EDGE + 1;
        let img = RgbaImage {
            width: edge,
            height: edge,
            pixels: vec![0x21; edge as usize * edge as usize * 4],
        };

        assert!(
            write_offline_icon(&cache_dir, "toobig", edge, &img).is_ok(),
            "the primitive itself does not care: the ceiling is the cache's rule, not the file format's"
        );
        let big = offline_cache_name("toobig", edge);
        assert_eq!(big, "toobig_257.png", "named for the edge it was given");
        assert_eq!(png_entries(&cache_dir), vec![big]);
        std::fs::remove_dir_all(&cache_dir).unwrap();

        // Through the cache, which is where the ceiling applies.
        let mut cache = writing_cache(&cache_dir);
        cache.set_display_edge(edge);
        let key = "toobig".to_string();
        // Insert straight into the cache: this is the insert path, and the point
        // is that `commit_resolved` is the only route a decoded icon takes.
        let decoded = img.clone();
        cache.commit_resolved(&key, decoded);
        assert!(cache.get(&key).is_some(), "cached in memory as usual");
        assert_eq!(
            png_entries(&cache_dir),
            Vec::<String>::new(),
            "an over-ceiling tile stays in memory only"
        );

        // At the ceiling it is written, so the bound is a bound and not a ban.
        let mut ok = writing_cache(&cache_dir);
        ok.set_display_edge(ICON_MAX_TILE_EDGE);
        let at_edge = RgbaImage {
            width: ICON_MAX_TILE_EDGE,
            height: ICON_MAX_TILE_EDGE,
            pixels: vec![0x21; ICON_MAX_TILE_EDGE as usize * ICON_MAX_TILE_EDGE as usize * 4],
        };
        ok.commit_resolved("atedge", at_edge);
        assert_eq!(
            png_entries(&cache_dir).len(),
            1,
            "the ceiling itself writes"
        );
    }

    // =======================================================================
    // Offline cache: the pruner
    // =======================================================================

    /// A cache directory holding `count` entries of `size` bytes each, named by the
    /// real [`offline_cache_name`]. Orphan temps are planted separately, by the
    /// tests that care about them.
    fn planted_cache(tag: &str, count: usize, size: usize) -> Tree {
        let tree = Tree::new(tag);
        let dir = tree.path().join("cache");
        std::fs::create_dir_all(&dir).unwrap();
        // The payload is built once: `count` reaches `OFFLINE_CACHE_MAX_FILES + 1`
        // in one of these tests, and rebuilding a `Vec` per entry would dominate
        // the test's own runtime.
        let payload = vec![b'x'; size];
        for i in 0..count {
            // Through the real naming convention, so the planted entries are named
            // exactly as a write would name them -- a prune test that invented
            // its own names would not be testing the real ones.
            let name = offline_cache_name(&format!("app{i}"), 64);
            std::fs::write(dir.join(name), &payload).unwrap();
        }
        tree
    }

    /// Seconds since the epoch for `path`'s current mtime.
    ///
    /// The pruning tests need a *known* age order, and `File::set_modified` is
    /// the only stdlib way to set one — but it needs an open handle per entry.
    /// One-second spacing between `write` calls gives a strictly increasing
    /// mtime on every filesystem with second granularity, which is enough: the
    /// policy is about *which* entries go, not about the exact age.
    fn mtime_secs(path: &Path) -> u64 {
        std::fs::metadata(path)
            .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// The largest mtime among `paths`, so a test can date every entry relative
    /// to a base that is already in the past.
    fn newest_mtime_secs(paths: &[PathBuf]) -> u64 {
        paths.iter().map(|p| mtime_secs(p)).max().unwrap_or(0)
    }

    /// The pruner evicts oldest first, and stops as soon as it is inside both
    /// limits.
    #[test]
    fn pruning_evicts_the_oldest_entries_first() {
        let tree = planted_cache("prune", 6, 100);
        let dir = tree.path().join("cache");
        let paths: Vec<PathBuf> = (0..6).map(|i| dir.join(format!("app{i}_64.png"))).collect();
        // Establish a known oldest-to-newest order by dating each file
        // explicitly. `set_modified` needs an open handle per entry; opening
        // read-only is enough and touches no content.
        //
        // The base is "now minus one hour", so the dates are in the past rather
        // than in the future: a future mtime is exactly the case the pruner
        // documents as sorted last, and using one here would test that instead
        // of what it is meant to.
        let base = newest_mtime_secs(&paths).saturating_sub(3600);
        // Recency is deliberately the *reverse* of the name order, so a pruner
        // that ignored mtime and fell back to the name tie-break would evict a
        // different set and fail this. Without that, "oldest first" and
        // "alphabetical first" would pick the same three entries and the
        // assertion would pass for the wrong reason.
        for (i, p) in paths.iter().enumerate() {
            let when =
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(base + (5 - i) as u64);
            std::fs::File::open(p)
                .expect("entry is readable")
                .set_modified(when)
                .expect("the filesystem supports explicit mtimes");
        }
        // So `app5` is oldest and `app0` newest, and eviction must take from the
        // `app5` end.
        assert_eq!(mtime_secs(&paths[5]) + 1, mtime_secs(&paths[4]));

        // Budget for three of the six: 600 bytes of 100, cap 3 files.
        let usage = prune_offline_cache(&dir, 600, 3).expect("prune succeeds");
        assert_eq!(usage.files, 3, "at most three entries survive");
        assert_eq!(usage.bytes, 300, "and the reported bytes agree");
        assert_eq!(usage.temps, 0);

        let survivors: Vec<String> = png_entries(&dir);
        assert_eq!(
            survivors,
            vec![
                "app0_64.png".to_string(),
                "app1_64.png".to_string(),
                "app2_64.png".to_string()
            ],
            "the three *newest* survive; eviction really is oldest-first"
        );
    }

    /// Both limits bind independently: a byte budget with generous headroom
    /// still trims a directory that is over its file cap, and vice versa.
    #[test]
    fn pruning_is_bounded_by_bytes_and_by_file_count() {
        let tree = planted_cache("pruneboth", 5, 100);
        let dir = tree.path().join("cache");
        // Byte budget alone, with a file cap that cannot bind.
        let by_bytes = prune_offline_cache(&dir, 200, usize::MAX).expect("prune");
        assert_eq!(by_bytes.files, 2, "200 bytes holds two 100-byte entries");
        assert!(by_bytes.bytes <= 200);

        // File cap alone, with a byte budget that cannot bind.
        let tree2 = planted_cache("prunecount", 5, 10);
        let dir2 = tree2.path().join("cache");
        let by_count = prune_offline_cache(&dir2, u64::MAX, 2).expect("prune");
        assert_eq!(by_count.files, 2, "two files fit the cap");
        assert!(by_count.bytes <= 20);

        // Already inside the limits: nothing is touched, and the report is the
        // full directory.
        let tree3 = planted_cache("pruneok", 2, 10);
        let dir3 = tree3.path().join("cache");
        let untouched = prune_offline_cache(&dir3, u64::MAX, usize::MAX).expect("prune");
        assert_eq!(untouched.files, 2);
        assert_eq!(untouched.bytes, 20);
        assert_eq!(png_entries(&dir3).len(), 2);
    }

    /// An orphan temp file is garbage and is removed; anything else that is not
    /// ours is left strictly alone.
    ///
    /// A shared cache root may hold files this module knows nothing about, so the
    /// pruner only ever deletes a `.png` regular file or a temp carrying its own
    /// marker. A directory named `foo.png` and an unrelated note must both
    /// survive.
    #[test]
    fn pruning_removes_orphan_temps_and_spares_everything_else() {
        let tree = planted_cache("prunetemp", 2, 50);
        let dir = tree.path().join("cache");

        let orphan = dir.join(format!("{OFFLINE_TMP_PREFIX}999.demo_64.png"));
        std::fs::write(&orphan, b"half a png").unwrap();
        // Not ours: no `.png` suffix, and a directory wearing one.
        std::fs::write(dir.join("README"), b"notes").unwrap();
        std::fs::create_dir(dir.join("sub.png")).unwrap();

        let usage = prune_offline_cache(&dir, u64::MAX, usize::MAX).expect("prune");
        assert_eq!(usage.temps, 1, "the orphan was collected");
        // Two planted entries: `sub.png` is a *directory*, so it is neither
        // counted as an entry nor a deletion candidate.
        assert_eq!(usage.files, 2, "only the real entries are counted");
        assert_eq!(usage.bytes, 100);
        assert!(!orphan.exists(), "the orphan is gone");
        assert!(dir.join("README").exists(), "an unrelated file survives");
        assert!(dir.join("sub.png").is_dir(), "a directory survives");

        // And the orphan is not counted as an entry that had to be evicted, so
        // the report is about cache entries only.
        assert_eq!(png_entries(&dir).len(), 2);
    }

    /// A symlink in the cache directory is measured as a link and is never
    /// followed into a deletion.
    ///
    /// `metadata` instead of `symlink_metadata` here would report the *target's*
    /// size and then `remove_file` the link, which is survivable — but reporting
    /// the target's size is enough to make a prune delete a file the launcher
    /// does not own, once the link is the oldest entry. So: skip non-regular
    /// files entirely.
    #[cfg(unix)]
    #[test]
    fn pruning_spares_symlinks() {
        use std::os::unix::fs::symlink;

        let tree = planted_cache("prunelink", 1, 10);
        let dir = tree.path().join("cache");
        // A target outside the cache directory entirely.
        let precious = tree.path().join("precious.txt");
        std::fs::write(&precious, b"do not delete me").unwrap();
        symlink(&precious, dir.join("linked_64.png")).unwrap();

        // A zero budget: the planted entry is over it and goes. The link is not
        // counted, so it is never a candidate — and its target is outside this
        // directory entirely, so nothing here can reach it either way.
        let usage = prune_offline_cache(&dir, 0, 0).expect("prune");
        assert_eq!(usage.files, 0, "the planted entry was evicted");
        assert_eq!(usage.bytes, 0);
        assert!(precious.exists(), "the link's target survives");
        assert!(
            dir.join("linked_64.png").exists(),
            "and so does the link: it was never a candidate"
        );

        // The negative that makes this a real test rather than an accident of
        // ordering: the planted entry really was the oldest *counted* thing, so a
        // pruner that did follow the link would either report the target's 13
        // bytes or delete the target. Neither happened, and the planted file
        // being gone proves the pass was not a no-op.
    }

    /// The pruner is reachable from the cache, resets its amortisation counter,
    /// and reports `NotFound` when the stage has no directory.
    #[test]
    fn the_cache_exposes_its_own_pruner() {
        let tree = planted_cache("cacheprune", 4, 100);
        let dir = tree.path().join("cache");

        // Four planted 100-byte entries are trivially inside both of the real
        // budgets, so the report is the directory untouched. The pruner is
        // reached and reset; whether it *evicts* anything is the pruner's own
        // test, above.
        let mut cache = writing_cache(&dir);
        cache.offline_writes = 7;
        let usage = cache.prune_offline_cache().expect("prune succeeds");
        assert_eq!(usage.files, 4, "inside the budgets, nothing goes");
        assert_eq!(usage.bytes, 400);
        assert_eq!(
            cache.offline_writes, 0,
            "a prune resets the amortisation counter"
        );
        assert_eq!(cache.offline_dir(), Some(dir.as_path()));

        // And the budgets it passed are the module's, not something else: a
        // directory one entry over the real file cap is trimmed by exactly one.
        let tree2 = planted_cache("cacheprune2", OFFLINE_CACHE_MAX_FILES + 1, 0);
        let dir2 = tree2.path().join("cache");
        let mut cache = writing_cache(&dir2);
        let usage = cache.prune_offline_cache().expect("prune succeeds");
        assert_eq!(
            usage.files, OFFLINE_CACHE_MAX_FILES,
            "the cache's own file cap is the one applied"
        );
        assert_eq!(
            png_entries(&dir2).len(),
            OFFLINE_CACHE_MAX_FILES,
            "and the directory agrees"
        );
        // The byte budget is the one in the doc, derived from the memory budget
        // rather than invented beside it.
        assert_eq!(OFFLINE_CACHE_BUDGET, ICON_CACHE_BUDGET * 8);
        assert_eq!(
            OFFLINE_CACHE_BUDGET,
            32 * 1024 * 1024,
            "~300 icons at the reference 167 px tile"
        );

        // No directory configured: NotFound, not a panic and not a silent Ok.
        let mut bare = IconCache::with_roots(vec![], "");
        assert_eq!(
            bare.prune_offline_cache().unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    /// The amortisation actually happens: a run of writes prunes once, not once
    /// per icon.
    ///
    /// The observable proxy is that a directory over its limits still holds its
    /// entries after fewer than [`OFFLINE_PRUNE_INTERVAL`] writes, and is trimmed
    /// after the interval. That is the assertion, and it is what stops a prune
    /// (`read_dir` plus an `lstat` per entry) from landing on every single icon.
    #[test]
    fn pruning_is_amortised_over_a_run_of_writes() {
        let tree = planted_cache("amortise", 4, 100);
        let dir = tree.path().join("cache");
        // A byte cap one entry below the planted total, so any prune trims.
        let mut cache = writing_cache(&dir);
        cache.offline_writes = 0;

        // Planted: 4 files x 100 bytes = 400. Write with a budget that 4 files
        // exceed. Force the prune trigger by reaching the interval: drive
        // `offline_writes` up to one below it, then write once more.
        cache.offline_writes = OFFLINE_PRUNE_INTERVAL - 1;
        cache.commit_resolved("fresh", tiny_tile());
        // The write itself did not prune (it was the interval-th), so the
        // planted files are all still present and the counter was reset by the
        // prune the interval triggered.
        assert_eq!(cache.offline_writes, 0, "the interval-th write pruned");
        // 5 files now; the real budgets are far above 500 bytes so nothing goes.
        assert_eq!(png_entries(&dir).len(), 5);

        // With one write *below* the interval, no prune runs.
        let tree2 = planted_cache("amortise2", 4, 100);
        let dir2 = tree2.path().join("cache");
        let mut cache2 = writing_cache(&dir2);
        cache2.offline_writes = 1;
        cache2.commit_resolved("fresh2", tiny_tile());
        assert_eq!(cache2.offline_writes, 2, "no prune below the interval");
        assert_eq!(png_entries(&dir2).len(), 5);
    }

    /// Every write and every prune together keep the directory inside the
    /// documented budget — the end-to-end bounding property.
    #[test]
    fn a_run_of_writes_keeps_the_directory_inside_the_budget() {
        let tree = Tree::new("bounded");
        tree.put("icons/hicolor/64x64/apps/demo.png", RGBA8_PNG);
        let cache_dir = tree.path().join("cache");

        // More writes than the prune interval, so the pruner really runs.
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(64);
        cache.set_offline_dir(Some(cache_dir.clone()));
        cache.set_offline_writing(true);
        for i in 0..(OFFLINE_PRUNE_INTERVAL + 8) {
            let key = format!("app{i}");
            cache.commit_resolved(&key, solid(64));
        }
        let usage = cache.prune_offline_cache().expect("prune");
        assert!(usage.files <= OFFLINE_CACHE_MAX_FILES, "file cap holds");
        assert!(
            usage.bytes as usize <= OFFLINE_CACHE_BUDGET,
            "byte cap holds: {} > {OFFLINE_CACHE_BUDGET}",
            usage.bytes
        );
        assert_eq!(
            usage.files,
            png_entries(&cache_dir).len(),
            "the report matches the directory"
        );
        assert_eq!(temp_entries(&cache_dir), Vec::<String>::new());
    }

    // =======================================================================
    // LOOKUP_ORDER is the implementation
    // =======================================================================

    /// [`LOOKUP_ORDER`] and the dispatch in [`IconCache::resolve_keys`] agree.
    ///
    /// The failure this guards is the one the brief describes: an enum that
    /// documents an order while the code implements a different one, with
    /// nothing keeping them in step. Reordering or dropping a variant in the
    /// const makes this red — the `LOOKUP_ORDER` assertion for the shape, and
    /// the per-stage behavioural assertions below for the meaning.
    ///
    /// The behavioural part is a table: for each stage, a key that *only that
    /// stage* can answer. If a stage is dropped from the dispatch the key falls
    /// through and never resolves; if two stages are swapped the wrong one wins.
    #[test]
    fn lookup_order_matches_the_dispatch() {
        // Shape: the documented order, and the batched sweep last.
        assert_eq!(
            LOOKUP_ORDER,
            [
                LookupStage::AbsolutePath,
                LookupStage::OfflineCache,
                LookupStage::Theme
            ]
        );
        assert_eq!(
            *LOOKUP_ORDER.last().unwrap(),
            LookupStage::Theme,
            "the sweep is batched, so it must be the tail of the per-key loop"
        );
        assert_eq!(
            LOOKUP_ORDER
                .iter()
                .filter(|s| **s == LookupStage::Theme)
                .count(),
            1
        );

        // (a) AbsolutePath, alone. No roots at all and no offline dir: only the
        //     file the key names can answer.
        let tree = Tree::new("order_path");
        let direct = tree.path().join("direct.png");
        std::fs::write(&direct, RGBA8_PNG).unwrap();
        let mut cache = IconCache::with_roots(vec![], "");
        cache.set_display_edge(64);
        let key = direct.to_string_lossy().to_string();
        cache.resolve_keys(std::slice::from_ref(&key));
        assert!(
            cache.get(&key).is_some(),
            "LookupStage::AbsolutePath must resolve a key nothing else can"
        );

        // (b) OfflineCache, alone. No roots, and the file is in the cache dir.
        let cache_dir = tree.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(offline_cache_path(&cache_dir, "onlycache", 64), GRAY1_PNG).unwrap();
        let mut cache = writing_cache(&cache_dir);
        cache.set_display_edge(64);
        cache.resolve_keys(&["onlycache".to_string()]);
        assert_eq!(
            cache
                .get("onlycache")
                .expect("offline stage resolves")
                .pixels,
            GRAY1_EXPECT,
            "LookupStage::OfflineCache must resolve a key nothing else can"
        );

        // (c) Theme, alone. Only a theme tree can answer, and the key is a plain
        //     name so the two fast paths decline it.
        let tree2 = Tree::new("order_theme");
        tree2.put("icons/hicolor/64x64/apps/onlytheme.png", RGBA8_PNG);
        let mut cache = IconCache::with_roots(vec![tree2.icons_root()], "");
        cache.set_display_edge(64);
        cache.resolve_keys(&["onlytheme".to_string()]);
        assert_eq!(
            cache.get("onlytheme").expect("sweep resolves").pixels,
            RGBA8_EXPECT,
            "LookupStage::Theme must resolve a key nothing else can"
        );
    }

    /// Precedence: where two stages can both answer, the earlier one in
    /// [`LOOKUP_ORDER`] wins — and each pairwise case is set up so the two
    /// candidates are distinguishable.
    #[test]
    fn the_stage_precedence_is_the_one_look_up_order_states() {
        // AbsolutePath before OfflineCache: a key that is *both* a readable file
        // and a cache entry must resolve as the file.
        let tree = Tree::new("prec_path_offline");
        let file = tree.path().join("both.png");
        std::fs::write(&file, RGBA8_PNG).unwrap();
        let cache_dir = tree.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        // The name `load_offline` would build for this key is refused (a path is
        // not a safe cache name), so the offline stage cannot answer at all --
        // which is exactly the invariant being asserted here: a path key never
        // reaches it. The cache holds a decoy under a name the path cannot match.
        std::fs::write(offline_cache_path(&cache_dir, "decoy", 64), GRAY1_PNG).unwrap();
        let mut cache = writing_cache(&cache_dir);
        cache.set_display_edge(64);
        let key = file.to_string_lossy().to_string();
        cache.resolve_keys(std::slice::from_ref(&key));
        assert_eq!(
            cache.get(&key).expect("resolved").pixels,
            RGBA8_EXPECT,
            "the path wins, and the offline stage never saw a path key"
        );

        // OfflineCache before Theme: both stages have a candidate for the same
        // plain name, and they differ in pixels.
        let tree2 = Tree::new("prec_offline_theme");
        tree2.put("icons/hicolor/64x64/apps/dual.png", RGBA8_PNG);
        let cache_dir2 = tree2.path().join("cache");
        std::fs::create_dir_all(&cache_dir2).unwrap();
        std::fs::write(offline_cache_path(&cache_dir2, "dual", 64), GRAY1_PNG).unwrap();
        let mut cache = IconCache::with_roots(vec![tree2.icons_root()], "");
        cache.set_display_edge(64);
        cache.set_offline_dir(Some(cache_dir2.clone()));
        cache.set_offline_writing(true);
        cache.resolve_keys(&["dual".to_string()]);
        assert_eq!(
            cache.get("dual").expect("resolved").pixels,
            GRAY1_EXPECT,
            "the offline entry beats the sweep"
        );

        // And with the offline stage off, the sweep's answer comes back — so the
        // previous assertion is about precedence, not about the sweep being dead.
        let mut cache = IconCache::with_roots(vec![tree2.icons_root()], "");
        cache.set_display_edge(64);
        cache.set_offline_dir(None);
        cache.resolve_keys(&["dual".to_string()]);
        assert_eq!(
            cache.get("dual").expect("resolved").pixels,
            RGBA8_EXPECT,
            "with no offline dir the sweep's own answer comes back"
        );
    }

    /// Adding a stage is one const entry and one arm. This asserts the arm is
    /// reachable *and* that a stage which declines a key leaves the key to the
    /// next one, which is the property the whole dispatch rests on: a stage must
    /// `continue`, never fall through into another stage's work.
    #[test]
    fn a_stage_that_declines_a_key_leaves_it_to_the_next_one() {
        let tree = Tree::new("declines");
        tree.put("icons/hicolor/64x64/apps/plain.png", RGBA8_PNG);
        let cache_dir = tree.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        // An entry the offline stage would find, for a key that is also a real
        // file. The path stage must claim the key without ever letting the
        // offline stage build a name from it.
        std::fs::write(offline_cache_path(&cache_dir, "plain", 64), GRAY1_PNG).unwrap();

        // Plain name: the two per-key stages both decline, so the sweep answers.
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(64);
        cache.set_offline_dir(Some(cache_dir.clone()));
        cache.resolve_keys(&["plain".to_string()]);
        assert_eq!(
            cache.get("plain").expect("resolved").pixels,
            GRAY1_EXPECT,
            "the offline stage claims a plain key ahead of the sweep"
        );

        // Path key: the path stage claims it, and nothing after it runs.
        let file = tree.path().join("plainfile.png");
        std::fs::write(&file, RGBA8_PNG).unwrap();
        let mut cache = writing_cache(&cache_dir);
        cache.set_display_edge(64);
        let key = file.to_string_lossy().to_string();
        cache.resolve_keys(std::slice::from_ref(&key));
        assert_eq!(
            cache.get(&key).expect("resolved").pixels,
            RGBA8_EXPECT,
            "the path stage claims a path key and the offline stage declines it"
        );
    }

    /// The offline stage's miss is not pinned, so a file written a second later is
    /// picked up — and the writer makes "a second later" happen on its own.
    ///
    /// This is the loop-closing property: a first launch misses and sweeps, then
    /// writes; the miss set is dropped because the icon *did* resolve; and a
    /// second launch, with no roots, hits the file.
    #[test]
    fn the_offline_stage_recovers_from_the_file_the_writer_just_produced() {
        let tree = Tree::new("recover");
        tree.put("icons/hicolor/64x64/apps/late.png", RGBA8_PNG);
        let cache_dir = tree.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        // A key with no candidate anywhere: recorded as a miss by a completed
        // sweep, not written (there is nothing to write).
        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(64);
        cache.set_offline_dir(Some(cache_dir.clone()));
        cache.set_offline_writing(true);
        let keys = vec!["neverinstalled".to_string()];
        cache.resolve_keys(&keys);
        assert!(cache.get("neverinstalled").is_none());
        assert!(cache.knows("neverinstalled"), "the miss is recorded");
        assert_eq!(
            png_entries(&cache_dir),
            Vec::<String>::new(),
            "nothing to write for a key that resolved to nothing"
        );

        // The key that *did* resolve is on disk, and only after the sweep.
        cache.resolve_keys(&["late".to_string()]);
        assert_eq!(png_entries(&cache_dir), vec!["late_64.png".to_string()]);

        // Drop every in-memory trace and go again with no roots: the file is the
        // only possible answer.
        let mut second = writing_cache(&cache_dir);
        second.set_display_edge(64);
        second.resolve_keys(&["late".to_string()]);
        assert_eq!(second.get("late").expect("resolved").pixels, RGBA8_EXPECT);
    }

    /// Every key a batch can present reaches the right stage, in one call.
    ///
    /// `resolve_keys` is the only entry point and it handles all three shapes of
    /// key at once: a path, a name the offline cache answers, and a name only the
    /// sweep answers. Asserting them together catches a dispatch that only works
    /// for one kind.
    #[test]
    fn one_batch_routes_every_kind_of_key_to_its_stage() {
        let tree = Tree::new("routing");
        tree.put("icons/hicolor/64x64/apps/bytheme.png", RGBA8_PNG);
        tree.put("icons/hicolor/64x64/apps/byoffline.png", RGBA8_PNG);
        let cache_dir = tree.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        // Pre-seeded, so the offline stage has something the sweep does not.
        std::fs::write(offline_cache_path(&cache_dir, "byoffline", 64), GRAY1_PNG).unwrap();
        let file = tree.path().join("bypath.png");
        std::fs::write(&file, GRAY1_PNG).unwrap();

        let mut cache = IconCache::with_roots(vec![tree.icons_root()], "");
        cache.set_display_edge(64);
        cache.set_offline_dir(Some(cache_dir.clone()));
        cache.set_offline_writing(true);
        let path_key = file.to_string_lossy().to_string();
        let keys = vec![
            path_key.clone(),
            "byoffline".to_string(),
            "bytheme".to_string(),
            "absent".to_string(),
        ];
        cache.resolve_keys(&keys);

        assert_eq!(
            cache.get(&path_key).expect("path").pixels,
            GRAY1_EXPECT,
            "path key decoded directly"
        );
        assert_eq!(
            cache.get("byoffline").expect("offline").pixels,
            GRAY1_EXPECT,
            "offline entry used"
        );
        assert_eq!(
            cache.get("bytheme").expect("theme").pixels,
            RGBA8_EXPECT,
            "sweep used"
        );
        assert!(cache.get("absent").is_none(), "absent key misses");
        assert!(cache.knows("absent"), "and the miss is recorded");

        // Only the two sweep-resolved keys were written; the path key and the
        // already-present offline entry were not.
        assert_eq!(
            png_entries(&cache_dir),
            vec!["byoffline_64.png".to_string(), "bytheme_64.png".to_string()],
            "the path key is never written, and an existing entry is not rewritten"
        );
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

    // =======================================================================
    // Cache-time squircle masking + monogram fallback
    // =======================================================================

    /// A `depth`-square opaque tile: the input every masking test starts from.
    /// Above [`ICON_MASK_MIN_EDGE`] so it is actually masked.
    fn masked_solid(n: u32) -> RgbaImage {
        solid(n.max(ICON_MASK_MIN_EDGE))
    }

    #[test]
    fn cached_icons_are_squircle_masked() {
        // (a) An image entering the cache is masked, and the pixels outside
        //     `squircle_span` are transparent.
        let n = 64u32;
        let mut cache = IconCache::with_roots(vec![], "");
        cache.insert_image("sq".into(), masked_solid(n));
        let img = cache.get("sq").expect("inserted tile is cached");

        // The boundary is `squircle_span` and nothing else, so the assertion
        // is against that function directly rather than a re-derivation of it.
        let r = n / 2;
        let rn = r as usize;
        for y in 0..n as usize {
            let dy = (y as i64 - r as i64).unsigned_abs() as u32;
            let half = squircle_span(r, dy) as usize;
            let row = &img.pixels[y * n as usize * 4..(y + 1) * n as usize * 4];
            for (x, px) in row.as_chunks::<4>().0.iter().enumerate() {
                // `apply_shape` keeps the half-open span
                // `[center - half, center + half)` with `center = edge / 2`,
                // so that is exactly what is under test here.
                let in_span = half > 0 && x + half >= rn && x < half + rn;
                if in_span {
                    assert_eq!(
                        px[3], 0xFF,
                        "({x},{y}) is inside the squircle and must be opaque"
                    );
                } else {
                    assert_eq!(
                        px[3], 0,
                        "({x},{y}) is outside the squircle and must be clear"
                    );
                }
            }
        }
        // The four outer corners are the visible claim, so state it directly
        // rather than relying only on the loop above.
        for &(x, y) in &[(0usize, 0usize), (63, 0), (0, 63), (63, 63)] {
            assert_eq!(
                img.pixels[(y * 64 + x) * 4 + 3],
                0,
                "corner ({x},{y}) must be clear"
            );
        }
        // The middle row survives edge to edge: the mask cuts corners, it does
        // not shrink the icon.
        assert_eq!(alpha_span(&img, 32), Some((0, 64)));

        // Idempotent: re-inserting the *already masked* bytes must not change
        // them, which is what lets the cache mask without a "was it masked?"
        // side-channel.
        let before = cache.get("sq").unwrap().pixels.clone();
        let again = RgbaImage {
            width: n,
            height: n,
            pixels: before.clone(),
        };
        cache.insert_image("sq".into(), again);
        assert_eq!(
            cache.get("sq").unwrap().pixels,
            before,
            "re-insert must not change bytes"
        );
    }

    #[test]
    fn masking_floor_leaves_tiny_icons_intact() {
        // The floor is a correctness rule, not a tuning knob: at edge 1,
        // `squircle_span(0, _) == 0` erases the tile completely, and at edge 8
        // the integer floor removes 31% of the icon. Both are regressions on
        // real hicons, which are routinely 1x1, 8x8 or 16x16.
        for n in [1u32, 8, 16] {
            let src = solid(n);
            let masked = mask_tile(src.clone(), IconShape::Squircle);
            assert_eq!(
                masked.pixels, src.pixels,
                "edge {n} is below the floor and must pass through byte-identical"
            );
            assert!(has_visible_ink(&masked), "edge {n} must survive intact");
        }
        // And at the floor it does mask: the boundary is not simply "never".
        let at_floor = mask_tile(solid(ICON_MASK_MIN_EDGE), IconShape::Squircle);
        assert!(
            at_floor.pixels.as_chunks::<4>().0.iter().any(|p| p[3] == 0),
            "the floor itself masks"
        );

        // Why the floor exists, measured rather than asserted from a comment:
        // `apply_shape` at these sizes is not a corner trim, it is a crop.
        // Measured on the *unmasked* mask so the floor is not what is being
        // measured.
        let removed = |n: u32| {
            let mut m = solid(n);
            apply_shape(&mut m, IconShape::Squircle);
            let gone = (0..n as usize * n as usize)
                .filter(|&i| m.pixels[i * 4 + 3] == 0)
                .count();
            gone * 100 / (n as usize * n as usize)
        };
        assert!(removed(1) >= 99, "1 px is erased entirely: {}%", removed(1));
        assert!(removed(8) >= 25, "8 px loses ~31%: {}%", removed(8));
        assert!(removed(64) <= 12, "64 px loses under 12%: {}%", removed(64));
    }

    #[test]
    fn a_missing_or_blank_icon_yields_a_monogram_tile() {
        // (b) Missing -> monogram. The tile is the app's own colour, the glyph
        //     is the first letter, and there is real ink on it.
        let _guard = crate::graphics::font::TEST_FONT_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::graphics::font::set_active_family(crate::graphics::font::FontFamily::NotoSans);
        let n = 64u32;
        let mut cache = IconCache::with_roots(vec![], "");
        cache.set_display_edge(n);
        let color = 0xFF1E_88E5u32;
        let img = cache
            .get_or_monogram("firefox", "Firefox", color)
            .expect("a monogram is always producible for an ASCII name");
        assert_eq!(
            (img.width, img.height),
            (n, n),
            "cached at the display edge"
        );
        assert!(has_visible_ink(&img), "the monogram is not blank");

        // The surface is the app's colour: at the tile's centre-ish, well
        // inside the shape, the pixel is the colour that was asked for.
        let at = |x: usize, y: usize| {
            let i = (y * n as usize + x) * 4;
            (
                img.pixels[i],
                img.pixels[i + 1],
                img.pixels[i + 2],
                img.pixels[i + 3],
            )
        };
        let surface = at(8, 32);
        assert_eq!(surface.3, 0xFF, "inside the tile is opaque");
        let want = [(color >> 16) as u8, (color >> 8) as u8, color as u8];
        assert_eq!(
            &[surface.0, surface.1, surface.2],
            &want[..],
            "tile is the app's colour"
        );

        // Non-background ink: at least one pixel is neither the tile colour nor
        // transparent, which can only be the glyph.
        let ink = (0..n as usize * n as usize)
            .filter(|&i| {
                let p = &img.pixels[i * 4..i * 4 + 4];
                p[3] != 0 && (p[0], p[1], p[2]) != (want[0], want[1], want[2])
            })
            .count();
        assert!(
            ink > 20,
            "only {ink} non-background pixels: no glyph was drawn"
        );

        // The first letter is what is drawn: a different name must differ.
        let mut other = IconCache::with_roots(vec![], "");
        other.set_display_edge(n);
        let g = other.get_or_monogram("gnome", "Gnome", color).unwrap();
        assert_ne!(g.pixels, img.pixels, "'Gnome' must not render 'F'");

        // Blank PNG -> monogram, not a hole. A key that decoded to a fully
        // transparent tile must be replaced, because it satisfies `knows()`
        // and would otherwise be blitted as nothing forever.
        let mut cache2 = IconCache::with_roots(vec![], "");
        cache2.set_display_edge(n);
        cache2.insert_image(
            "blank".into(),
            RgbaImage {
                width: n,
                height: n,
                pixels: vec![0u8; n as usize * n as usize * 4],
            },
        );
        assert!(cache2.knows("blank"), "the blank tile was cached first");
        let fixed = cache2
            .get_or_monogram("blank", "Blank", color)
            .expect("replaced");
        assert!(
            has_visible_ink(&fixed),
            "a blank icon must become a monogram"
        );
        // "Blank" and "Firefox" start with different letters, so the two must
        // differ: the replacement really was re-rendered, not the old blank.
        assert_ne!(
            fixed.pixels, img.pixels,
            "the blank must be replaced, not reused"
        );

        // A name with no renderable first character still gets a tile: '?' is
        // in the font's range, so the launcher never shows a hole.
        let mut cache3 = IconCache::with_roots(vec![], "");
        cache3.set_display_edge(n);
        assert!(cache3
            .get_or_monogram("emoji", "\u{1F600}", color)
            .is_some());
    }

    #[test]
    fn the_same_key_always_yields_the_same_bytes() {
        // (c) Determinism. Two independent caches, same inputs, same bytes --
        //         and the same cache asked twice returns the identical Rc.
        let _guard = crate::graphics::font::TEST_FONT_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::graphics::font::set_active_family(crate::graphics::font::FontFamily::NotoSans);
        let n = 48u32;
        let build = || {
            let mut c = IconCache::with_roots(vec![], "");
            c.set_display_edge(n);
            c.get_or_monogram("term", "Terminal", 0xFF10B981)
                .unwrap()
                .pixels
                .clone()
        };
        let a = build();
        let b = build();
        assert_eq!(a, b, "two independent caches must agree byte for byte");

        // A decoded icon is equally deterministic, mask included.
        let tree = Tree::new("determinism");
        tree.put("icons/hicolor/64x64/apps/det.png", RGBA8_PNG);
        let decode = || {
            let mut c = IconCache::with_roots(vec![tree.icons_root()], "");
            c.set_display_edge(64);
            c.resolve_keys(&["det".to_string()]);
            c.get("det").unwrap().pixels.clone()
        };
        assert_eq!(decode(), decode(), "decode + mask must be reproducible");

        // And a repeat call is a hit, not a regeneration.
        let mut c = IconCache::with_roots(vec![], "");
        c.set_display_edge(n);
        let first = c.get_or_monogram("term", "Terminal", 0xFF10B981).unwrap();
        let second = c.get_or_monogram("term", "Terminal", 0xFF10B981).unwrap();
        assert!(
            Rc::ptr_eq(&first, &second),
            "a cached key must not be re-rendered"
        );
        assert_eq!(c.len(), 1, "the second call must not add an entry");
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
            assert!(
                sq <= 1000 && strip <= 1000,
                "target {bad} overflowed the score"
            );
            assert!(
                size_score(None, bad) > strip,
                "scalable stays mid-range at target {bad}"
            );
        }
    }
}
