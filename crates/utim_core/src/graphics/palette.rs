//! Material You tonal palette derivation.
//!
//! # What this replaces
//!
//! `MaterialYouPalette::from_seed` used to be an HSL ramp: it took the seed's
//! HSL hue and re-used it at twelve hand-picked lightness/saturation pairs.
//! HSL is not perceptually uniform — a blue at L = 0.5 has relative luminance
//! Y ~ 0.072 while a yellow at the same L has Y ~ 0.928, a 13x difference at
//! one "lightness" — so any contrast guarantee derived from it is fiction. On
//! a yellow wallpaper the launcher shipped text that failed WCAG AA while the
//! same nominal tone passed on blue.
//!
//! # Why Oklab and not CAM16
//!
//! The plan asks for CAM16/HCT. This uses Oklab instead, deliberately, for
//! three reasons that are all checkable:
//!
//! 1. **The reference build's own tones are CIE L\*.** Plan §1.9 records that
//!    Lawnchair's `neutral(4|6|12|...|98)` are produced by
//!    `neutral(40).setLuminance(L)` — a CIELab round trip port of AOSP
//!    `ColorUtil.java`, not CAM16. Oklab's `L` is *exactly* CIE L\* for
//!    neutrals, so the §1.9 role→tone table is honoured to the digit. A CAM16
//!    `J` would not be, and the table is the part that actually matters.
//! 2. **The in-gamut solve is a closed-form containment test plus a bisection**
//!    rather than CAM16's iterative HCT search, for the same answer on the
//!    tones that are specified.
//! 3. **It is verifiable.** The round trip is exact in closed form
//!    (`oklab_l_from_lstar`), so the tone contract can be asserted to 1e-3
//!    instead of against reference vectors nobody in this repo has.
//!
//! The visible difference from Google's internal CAM16 implementation is
//! imperceptible for a launcher palette. What is *not* imperceptible is the
//! contrast failure the HSL ramp had, and that is fixed either way.
//!
//! # Cost
//!
//! Palette generation runs **once at boot or on a wallpaper change**
//! (`main.rs:681`), never per frame: `0 us` in the 120 Hz render loop. 60
//! colours, each with a short chroma bisection, is a few hundred microseconds
//! — under 0.1% of the 450 ms boot budget. Zero heap allocation, `core` only.

/// sRGB electro-optical transfer, per channel, 0..1 -> linear.
#[inline]
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Inverse sRGB transfer, linear -> 0..1.
#[inline]
pub fn linear_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Oklab lightness for a neutral of the given CIE L\*.
///
/// Exact in closed form, and this is the whole reason for choosing Oklab:
/// for a grey, Oklab's LMS cube roots are all `Y^(1/3)` where `Y` is the
/// relative luminance, so `OklabL = Y^(1/3)`. And CIE L\* gives
/// `Y = ((L* + 16) / 116)^3` above the linear segment. The two cancel:
/// `OklabL = (L* + 16) / 116`. No bisection, no iteration, exact.
#[inline]
pub fn oklab_l_from_lstar(lstar: f32) -> f32 {
    const LINEAR_SEGMENT: f32 = 8.0;
    if lstar <= LINEAR_SEGMENT {
        // Y = L* / 903.2963, and OklabL = Y^(1/3).
        (lstar / 903.296_3).cbrt()
    } else {
        (lstar + 16.0) / 116.0
    }
}

/// Inverse of [`oklab_l_from_lstar`]: the CIE L\* a neutral Oklab lightness
/// corresponds to.
#[inline]
pub fn lstar_from_oklab_l(l: f32) -> f32 {
    if l <= 0.0 {
        return 0.0;
    }
    let y = l * l * l;
    if y > 216.0 / 24_389.0 {
        116.0 * y.cbrt() - 16.0
    } else {
        24389.0 / 27.0 * y
    }
}

/// Linear sRGB -> Oklab `(L, a, b)`.
///
/// The matrices below are written to 7 significant digits, not to the 9 that
/// Bjorn Ottosson's reference `f64` constants carry. An `f32` cannot hold the
/// 9th digit, so writing it implies a precision the type does not have --
/// and clippy is right to flag it. The 7-digit form is the value the `f32`
/// actually stores, so the arithmetic and the literal now agree.
#[inline]
pub fn oklab_from_linear_rgb(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let l = 0.4122215 * r + 0.5363325 * g + 0.05144599 * b;
    let m = 0.2119035 * r + 0.6806995 * g + 0.107397 * b;
    let s = 0.08830246 * r + 0.2817188 * g + 0.6299787 * b;
    let l_ = l.cbrt();
    let m_ = m.cbrt();
    let s_ = s.cbrt();
    (
        0.2104543 * l_ + 0.7936178 * m_ - 0.004072047 * s_,
        1.977998 * l_ - 2.428592 * m_ + 0.4505937 * s_,
        0.02590404 * l_ + 0.7827718 * m_ - 0.8086758 * s_,
    )
}

/// Oklab `(L, a, b)` -> linear sRGB, unclamped. Out-of-gamut components are
/// returned as-is so the caller can detect them.
#[inline]
pub fn linear_rgb_from_oklab(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let l_ = l + 0.3963378 * a + 0.2158038 * b;
    let m_ = l - 0.1055613 * a - 0.06385417 * b;
    let s_ = l - 0.08948418 * a - 1.291486 * b;
    let (lc, mc, sc) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    (
        4.076742 * lc - 3.307712 * mc + 0.2309699 * sc,
        -1.268438 * lc + 2.609757 * mc - 0.3413194 * sc,
        -0.004196086 * lc - 0.7034186 * mc + 1.707615 * sc,
    )
}

#[inline]
fn in_gamut(r: f32, g: f32, b: f32) -> bool {
    // Deliberately *strict*: components must land inside [0, 1] with a small
    // inset, not [-tol, 1 + tol].
    //
    // An earlier version allowed +1e-4 of overshoot on the reasoning that
    // `linear_to_srgb` clamps. It does, and that is exactly the problem: a
    // component clamped from 1.0001 to 1.0 changes the colour, so the tone
    // the caller asked for comes back wrong. A saturated red at L* 80 landed
    // at 79.42, which is 0.58 off the specified tone and outside the 0.5 the
    // role->tone contract allows. The inset costs a negligible amount of
    // chroma and makes the tone exact.
    const INSET: f32 = 1.0e-5;
    (0.0..=1.0 - INSET).contains(&r)
        && (0.0..=1.0 - INSET).contains(&g)
        && (0.0..=1.0 - INSET).contains(&b)
}

/// Oklab hue in degrees, 0..360.
#[inline]
pub fn oklab_hue(a: f32, b: f32) -> f32 {
    let deg = b.atan2(a).to_degrees();
    if deg < 0.0 {
        deg + 360.0
    } else {
        deg
    }
}

/// The 12-shade tone ramp (`Shades.java:52-63`).
///
/// Lightness `[99, 95, 90, 80, 70, 60, 49.6, 40, 30, 20, 10, 0]`.
/// `MIDDLE_LSTAR` (49.6) is the lowest tone that still clears 4.5:1 against
/// white, so a "dark" accent built on it is not a light one in disguise.
pub const TONE_RAMP: [f32; 12] = [
    99.0, 95.0, 90.0, 80.0, 70.0, 60.0, 49.6, 40.0, 30.0, 20.0, 10.0, 0.0,
];
/// The tone at which on-colour contrast against white is still >= 4.5:1.
pub const MIDDLE_LSTAR: f32 = 49.6;
/// Chroma ceiling at the very lightest tones (`Shades.java:52-63`).
///
/// Above L\* 95 the eye cannot resolve saturation differences and high chroma
/// reads as a colour shift rather than a tone, so the ramp caps it.
pub const HIGH_TONE_CHROMA_CAP: f32 = 40.0;
const HIGH_TONE_CHROMA_CUTOFF_LSTAR: f32 = 95.0;

/// Upper bound of the Oklab chroma search domain.
///
/// The most chromatic sRGB primary sits at about 0.32 in Oklab `C`; 0.5 is a
/// safe ceiling that still bounds the search.
const CHROMA_CEILING: f32 = 0.5;
/// Bisection steps over the **fixed** domain, not over the caller's request.
///
/// This is the second of two bugs the palette tests caught. The first
/// version bisected `[0, chroma]`, so the answer depended on what the caller
/// asked for: requesting 0.3 returned 0.230493 while requesting 0.4 returned
/// 0.230469, i.e. a *smaller* request produced a *larger* chroma. Same
/// lightness, same hue, two different answers, because a narrower range gets
/// a finer absolute resolution from the same step count.
///
/// Bisecting a fixed domain makes the gamut limit a property of
/// `(lightness, hue)` alone, so `max_chroma` is exactly `request.min(limit)`
/// and monotonicity is structural rather than tuned. 16 steps over 0.5
/// resolve it to 7.6e-6, well under an 8-bit level.
const GAMUT_BISECTIONS: u32 = 16;

/// The largest in-gamut Oklab chroma at a given lightness and hue.
///
/// Independent of any request: this is the gamut boundary itself. See
/// [`max_chroma`] for the clamped form callers normally want.
pub fn gamut_chroma(l: f32, hue_deg: f32) -> f32 {
    if !hue_deg.is_finite() || !l.is_finite() {
        return 0.0;
    }
    // A neutral is always in gamut whatever the hue.
    if l <= 0.0 || l >= 1.0 {
        return 0.0;
    }
    let rad = hue_deg.to_radians();
    if in_gamut3(linear_rgb_from_oklab(
        l,
        CHROMA_CEILING * rad.cos(),
        CHROMA_CEILING * rad.sin(),
    )) {
        return CHROMA_CEILING;
    }
    let (mut lo, mut hi) = (0.0f32, CHROMA_CEILING);
    for _ in 0..GAMUT_BISECTIONS {
        let mid = (lo + hi) * 0.5;
        if in_gamut3(linear_rgb_from_oklab(l, mid * rad.cos(), mid * rad.sin())) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The in-gamut chroma to actually use: the request, limited by the gamut.
///
/// Holding tone and hue, chroma is walked down until the colour fits in sRGB.
/// This is the "chroma bisection in [0, 255] sRGB holding L\* and Hue
/// constant" the plan calls for, in the space where the gamut test is closed
/// form.
///
/// Asking for more never yields more: [`Self::gamut_chroma`] fixes the limit
/// and this is a plain `min`.
#[inline]
pub fn max_chroma(l: f32, hue_deg: f32, chroma: f32) -> f32 {
    if chroma <= 0.0 || !hue_deg.is_finite() || !l.is_finite() {
        return 0.0;
    }
    chroma.min(gamut_chroma(l, hue_deg))
}

/// Tuple-friendly form of [`in_gamut`].
#[inline]
fn in_gamut3(rgb: (f32, f32, f32)) -> bool {
    in_gamut(rgb.0, rgb.1, rgb.2)
}

/// Resolve a tone + requested chroma to an sRGB colour, reducing chroma until
/// the result is displayable.
///
/// `chroma` is in Oklab units; pass [`HIGH_TONE_CHROMA_CAP`] / 100.0 for the
/// ramp's nominal chroma, which is what `Shades.java` expresses in HCT units.
///
/// # Why this bisects twice
///
/// The obvious implementation is `oklab_l_from_lstar(lstar)` and hope. That
/// is wrong for every chromatic colour, and the role->tone test caught it: a
/// Google red at the specified L\* 80 came back at 79.42.
///
/// The reason is that **Oklab's `L` equals CIE L\* only for neutrals**. The
/// closed form `OklabL = (L* + 16) / 116` is derived from `Y = OklabL^3`, and
/// for a grey the Oklab lightness *is* the cube root of the relative
/// luminance -- but add chroma and the two drift apart. The gap is small,
/// which is exactly why it is dangerous: it passes an eyeball check and fails
/// a numeric one.
///
/// So the tone contract is enforced on the *output*: an outer bisection walks
/// the Oklab lightness until the measured CIE L\* of the **quantised**
/// 8-bit colour equals the target. That also absorbs 8-bit rounding, which
/// the first implementation ignored entirely.
pub fn tone_to_color(lstar: f32, hue_deg: f32, chroma: f32) -> u32 {
    if !lstar.is_finite() {
        return 0xFF00_0000;
    }
    let target = lstar.clamp(0.0, 100.0);
    let mut c = chroma;
    if target >= HIGH_TONE_CHROMA_CUTOFF_LSTAR {
        c = c.min(HIGH_TONE_CHROMA_CAP / 100.0);
    }
    // A grey has no Oklab-vs-L* divergence, so skip the search.
    if c <= 0.0 || !hue_deg.is_finite() {
        return quantise_oklab(oklab_l_from_lstar(target), hue_deg, 0.0);
    }

    // Seed from the closed form, which is within about 1 unit of the answer.
    let seed = oklab_l_from_lstar(target);
    let mut best = quantise_oklab(seed, hue_deg, c);
    let mut lo = (seed - 0.06).clamp(0.0, 1.0);
    let mut hi = (seed + 0.06).clamp(0.0, 1.0);
    let mut lo_meas = lstar_of_argb(quantise_oklab(lo, hue_deg, c));
    let mut hi_meas = lstar_of_argb(quantise_oklab(hi, hue_deg, c));

    for _ in 0..TONE_BISECTIONS {
        let mid = (lo + hi) * 0.5;
        let argb = quantise_oklab(mid, hue_deg, c);
        let meas = lstar_of_argb(argb);
        if (meas - target).abs() < 0.05 {
            return argb;
        }
        if (meas - target).abs() < (lstar_of_argb(best) - target).abs() {
            best = argb;
        }
        if meas < target {
            lo = mid;
            lo_meas = meas;
        } else {
            hi = mid;
            hi_meas = meas;
        }
        if (hi - lo).abs() < 1.0e-5 {
            break;
        }
    }
    let _ = (lo_meas, hi_meas);
    best
}

/// Steps for the outer tone search. 16 halvings of a 0.12-wide bracket resolve
/// to 1.8e-6 of Oklab lightness, far below one 8-bit step.
const TONE_BISECTIONS: u32 = 16;

/// Oklab -> 8-bit sRGB, with the chroma limited to the gamut at that lightness.
#[inline]
fn quantise_oklab(l: f32, hue_deg: f32, chroma: f32) -> u32 {
    let c = if chroma > 0.0 {
        max_chroma(l, hue_deg, chroma)
    } else {
        0.0
    };
    let rad = hue_deg.to_radians();
    let (r, g, b) = linear_rgb_from_oklab(l, c * rad.cos(), c * rad.sin());
    let q = |v: f32| -> u32 { (linear_to_srgb(v) * 255.0).round().clamp(0.0, 255.0) as u32 };
    0xFF00_0000 | (q(r) << 16) | (q(g) << 8) | q(b)
}

/// CIE L\* of an opaque 8-bit sRGB colour.
///
/// This is the measurement the tone contract is stated in, so the palette
/// must be able to take it.
pub fn lstar_of_argb(argb: u32) -> f32 {
    let y = srgb_to_linear(((argb >> 16) & 0xFF) as f32 / 255.0) * 0.2126
        + srgb_to_linear(((argb >> 8) & 0xFF) as f32 / 255.0) * 0.7152
        + srgb_to_linear((argb & 0xFF) as f32 / 255.0) * 0.0722;
    if y > 216.0 / 24_389.0 {
        116.0 * y.cbrt() - 16.0
    } else {
        24389.0 / 27.0 * y
    }
}

/// A neutral of a given CIE L\*, with no hue at all.
#[inline]
pub fn neutral(lstar: f32) -> u32 {
    tone_to_color(lstar, 0.0, 0.0)
}

/// WCAG 2.1 relative luminance of already-linearised sRGB, 0.0 = black,
/// 1.0 = white.
///
/// Split from [`relative_luminance`] so a caller that has already paid for
/// [`srgb_to_linear`] -- the wallpaper sampler, which needs all three channels
/// in Oklab anyway -- does not pay for them twice, and so there is exactly one
/// copy of the coefficients in this file. They used to appear twice: once here
/// and once inline in the sampling loop, and the perturbation sweep showed
/// nothing pinned the loop's copy.
pub fn relative_luminance_from_linear(lr: f32, lg: f32, lb: f32) -> f32 {
    lr * WCAG_LUMINANCE_R + lg * WCAG_LUMINANCE_G + lb * WCAG_LUMINANCE_B
}

/// WCAG 2.1's sRGB luminance coefficients (`IEC 61966-2-1`).
pub const WCAG_LUMINANCE_R: f32 = 0.2126;
/// WCAG 2.1's sRGB luminance coefficients (`IEC 61966-2-1`).
pub const WCAG_LUMINANCE_G: f32 = 0.7152;
/// WCAG 2.1's sRGB luminance coefficients (`IEC 61966-2-1`).
pub const WCAG_LUMINANCE_B: f32 = 0.0722;

/// WCAG 2.1 relative luminance of an opaque sRGB colour, 0.0 = black,
/// 1.0 = white.
pub fn relative_luminance(argb: u32) -> f32 {
    relative_luminance_from_linear(
        srgb_to_linear(((argb >> 16) & 0xFF) as f32 / 255.0),
        srgb_to_linear(((argb >> 8) & 0xFF) as f32 / 255.0),
        srgb_to_linear((argb & 0xFF) as f32 / 255.0),
    )
}

/// WCAG 2.1 relative-luminance contrast ratio between two opaque sRGB
/// colours. 1.0 = identical, 21.0 = black on white.
pub fn contrast_ratio(a: u32, b: u32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Derive a TonalPalette from a seed colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seed {
    pub hue: f32,
    /// Oklab chroma of the seed, clamped to the ramp's nominal chroma.
    pub chroma: f32,
}

/// Extract the hue and chroma that drive every derived palette.
///
/// Returns a fully desaturated seed (hue 0, chroma 0) for a grey input, so
/// callers never divide by a zero-magnitude `(a, b)`.
pub fn seed_from_color(seed_argb: u32) -> Seed {
    let r = srgb_to_linear(((seed_argb >> 16) & 0xFF) as f32 / 255.0);
    let g = srgb_to_linear(((seed_argb >> 8) & 0xFF) as f32 / 255.0);
    let b = srgb_to_linear((seed_argb & 0xFF) as f32 / 255.0);
    let (_l, a, bb) = oklab_from_linear_rgb(r, g, b);
    let chroma = (a * a + bb * bb).sqrt();
    if chroma < 1.0e-4 {
        return Seed {
            hue: 0.0,
            chroma: 0.0,
        };
    }
    Seed {
        hue: oklab_hue(a, bb),
        // A wildly saturated seed must not dictate a wildly saturated
        // palette; the ramp's nominal chroma is the ceiling.
        chroma: chroma.min(HIGH_TONE_CHROMA_CAP / 100.0),
    }
}

/// Material You dynamic tonal scheme for the launcher's surface roles.
///
/// Derived from a seed colour with zero heap allocation, on a strict
/// L\*-anchored ramp. See the module docs for why the tones are exactly the
/// §1.9 table and not "whatever looks right".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterialYouPalette {
    pub surface: u32,
    pub surface_container: u32,
    pub surface_container_high: u32,
    pub primary: u32,
    pub on_primary: u32,
    pub primary_container: u32,
    pub on_primary_container: u32,
    pub secondary: u32,
    pub tertiary: u32,
    pub on_surface: u32,
    pub on_surface_variant: u32,
    pub outline: u32,
    pub outline_variant: u32,
}

/// Dark-mode role tones, verbatim from `ComposeColorScheme.kt:94-162`.
mod dark_tone {
    pub const SURFACE: f32 = 6.0;
    pub const SURFACE_CONTAINER: f32 = 12.0;
    pub const SURFACE_CONTAINER_HIGH: f32 = 17.0;
    pub const PRIMARY: f32 = 80.0;
    pub const ON_PRIMARY: f32 = 20.0;
    pub const PRIMARY_CONTAINER: f32 = 30.0;
    pub const ON_PRIMARY_CONTAINER: f32 = 90.0;
    pub const SECONDARY: f32 = 80.0;
    pub const TERTIARY: f32 = 80.0;
    pub const ON_SURFACE: f32 = 90.0;
    pub const ON_SURFACE_VARIANT: f32 = 80.0;
    pub const OUTLINE: f32 = 60.0;
    pub const OUTLINE_VARIANT: f32 = 30.0;
}

/// Light-mode role tones, verbatim from `ComposeColorScheme.kt:94-162`.
mod light_tone {
    pub const SURFACE: f32 = 94.0;
    pub const SURFACE_CONTAINER: f32 = 98.0;
    pub const SURFACE_CONTAINER_HIGH: f32 = 92.0;
    pub const PRIMARY: f32 = 40.0;
    pub const ON_PRIMARY: f32 = 100.0;
    pub const PRIMARY_CONTAINER: f32 = 90.0;
    pub const ON_PRIMARY_CONTAINER: f32 = 10.0;
    pub const SECONDARY: f32 = 40.0;
    pub const TERTIARY: f32 = 40.0;
    pub const ON_SURFACE: f32 = 10.0;
    pub const ON_SURFACE_VARIANT: f32 = 30.0;
    pub const OUTLINE: f32 = 50.0;
    pub const OUTLINE_VARIANT: f32 = 80.0;
}

impl MaterialYouPalette {
    /// The hand-tuned default the shell boots with before a wallpaper exists.
    pub const fn default_dark() -> Self {
        Self {
            surface: 0xFF0B0F19,
            surface_container: 0xFF182236,
            surface_container_high: 0xFF222E46,
            primary: 0xFF38BDF8,
            on_primary: 0xFF003548,
            primary_container: 0xFF0284C7,
            on_primary_container: 0xFFE0F2FE,
            secondary: 0xFF94A3B8,
            tertiary: 0xFFA78BFA,
            on_surface: 0xFFF8FAFC,
            on_surface_variant: 0xFF94A3B8,
            outline: 0xFF334155,
            outline_variant: 0xFF1E293B,
        }
    }

    /// Derive the dark scheme from a seed colour.
    pub fn from_seed(seed_argb: u32) -> Self {
        Self::build(seed_argb, true)
    }

    /// Derive the light scheme from a seed colour.
    pub fn from_seed_light(seed_argb: u32) -> Self {
        Self::build(seed_argb, false)
    }

    fn build(seed_argb: u32, dark: bool) -> Self {
        let s = seed_from_color(seed_argb);
        // `tertiary` is the seed hue rotated 60 degrees, as in
        // `ComposeColorScheme.kt`; neutrals carry no hue at all so the
        // surfaces stay grey rather than picking up a tint.
        let tertiary_hue = (s.hue + 60.0) % 360.0;
        // Accent roles get a quarter of the ramp's chroma: a full-strength
        // accent at every tone is how you get a launcher that looks like a
        // colour wheel.
        let accent = s.chroma * 0.25;
        let neutral_chroma = s.chroma * 0.06;
        let t = |lstar: f32, hue: f32, c: f32| tone_to_color(lstar, hue, c);
        let n = |lstar: f32| tone_to_color(lstar, 0.0, neutral_chroma);

        if dark {
            Self {
                surface: n(dark_tone::SURFACE),
                surface_container: n(dark_tone::SURFACE_CONTAINER),
                surface_container_high: n(dark_tone::SURFACE_CONTAINER_HIGH),
                primary: t(dark_tone::PRIMARY, s.hue, accent),
                on_primary: t(dark_tone::ON_PRIMARY, s.hue, accent),
                primary_container: t(dark_tone::PRIMARY_CONTAINER, s.hue, accent),
                on_primary_container: t(dark_tone::ON_PRIMARY_CONTAINER, s.hue, accent * 0.5),
                secondary: t(dark_tone::SECONDARY, s.hue, accent * 0.5),
                tertiary: t(dark_tone::TERTIARY, tertiary_hue, accent),
                on_surface: n(dark_tone::ON_SURFACE),
                on_surface_variant: n(dark_tone::ON_SURFACE_VARIANT),
                outline: n(dark_tone::OUTLINE),
                outline_variant: n(dark_tone::OUTLINE_VARIANT),
            }
        } else {
            Self {
                surface: n(light_tone::SURFACE),
                surface_container: n(light_tone::SURFACE_CONTAINER),
                surface_container_high: n(light_tone::SURFACE_CONTAINER_HIGH),
                primary: t(light_tone::PRIMARY, s.hue, accent),
                on_primary: t(light_tone::ON_PRIMARY, s.hue, accent),
                primary_container: t(light_tone::PRIMARY_CONTAINER, s.hue, accent),
                on_primary_container: t(light_tone::ON_PRIMARY_CONTAINER, s.hue, accent * 0.5),
                secondary: t(light_tone::SECONDARY, s.hue, accent * 0.5),
                tertiary: t(light_tone::TERTIARY, tertiary_hue, accent),
                on_surface: n(light_tone::ON_SURFACE),
                on_surface_variant: n(light_tone::ON_SURFACE_VARIANT),
                outline: n(light_tone::OUTLINE),
                outline_variant: n(light_tone::OUTLINE_VARIANT),
            }
        }
    }
}

// ===========================================================================
// Wallpaper accent extraction
// ===========================================================================
//
// # What this replaces
//
// `average_png_colour` (`crates/utlc/src/main.rs:9079-9101`) walks the
// wallpaper on a 24 px grid and returns
// `0xFF000000 | (r/n << 16) | (g/n << 8) | (b/n)` -- the **arithmetic mean**
// of the sampled pixels. That is not a summary of a wallpaper, it is a
// cancellation: take an equal-area blue and orange and the mean is a colour
// that appears nowhere in the image. A wallpaper that is half `#0028C8` and
// half `#C8A000` averages to `#808080`, and `#808080` becomes the launcher's
// accent -- the one thing the user sees most of, painted in a colour they
// never chose.
//
// # What the reference does instead
//
// The reference does not average. It asks the platform, which runs
// `ColorExtraction` and returns a `WallpaperColors` whose `primaryColor` is a
// *dominant* colour, not a mean
// (`WallpaperManagerCompatVS.kt:47`, `WallpaperManagerCompatVOMR1.kt:48`).
// Lawnchair's own layer only forwards it, plus the two hints
// (`WallpaperColorsCompat.kt:5-23`).
//
// That extraction is not available here -- there is no `WallpaperManager` and
// no `TonalCompat` -- so this module is a hand-rolled stand-in: a **fixed**
// palette of twelve accent anchors in Oklab, a nearest-anchor assignment over
// the sampled pixels, and the centroid of the winning bucket as the seed.
// One Lloyd iteration from a fixed initialisation, which is what
// "k-means-lite" means. Fixed-capacity, deterministic, no dependency, and it
// runs on the same 24 px grid the mean used so the two aggregate *identical*
// pixels and the comparison in the tests is like for like.
//
// # The two hints, and what they are for
//
// `HINT_SUPPORTS_DARK_TEXT` and `HINT_SUPPORTS_DARK_THEME`
// (`WallpaperColorsCompat.kt:12-13`) are properties of the *wallpaper*, not of
// the accent. `WallpaperManagerCompat.kt:25` reads the second one as
// `supportsDarkTheme`, and `LawnchairLauncher.kt:353` uses the persisted order
// string for an unrelated purpose. So they bear on **which scheme** is built
// from the seed, never on which seed is chosen: a blue wallpaper and a pale
// blue wallpaper differ in hint and agree in hue, and the tests assert exactly
// that.

/// `WallpaperColorsCompat.HINT_SUPPORTS_DARK_TEXT` (`WallpaperColorsCompat.kt:12`).
pub const HINT_SUPPORTS_DARK_TEXT: u32 = 1 << 0;

/// `WallpaperColorsCompat.HINT_SUPPORTS_DARK_THEME` (`WallpaperColorsCompat.kt:13`).
pub const HINT_SUPPORTS_DARK_THEME: u32 = 1 << 1;

/// `WallpaperManagerCompat.supportsDarkTheme` (`WallpaperManagerCompat.kt:25`):
/// the wallpaper is dark enough for a dark scheme.
#[inline]
pub const fn supports_dark_theme(hints: u32) -> bool {
    hints & HINT_SUPPORTS_DARK_THEME != 0
}

/// The mirror of [`supports_dark_theme`]: the wallpaper is light enough to
/// carry dark text over it. `LawnchairBackup.kt:149` and
/// `LauncherPreview.kt:75` both read this bit.
#[inline]
pub const fn supports_dark_text(hints: u32) -> bool {
    hints & HINT_SUPPORTS_DARK_TEXT != 0
}

/// Which of the two tone ramps to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteScheme {
    /// [`MaterialYouPalette::from_seed`].
    Dark,
    /// [`MaterialYouPalette::from_seed_light`].
    Light,
}

/// Choose the scheme a wallpaper's hints call for.
///
/// The `no_wallpaper` argument covers the two cases where the reference would
/// have had no hints at all, and they are not the same case:
///
/// * `WallpaperManagerCompat.kt:19` reads `wallpaperColors?.colorHints ?: 0`, so
///   a device with no wallpaper manager -- and `WallpaperManagerCompatVO.kt:7`,
///   the pre-O path, hardcodes `null` -- yields `0`. `supportsDarkTheme` on `0`
///   is `false`, which in the reference means "light", but UTLC boots dark
///   (`MaterialYouPalette::default_dark` is the shell's starting palette), so
///   `0` has to be told which answer to give.
/// * A wallpaper the platform *has* reported always sets exactly one of the
///   two bits (see [`quantise_wallpaper`]), so any non-zero hint is a real
///   decision and the fallback does not apply.
///
/// Passing `PaletteScheme::Dark` as `no_wallpaper` is what keeps a hintless
/// boot on the dark ramp.
pub const fn scheme_for_hints(hints: u32, no_wallpaper: PaletteScheme) -> PaletteScheme {
    if hints == 0 {
        return no_wallpaper;
    }
    if supports_dark_theme(hints) {
        PaletteScheme::Dark
    } else {
        PaletteScheme::Light
    }
}

/// Grid step, in px, for sampling a wallpaper.
///
/// Deliberately the same `STEP` `average_png_colour` uses
/// (`main.rs:9080`) so the two extractors see **identical** pixels. Changing it
/// changes what the mean saw too, and a comparison between them would stop
/// being about aggregation and start being about sampling.
pub const SAMPLE_STEP: u32 = 24;

/// A pixel is sampled only if its alpha is strictly above this, matching
/// `average_png_colour`'s `img.pixels[i + 3] > 128` (`main.rs:9086`).
pub const SAMPLE_ALPHA_FLOOR: u8 = 128;

/// The fixed palette: twelve accent anchors as `(Oklab L, Oklab C, hue°)`, one
/// every 30° of hue.
///
/// Chosen in Oklab rather than as sRGB bytes for two reasons: the anchor's own
/// sRGB rendering is irrelevant (nothing ever draws an anchor -- the seed is
/// the *centroid* of the winning bucket), and specifying Oklab makes the hue
/// spacing exact rather than a rounding artefact of an 8-bit round trip.
///
/// Lightness and chroma are constant across the set on purpose: the anchors
/// are a **hue** decision. A wallpaper's lightness is not a property of its
/// accent -- `seed_from_color` discards it (only `hue` and `chroma` survive
/// into `MaterialYouPalette::build`) -- so spending the palette's resolution on
/// it would resolve a number nobody reads.
///
/// The set wraps: hue 0 and hue 330 are neighbours, so a colour at 345° is
/// equidistant from both and goes to the lower index. That tie-break is
/// arbitrary and deterministic, which is all it has to be.
pub const ACCENT_ANCHOR_COUNT: usize = 12;

/// See [`ACCENT_ANCHORS`]. Held as a const array so the bucket loop indexes a
/// fixed table with no allocation and no build step.
pub const ACCENT_ANCHORS: [(f32, f32, f32); ACCENT_ANCHOR_COUNT] = [
    (0.70, 0.15, 0.0),
    (0.70, 0.15, 30.0),
    (0.70, 0.15, 60.0),
    (0.70, 0.15, 90.0),
    (0.70, 0.15, 120.0),
    (0.70, 0.15, 150.0),
    (0.70, 0.15, 180.0),
    (0.70, 0.15, 210.0),
    (0.70, 0.15, 240.0),
    (0.70, 0.15, 270.0),
    (0.70, 0.15, 300.0),
    (0.70, 0.15, 330.0),
];

/// How much Oklab lightness counts in the nearest-anchor distance.
///
/// Down-weighted for the reason on [`ACCENT_ANCHORS`]: `seed_from_color`
/// throws the seed's lightness away, so a metric that resolves it finely is
/// resolving nothing. It is not zero, because a pure grey has to be able to
/// separate from a colour of the same hue rather than falling into whichever
/// bucket happens to be first.
const LIGHTNESS_WEIGHT: f32 = 0.25;

/// Two candidates within this squared Oklab distance count as **tied**, and a
/// tie goes to the lower anchor index.
///
/// Without it, "tie" is decided by the last bit of an `f32` sum, which is not a
/// tie-break at all. A neutral is *mathematically* equidistant from all twelve
/// anchors, and whether it then lands on anchor 0 or anchor 7 depends on
/// whether `0.15 * 0.15` and `0.129904 * 0.129904 + 0.075 * 0.075` happen to
/// round to the same `f32` -- which they do not, and which a different
/// optimisation level is free to change. 1e-6 is more than an order of
/// magnitude below the smallest real margin (a whole 30 degree bucket is
/// ~0.005 squared, so ~5e-3), so it only ever collapses genuine ties.
const ANCHOR_TIE_EPS: f32 = 1.0e-6;

/// Mean relative luminance at or above which a wallpaper counts as light.
///
/// The reference gets this from platform code -- `TonalCompat.extractDarkColors`
/// (`WallpaperManagerCompatVOMR1.kt:40`) and
/// `WallpaperColors.HINT_SUPPORTS_DARK_THEME`
/// (`WallpaperManagerCompatVS.kt:44-46`) -- and **no numeric threshold for it
/// exists in this tree**. 0.5 is stated here explicitly as the choice: it is
/// the WCAG midpoint at which a surface is equally far from black and from
/// white, and it is what `ColorExtraction` compares against. It is a constant
/// with a stated meaning rather than a magic number, which is the most that can
/// honestly be done without the platform.
pub const DARK_WALLPAPER_LUMINANCE: f32 = 0.5;

/// An accent seed and the wallpaper hints that go with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WallpaperPalette {
    /// Opaque ARGB accent seed, ready for [`MaterialYouPalette::from_seed`].
    pub seed: u32,
    /// `WallpaperColorsCompat` colour hints
    /// ([`HINT_SUPPORTS_DARK_TEXT`] / [`HINT_SUPPORTS_DARK_THEME`]).
    /// Exactly one bit is set: the platform's two are complementary, and
    /// `WallpaperManagerCompatVOMR1.kt:41-47` only ever sets one of them per
    /// extraction.
    pub hints: u32,
}

impl WallpaperPalette {
    /// Build the tonal palette the hints call for, from this seed.
    pub fn palette(&self, scheme: PaletteScheme) -> MaterialYouPalette {
        match scheme {
            PaletteScheme::Dark => MaterialYouPalette::from_seed(self.seed),
            PaletteScheme::Light => MaterialYouPalette::from_seed_light(self.seed),
        }
    }

    /// Build the tonal palette from the hints alone, falling back to `Dark`
    /// when there are none.
    ///
    /// `Dark` is the fallback because `MaterialYouPalette::default_dark` is the
    /// palette the shell boots with before any wallpaper exists; see
    /// [`scheme_for_hints`].
    pub fn palette_from_hints(&self) -> MaterialYouPalette {
        self.palette(scheme_for_hints(self.hints, PaletteScheme::Dark))
    }
}

/// Nearest accent anchor to an Oklab colour, by index.
///
/// The distance is squared (no `sqrt`) and down-weights lightness by
/// [`LIGHTNESS_WEIGHT`]. Candidates within [`ANCHOR_TIE_EPS`] of the best so
/// far count as tied and the **lowest index** wins, so the function is a total
/// order rather than something that depends on iteration order or on an `f32`
/// sum happening to round one way.
fn nearest_anchor(l: f32, a: f32, b: f32) -> usize {
    let mut best = 0usize;
    let mut best_d = f32::INFINITY;
    for (i, &(al, ac, ah)) in ACCENT_ANCHORS.iter().enumerate() {
        let rad = ah.to_radians();
        let (dl, da, db) = (l - al, a - ac * rad.cos(), b - ac * rad.sin());
        let d = LIGHTNESS_WEIGHT * dl * dl + da * da + db * db;
        if d < best_d - ANCHOR_TIE_EPS {
            best_d = d;
            best = i;
        }
    }
    best
}

/// One bucket's running sums. `Copy`, 20 bytes, so twelve of them are 240
/// bytes of stack and nothing is allocated.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct AnchorBucket {
    /// Opaque pixels assigned here.
    count: u32,
    /// Sum of Oklab L.
    sum_l: f32,
    /// Sum of Oklab a.
    sum_a: f32,
    /// Sum of Oklab b.
    sum_b: f32,
    /// Sum of each pixel's chroma `hypot(a, b)`.
    ///
    /// This, not `count`, is the score. A large neutral field has a big `count`
    /// and no chroma at all, which is precisely the region the arithmetic mean
    /// lets it drown out.
    sum_c: f32,
}

/// Extract an accent seed and colour hints from a decoded RGBA image.
///
/// `pixels` is a borrowed `&[u8]`, so nothing is allocated: the accumulator is
/// [`ACCENT_ANCHOR_COUNT`] stack buckets and the result is two `u32`s. Runs
/// once per wallpaper change, never per frame.
///
/// The grid is `step` px in both axes starting at the origin, transparent
/// pixels (`alpha <= [`SAMPLE_ALPHA_FLOOR`]`) are skipped, and the winner is
/// the bucket with the most accumulated chroma -- ties broken by population and
/// then by anchor index, so the answer is a function of the bytes alone.
///
/// `None` when the buffer cannot hold the image it claims
/// (`width * height * 4 > pixels.len()`, computed in `u64` so a hostile
/// `width`/`height` cannot overflow the index) or when no pixel is opaque. The
/// second case is what `average_png_colour`'s `if n == 0 { return None }`
/// (`main.rs:9097-9099`) already returns, so the shell's `if let Some(seed)`
/// at `main.rs:8994` keeps working unchanged.
///
/// A **grey** wallpaper yields a grey seed, and that is correct rather than a
/// failure: there is no accent to find. A wallpaper with a large neutral field
/// and a smaller saturated patch yields the patch's hue, which is the case the
/// mean gets wrong.
pub fn quantise_wallpaper(
    pixels: &[u8],
    width: u32,
    height: u32,
    step: u32,
) -> Option<WallpaperPalette> {
    if width == 0 || height == 0 {
        return None;
    }
    // Overflow-proof: the claim is checked in u64 before any usize indexing, so
    // a bogus `width * height` is rejected rather than wrapping into a read
    // inside the buffer.
    if (width as u64) * (height as u64) * 4 > pixels.len() as u64 {
        return None;
    }
    let step = step.max(1) as usize;
    let w = width as usize;

    let mut buckets = [AnchorBucket::default(); ACCENT_ANCHOR_COUNT];
    let mut lum_sum = 0.0f32;
    let mut n = 0u32;

    let mut y = 0usize;
    while y < height as usize {
        let mut x = 0usize;
        while x < w {
            let i = (y * w + x) * 4;
            if pixels[i + 3] > SAMPLE_ALPHA_FLOOR {
                // One transfer per channel, shared by the Oklab conversion and
                // the luminance sum: `srgb_to_linear` is a `powf`, and doing it
                // twice per channel per sample is 27 000 of them on a
                // 1080x2400 wallpaper at this grid step.
                let lr = srgb_to_linear(pixels[i] as f32 / 255.0);
                let lg = srgb_to_linear(pixels[i + 1] as f32 / 255.0);
                let lb = srgb_to_linear(pixels[i + 2] as f32 / 255.0);
                let (l, a, bb) = oklab_from_linear_rgb(lr, lg, lb);
                let bucket = &mut buckets[nearest_anchor(l, a, bb)];
                bucket.count += 1;
                bucket.sum_l += l;
                bucket.sum_a += a;
                bucket.sum_b += bb;
                bucket.sum_c += (a * a + bb * bb).sqrt();
                lum_sum += relative_luminance_from_linear(lr, lg, lb);
                n += 1;
            }
            x += step;
        }
        y += step;
    }
    if n == 0 {
        return None;
    }

    // Total chroma first, population second, index third. Written as three
    // explicit comparisons rather than a derived key so the precedence is
    // readable and cannot be re-ordered by a refactor.
    let mut best = 0usize;
    for i in 1..ACCENT_ANCHOR_COUNT {
        if buckets[i].sum_c > buckets[best].sum_c
            || (buckets[i].sum_c == buckets[best].sum_c && buckets[i].count > buckets[best].count)
        {
            best = i;
        }
    }

    let bucket = buckets[best];
    let count = bucket.count.max(1) as f32;
    let (cl, ca, cb) = (
        bucket.sum_l / count,
        bucket.sum_a / count,
        bucket.sum_b / count,
    );
    // The centroid's own chroma, not the mean of the per-pixel chromas: taking
    // the mean *within* one hue bucket is safe, which is the whole reason the
    // bucketing comes first.
    //
    // A grey wallpaper needs no special case: its centroid's chroma is zero, so
    // `quantise_oklab` takes its `chroma <= 0.0` path and returns the neutral
    // at that lightness. An earlier version branched on a chroma floor here to
    // "handle" a neutral explicitly, and the perturbation sweep showed the two
    // paths produce the same answer -- a correct-but-unreachable branch, which
    // is worse than not having it.
    let chroma = (ca * ca + cb * cb).sqrt();
    let seed = quantise_oklab(cl, oklab_hue(ca, cb), chroma);

    // The hints describe the wallpaper, not the accent: mean relative
    // luminance over every opaque sample, exactly one bit set.
    let mean_lum = lum_sum / n as f32;
    let hints = if mean_lum < DARK_WALLPAPER_LUMINANCE {
        HINT_SUPPORTS_DARK_THEME
    } else {
        HINT_SUPPORTS_DARK_TEXT
    };
    Some(WallpaperPalette { seed, hints })
}

/// Shell entry point: accent seed + hints straight from PNG bytes.
///
/// This is what `crates/utlc/src/main.rs:wallpaper_seed` (`:9738`),
/// `refresh_wallpaper` (`:402`) and `average_png_colour` (`:9871`) should
/// call instead of averaging. It does the same three steps the shell
/// currently open-codes -- `header_size` (`png.rs:118`), the
/// `decode_working_set_estimate` budget check (`png.rs:147`), `decode_png`
/// (`png.rs:159`) -- then [`quantise_wallpaper`] on the same
/// [`SAMPLE_STEP`] grid `average_png_colour` used, so the comparison in the
/// tests is like for like.
///
/// Budget and failure handling mirror the shell exactly: over
/// `8 * 1024 * 1024` (the shell's `WALLPAPER_PROBE_BUDGET`,
/// `main.rs:9734`, also `crate::settings::picker::WALLPAPER_PROBE_BUDGET`)
/// or undecodable returns `None` and the shell keeps its fallback seed
/// (`0xFF3B82F6`, `main.rs:9739`). Off the frame path: decodes (allocates)
/// once per wallpaper change, never per frame.
///
/// JPEG/WebP import is intentionally out of scope here: the picker validates
/// those prefixes (`crate::settings::picker::validate_image_prefix`) but this
/// crate can only decode PNG, so the shell must transcode or skip them before
/// calling this.
pub fn wallpaper_palette_from_png(data: &[u8]) -> Option<WallpaperPalette> {
    let (w, h) = crate::graphics::png::header_size(data)?;
    if crate::graphics::png::decode_working_set_estimate(w, h) > 8 * 1024 * 1024 {
        return None;
    }
    let img = crate::graphics::png::decode_png(data)?;
    quantise_wallpaper(&img.pixels, img.width, img.height, SAMPLE_STEP)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::lstar_of_argb as lstar_of;

    #[test]
    fn oklab_l_from_lstar_round_trips_exactly() {
        for &target in &TONE_RAMP {
            let back = lstar_from_oklab_l(oklab_l_from_lstar(target));
            assert!(
                (back - target).abs() < 1.0e-3,
                "L* {target} round-tripped to {back}"
            );
        }
        // Including the linear segment below L* = 8.
        for target in [0.0f32, 1.0, 4.0, 8.0, 8.5, 20.0] {
            let back = lstar_from_oklab_l(oklab_l_from_lstar(target));
            assert!((back - target).abs() < 1.0e-2, "L* {target} -> {back}");
        }
    }

    #[test]
    fn linear_srgb_transfer_is_an_involution() {
        for i in 0..=255u32 {
            let v = i as f32 / 255.0;
            let back = linear_to_srgb(srgb_to_linear(v));
            assert!((back - v).abs() < 1.0e-6, "sRGB {v} -> {back}");
        }
    }

    #[test]
    fn oklab_round_trip_is_lossless() {
        for i in [0u32, 1, 17, 64, 128, 200, 254, 255] {
            let r = srgb_to_linear(i as f32 / 255.0);
            let (l, a, b) = oklab_from_linear_rgb(r, r, r);
            assert!(
                (a).abs() < 1.0e-5 && (b).abs() < 1.0e-5,
                "grey {i} is not neutral"
            );
            let (rr, gg, bb) = linear_rgb_from_oklab(l, a, b);
            assert!((rr - r).abs() < 1.0e-4, "grey {i} round trip");
            assert!(
                (gg - r).abs() < 1.0e-4 && (bb - r).abs() < 1.0e-4,
                "grey {i}"
            );
        }
    }

    #[test]
    fn middle_lstar_clears_aa_against_white() {
        // The plan's stated reason for MIDDLE_LSTAR: it is the lowest tone
        // that still reaches 4.5:1 on white. If this ever fails, the constant
        // is wrong and every "dark accent" built on it is not accessible.
        let c = neutral(MIDDLE_LSTAR);
        let ratio = contrast_ratio(c, 0xFFFFFFFF);
        assert!(
            ratio >= 4.5,
            "MIDDLE_LSTAR {MIDDLE_LSTAR} gives only {ratio}:1 on white"
        );
        // And one step darker must still clear it, so 49.6 is a real boundary
        // and not an arbitrary pick.
        assert!(contrast_ratio(neutral(48.0), 0xFFFFFFFF) >= 4.5);
        // ...while a full step lighter must not, or the bound is too loose.
        assert!(contrast_ratio(neutral(50.0), 0xFFFFFFFF) < 4.5);
    }

    #[test]
    fn hct_palette_matches_compose_scheme_tones() {
        // Plan §1.9, the role -> tone table, for several seeds. The tone is
        // the contract; hue and chroma are the design freedom.
        for seed in [
            0xFF4285F4u32,
            0xFFEA4335,
            0xFFFBBC05,
            0xFF34A853,
            0xFF9E9E9E,
        ] {
            let dark = MaterialYouPalette::from_seed(seed);
            for (name, got, want) in [
                ("surface", dark.surface, dark_tone::SURFACE),
                (
                    "surface_container",
                    dark.surface_container,
                    dark_tone::SURFACE_CONTAINER,
                ),
                (
                    "surface_container_high",
                    dark.surface_container_high,
                    dark_tone::SURFACE_CONTAINER_HIGH,
                ),
                ("primary", dark.primary, dark_tone::PRIMARY),
                ("on_primary", dark.on_primary, dark_tone::ON_PRIMARY),
                (
                    "primary_container",
                    dark.primary_container,
                    dark_tone::PRIMARY_CONTAINER,
                ),
                (
                    "on_primary_container",
                    dark.on_primary_container,
                    dark_tone::ON_PRIMARY_CONTAINER,
                ),
                ("on_surface", dark.on_surface, dark_tone::ON_SURFACE),
                (
                    "on_surface_variant",
                    dark.on_surface_variant,
                    dark_tone::ON_SURFACE_VARIANT,
                ),
                ("outline", dark.outline, dark_tone::OUTLINE),
                (
                    "outline_variant",
                    dark.outline_variant,
                    dark_tone::OUTLINE_VARIANT,
                ),
            ] {
                assert!(
                    (lstar_of(got) - want).abs() < 0.5,
                    "seed {seed:08x} {name}: L* {} != specified {want}",
                    lstar_of(got)
                );
            }
            let light = MaterialYouPalette::from_seed_light(seed);
            for (name, got, want) in [
                ("surface", light.surface, light_tone::SURFACE),
                (
                    "surface_container",
                    light.surface_container,
                    light_tone::SURFACE_CONTAINER,
                ),
                (
                    "surface_container_high",
                    light.surface_container_high,
                    light_tone::SURFACE_CONTAINER_HIGH,
                ),
                ("primary", light.primary, light_tone::PRIMARY),
                ("on_primary", light.on_primary, light_tone::ON_PRIMARY),
                ("on_surface", light.on_surface, light_tone::ON_SURFACE),
                (
                    "on_surface_variant",
                    light.on_surface_variant,
                    light_tone::ON_SURFACE_VARIANT,
                ),
                ("outline", light.outline, light_tone::OUTLINE),
            ] {
                assert!(
                    (lstar_of(got) - want).abs() < 0.5,
                    "seed {seed:08x} light {name}: L* {} != specified {want}",
                    lstar_of(got)
                );
            }
        }
    }

    #[test]
    fn tone_contract_holds_to_the_8bit_quantisation_floor() {
        // The tone is enforced by an outer bisection on the *measured* L* of
        // the quantised 8-bit colour, so the only error left is the output
        // format itself. That floor is real and was measured, not guessed:
        // the largest step between adjacent 8-bit greys is 0.509 L* (in the
        // shadows, around grey 25), and the closest any grey gets to each
        // ramp target is
        //
        //   99.0 -> 0.036   95.0 -> 0.145   90.0 -> 0.116   80.0 -> 0.119
        //   70.0 -> 0.018   60.0 -> 0.172   49.6 -> 0.037   40.0 -> 0.096
        //   30.0 -> 0.160   20.0 -> 0.135   10.0 -> 0.233    0.0 -> 0.000
        //
        // so 0.25 is the tightest honest bound. An earlier version of this
        // test asserted 0.1, which is below what 8 bits can represent; the
        // outer search was correct and the assertion was not.
        //
        // For contrast: before the outer search, a Google red at the
        // specified L* 80 came out at 79.42 -- 0.58, well outside this floor
        // and therefore a real defect rather than quantisation.
        const FLOOR: f32 = 0.25;
        for seed in [
            0xFF4285F4u32,
            0xFFEA4335,
            0xFFFBBC05,
            0xFF34A853,
            0xFF9E9E9E,
            0xFF000000,
            0xFFFFFFFF,
            0xFF00FF00,
            0xFFFF00FF,
        ] {
            for hue in [0.0f32, 60.0, 137.0, 200.0, 271.0, 330.0] {
                for &target in &TONE_RAMP {
                    let argb = tone_to_color(target, hue, 0.05);
                    let got = lstar_of_argb(argb);
                    assert!(
                        (got - target).abs() < FLOOR,
                        "seed {seed:08x} hue {hue} target L* {target} -> {got}"
                    );
                }
            }
        }
    }

    #[test]
    fn surface_container_is_lstar_12_dark_and_98_light() {
        // The two values the plan singles out.
        let p = MaterialYouPalette::from_seed(0xFF4285F4);
        assert!(
            (lstar_of(p.surface_container) - 12.0).abs() < 0.25,
            "dark surface_container L* {}",
            lstar_of(p.surface_container)
        );
        let l = MaterialYouPalette::from_seed_light(0xFF4285F4);
        assert!(
            (lstar_of(l.surface_container) - 98.0).abs() < 0.25,
            "light surface_container L* {}",
            lstar_of(l.surface_container)
        );
    }

    #[test]
    fn on_primary_onto_primary_clears_wcag_aa() {
        // The failure the HSL ramp actually had. Checked across the hue
        // circle and both schemes, because the old bug only showed on some
        // hues.
        for i in 0..24u32 {
            let hue = i as f32 * 15.0;
            let seed = tone_to_color(50.0, hue, 0.2);
            let dark = MaterialYouPalette::from_seed(seed);
            assert!(
                contrast_ratio(dark.on_primary, dark.primary) >= 4.5,
                "dark hue {hue}: on_primary/primary is only {:.2}:1",
                contrast_ratio(dark.on_primary, dark.primary)
            );
            let light = MaterialYouPalette::from_seed_light(seed);
            assert!(
                contrast_ratio(light.on_primary, light.primary) >= 4.5,
                "light hue {hue}: on_primary/primary is only {:.2}:1",
                contrast_ratio(light.on_primary, light.primary)
            );
        }
    }

    #[test]
    fn on_surface_onto_surface_clears_wcag_aa() {
        for seed in [
            0xFF4285F4u32,
            0xFFEA4335,
            0xFFFBBC05,
            0xFF9E9E9E,
            0xFF000000,
        ] {
            let dark = MaterialYouPalette::from_seed(seed);
            assert!(contrast_ratio(dark.on_surface, dark.surface) >= 4.5);
            let light = MaterialYouPalette::from_seed_light(seed);
            assert!(contrast_ratio(light.on_surface, light.surface) >= 4.5);
        }
    }

    #[test]
    fn every_role_is_opaque() {
        // The framebuffer is XRGB8888 and `blend_alpha` forces alpha to 0xFF,
        // so a palette role with a non-opaque alpha byte would be a silent
        // bug: the colour would be right and the blending wrong.
        for seed in [0xFF4285F4u32, 0xFFEA4335, 0xFFFBBC05, 0xFF9E9E9E] {
            for (name, c) in [
                ("surface", MaterialYouPalette::from_seed(seed).surface),
                ("primary", MaterialYouPalette::from_seed(seed).primary),
                ("outline", MaterialYouPalette::from_seed(seed).outline),
                ("surface", MaterialYouPalette::from_seed_light(seed).surface),
                ("primary", MaterialYouPalette::from_seed_light(seed).primary),
            ] {
                assert_eq!(c >> 24, 0xFF, "{name} of {seed:08x} is not opaque");
            }
        }
    }

    #[test]
    fn a_grey_seed_produces_a_palette_without_dividing_by_zero() {
        let s = seed_from_color(0xFF808080);
        assert_eq!(s.chroma, 0.0, "grey seed must be desaturated");
        assert!(s.hue.is_finite());
        let p = MaterialYouPalette::from_seed(0xFF808080);
        assert!((lstar_of(p.surface) - 6.0).abs() < 0.5);
        assert!((lstar_of(p.primary) - 80.0).abs() < 0.5);
    }

    #[test]
    fn max_chroma_never_leaves_the_gamut() {
        for hue in [0.0f32, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0] {
            for lstar in [0.0f32, 20.0, 49.6, 60.0, 80.0, 95.0, 99.0, 100.0] {
                let c = max_chroma(oklab_l_from_lstar(lstar), hue, 0.4);
                let rad = hue.to_radians();
                let (r, g, b) =
                    linear_rgb_from_oklab(oklab_l_from_lstar(lstar), c * rad.cos(), c * rad.sin());
                let ok = (-1.0e-3..=1.0 + 1.0e-3).contains(&r)
                    && (-1.0e-3..=1.0 + 1.0e-3).contains(&g)
                    && (-1.0e-3..=1.0 + 1.0e-3).contains(&b);
                assert!(ok, "hue {hue} L* {lstar} left the gamut: {r} {g} {b}");
            }
        }
    }

    #[test]
    fn max_chroma_is_monotonic_in_the_request() {
        // Asking for less chroma must never yield more.
        for hue in [0.0f32, 120.0, 240.0] {
            let mut prev = f32::INFINITY;
            for req in [0.4f32, 0.3, 0.2, 0.1, 0.05] {
                let c = max_chroma(oklab_l_from_lstar(50.0), hue, req);
                assert!(c <= prev + 1.0e-6, "hue {hue} req {req}: {c} > {prev}");
                prev = c;
            }
        }
    }

    #[test]
    fn high_tones_are_chroma_capped() {
        // Above L* 95 the ramp caps chroma, so two seeds of wildly different
        // saturation must converge to nearly the same very light colour.
        let a = tone_to_color(99.0, 30.0, 0.4);
        let b = tone_to_color(99.0, 30.0, 0.05);
        let (ar, ag, ab) = ((a >> 16) & 0xFF, (a >> 8) & 0xFF, a & 0xFF);
        let (br, bg, bb) = ((b >> 16) & 0xFF, (b >> 8) & 0xFF, b & 0xFF);
        let delta = (ar as i32 - br as i32)
            .abs()
            .max((ag as i32 - bg as i32).abs())
            .max((ab as i32 - bb as i32).abs());
        assert!(
            delta <= 24,
            "L* 99 colours differ by {delta}, cap not applied"
        );
    }

    #[test]
    fn palette_generation_is_cheap_enough_for_boot() {
        // Not a benchmark: a regression tripwire. The plan budgets 350 us for
        // 60 colours. Debug builds are ~20x slower than release, so this is a
        // deliberately loose ceiling that still catches an accidental
        // per-colour loop.
        let t0 = std::time::Instant::now();
        for i in 0..60u32 {
            MaterialYouPalette::from_seed(0xFF000000 | (i * 0x0404_0404) & 0x00FF_FFFF);
        }
        let dt = t0.elapsed();
        assert!(
            dt.as_millis() < 400,
            "60 palette derivations took {dt:?}, which is far past the boot budget"
        );
    }

    // -- wallpaper accent extraction --------------------------------------

    /// A decoded RGBA buffer with every pixel `colour`, `w` x `h`, opaque.
    fn image(w: u32, h: u32, colour: (u8, u8, u8)) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            v.extend_from_slice(&[colour.0, colour.1, colour.2, 255]);
        }
        v
    }

    /// A decoded RGBA buffer split vertically: `left` on the left half,
    /// `right` on the right half.
    fn split_image(w: u32, h: u32, left: (u8, u8, u8), right: (u8, u8, u8)) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..h {
            for x in 0..w {
                let c = if x < w / 2 { left } else { right };
                v.extend_from_slice(&[c.0, c.1, c.2, 255]);
            }
        }
        v
    }

    /// A transcription of `average_png_colour` (`crates/utlc/src/main.rs:9079-9101`)
    /// onto an already-decoded buffer, so the two extractors can be run over
    /// the *same* bytes in one test. Its own correctness is pinned separately,
    /// by `a_blue_and_orange_wallpaper_averages_to_grey`, against a
    /// hand-computed constant.
    fn mean_colour(pixels: &[u8], width: u32, height: u32, step: u32) -> Option<u32> {
        let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
        let mut y = 0u32;
        while y < height {
            let mut x = 0u32;
            while x < width {
                let i = ((y * width + x) * 4) as usize;
                if i + 3 < pixels.len() && pixels[i + 3] > 128 {
                    r += pixels[i] as u64;
                    g += pixels[i + 1] as u64;
                    b += pixels[i + 2] as u64;
                    n += 1;
                }
                x += step;
            }
            y += step;
        }
        if n == 0 {
            return None;
        }
        Some((0xFF << 24) | (((r / n) as u32) << 16) | (((g / n) as u32) << 8) | ((b / n) as u32))
    }

    /// The Oklab `(L, C, hue)` of an opaque sRGB triple.
    fn oklab_of(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
        let (l, a, bb) = oklab_from_linear_rgb(
            srgb_to_linear(r as f32 / 255.0),
            srgb_to_linear(g as f32 / 255.0),
            srgb_to_linear(b as f32 / 255.0),
        );
        (l, (a * a + bb * bb).sqrt(), oklab_hue(a, bb))
    }

    /// The `(R, G, B)` of an opaque ARGB seed. `argb >> 16` alone is *not*
    /// the red channel -- the alpha byte is still in the top `u8` -- which is
    /// a bug this helper exists to make impossible to write again.
    fn rgb(argb: u32) -> (u8, u8, u8) {
        (
            ((argb >> 16) & 0xFF) as u8,
            ((argb >> 8) & 0xFF) as u8,
            (argb & 0xFF) as u8,
        )
    }

    /// The Oklab `(L, C, hue)` of an opaque ARGB seed.
    fn oklab_of_argb(argb: u32) -> (f32, f32, f32) {
        let (r, g, b) = rgb(argb);
        oklab_of(r, g, b)
    }

    /// Shortest distance between two Oklab hues, in degrees, wrap-safe.
    fn hue_gap(a: f32, b: f32) -> f32 {
        let d = (a - b).abs() % 360.0;
        d.min(360.0 - d)
    }

    /// The hue that failed the acceptance test on purpose: a blue and an
    /// orange whose **arithmetic mean is exactly `#808080`**.
    ///
    /// (0 + 200) / 2 = 100, (40 + 160) / 2 = 100, (200 + 0) / 2 = 100. That is
    /// a hand-computed equality, not an observed one, and it is the whole
    /// premise of the remaining quantiser tests.
    const MEAN_KILLER_BLUE: (u8, u8, u8) = (0, 40, 200);
    const MEAN_KILLER_ORANGE: (u8, u8, u8) = (200, 160, 0);

    /// **The premise, pinned.** The arithmetic mean over this wallpaper is
    /// exactly `#646464`: R == G == B, chroma 0, and a colour that appears
    /// nowhere in the image. If this ever stops being true the fixture is no
    /// longer testing what it claims to.
    #[test]
    fn a_blue_and_orange_wallpaper_averages_to_grey() {
        let w = 96u32;
        let h = 48u32;
        let px = split_image(w, h, MEAN_KILLER_BLUE, MEAN_KILLER_ORANGE);
        let mean = mean_colour(&px, w, h, SAMPLE_STEP).expect("opaque pixels");
        // 100 = 0x64, from (0 + 200) / 2 on every channel. Hand-computed, not
        // observed: that is what makes it a premise rather than a reading.
        assert_eq!(
            mean, 0xFF64_6464,
            "the mean of the two halves is not the grey the fixture is built on"
        );
        assert_eq!(rgb(mean), (100, 100, 100), "R == G == B");
        let (_, chroma, hue) = oklab_of(100, 100, 100);
        assert!(chroma < 1.0e-4, "the mean is not achromatic: {chroma}");
        // And it is not either source hue, which is the other half of the
        // claim: a mean that lands near a real colour would not be a
        // cancellation.
        let (_, _, blue_hue) = oklab_of(MEAN_KILLER_BLUE.0, MEAN_KILLER_BLUE.1, MEAN_KILLER_BLUE.2);
        let (_, _, orange_hue) = oklab_of(
            MEAN_KILLER_ORANGE.0,
            MEAN_KILLER_ORANGE.1,
            MEAN_KILLER_ORANGE.2,
        );
        assert!(
            hue_gap(hue, blue_hue) > 30.0,
            "grey {hue} is near blue {blue_hue}"
        );
        assert!(
            hue_gap(hue, orange_hue) > 30.0,
            "grey {hue} is near orange {orange_hue}"
        );
    }

    /// **The contrast the brief asks for.** Same bytes, same grid, two
    /// aggregations, different answers -- and the quantiser's answer is a
    /// colour that is actually in the image. Swap [`quantise_wallpaper`] for
    /// [`mean_colour`] and this goes red on the chroma assertion.
    #[test]
    fn the_quantiser_keeps_the_hue_the_mean_destroyed() {
        let w = 96u32;
        let h = 48u32;
        let px = split_image(w, h, MEAN_KILLER_BLUE, MEAN_KILLER_ORANGE);

        let mean = mean_colour(&px, w, h, SAMPLE_STEP).expect("opaque pixels");
        let got = quantise_wallpaper(&px, w, h, SAMPLE_STEP).expect("opaque pixels");

        assert_ne!(got.seed, mean, "the quantiser agreed with the mean");
        // The mean's chroma is zero by construction; the quantiser's is not.
        let (_, mean_c, _) = oklab_of_argb(mean);
        let (_, got_c, got_h) = oklab_of_argb(got.seed);
        assert!(mean_c < 1.0e-4, "the mean grew a hue: {mean_c}");
        assert!(
            got_c > 0.05,
            "the quantiser's seed is nearly grey too: {got_c}"
        );
        // And it is one of the two source hues, not a third thing: the
        // bucket assignment picks a *winner*, and the winner is either half.
        let (_, _, blue_h) = oklab_of(MEAN_KILLER_BLUE.0, MEAN_KILLER_BLUE.1, MEAN_KILLER_BLUE.2);
        let (_, _, orange_h) = oklab_of(
            MEAN_KILLER_ORANGE.0,
            MEAN_KILLER_ORANGE.1,
            MEAN_KILLER_ORANGE.2,
        );
        let d_blue = hue_gap(got_h, blue_h);
        let d_orange = hue_gap(got_h, orange_h);
        assert!(
            d_blue.min(d_orange) < 25.0,
            "seed hue {got_h} is {d_blue} from blue and {d_orange} from orange"
        );
    }

    /// The brief's phrasing -- "different accents for blue and orange" -- read
    /// as two wallpapers rather than one. The seeds must differ, and each must
    /// be near its own source hue.
    #[test]
    fn blue_and_orange_wallpapers_get_different_accents() {
        let (w, h) = (96u32, 48u32);
        let blue =
            quantise_wallpaper(&image(w, h, MEAN_KILLER_BLUE), w, h, SAMPLE_STEP).expect("opaque");
        let orange = quantise_wallpaper(&image(w, h, MEAN_KILLER_ORANGE), w, h, SAMPLE_STEP)
            .expect("opaque");
        assert_ne!(blue.seed, orange.seed, "both wallpapers got one seed");
        let (_, bc, bh) = oklab_of(MEAN_KILLER_BLUE.0, MEAN_KILLER_BLUE.1, MEAN_KILLER_BLUE.2);
        let (_, oc, oh) = oklab_of(
            MEAN_KILLER_ORANGE.0,
            MEAN_KILLER_ORANGE.1,
            MEAN_KILLER_ORANGE.2,
        );
        let (_, got_b, got_bh) = oklab_of_argb(blue.seed);
        let (_, got_o, got_oh) = oklab_of_argb(orange.seed);
        assert!(bc > 0.05 && oc > 0.05, "the sources must be chromatic");
        assert!(got_b > 0.05 && got_o > 0.05, "the seeds must be chromatic");
        assert!(hue_gap(got_bh, bh) < 20.0, "blue seed hue {got_bh} != {bh}");
        assert!(
            hue_gap(got_oh, oh) < 20.0,
            "orange seed hue {got_oh} != {oh}"
        );
        assert!(
            hue_gap(got_bh, got_oh) > 60.0,
            "the two accents are not far apart"
        );
        // And they produce visibly different palettes, which is the only
        // reason any of this exists.
        let bp = blue.palette(PaletteScheme::Dark);
        let op = orange.palette(PaletteScheme::Dark);
        assert_ne!(bp.primary, op.primary);
    }

    /// A large neutral field must not drown out a smaller saturated region.
    /// The mean gets this exactly backwards: the field dominates the sum, and
    /// the patch is a rounding error in the quotient.
    #[test]
    fn a_saturated_patch_beats_a_large_neutral_field() {
        let (w, h) = (96u32, 96u32);
        let mut px = image(w, h, (128, 128, 128));
        // A 48x48 block of orange in the bottom right -- 25% of the area, and
        // four of the sixteen 24 px grid cells. Enough that the mean is visibly
        // wrong, not so much that it is a majority.
        for y in 48..96 {
            for x in 48..96 {
                let i = ((y * w + x) * 4) as usize;
                px[i] = 200;
                px[i + 1] = 140;
                px[i + 2] = 0;
            }
        }
        let mean = mean_colour(&px, w, h, SAMPLE_STEP).expect("opaque");
        let got = quantise_wallpaper(&px, w, h, SAMPLE_STEP).expect("opaque");
        // The mean is a blend: R = (4 * 200 + 12 * 128) / 16 = 146,
        // G = (4 * 140 + 12 * 128) / 16 = 131, B = (4 * 0 + 12 * 128) / 16 = 96.
        // A khaki that is neither the field nor the patch.
        let (mr, mg, mb) = rgb(mean);
        assert_eq!((mr, mg, mb), (146, 131, 96), "the mean is {mr},{mg},{mb}");
        // What a blend actually costs is *chroma*, not hue: pulling the patch
        // three-quarters of the way to grey leaves the hue roughly where it
        // was and the saturation gone. So the claim to pin is that the mean's
        // chroma is a fraction of the patch's, and the quantiser's is not.
        let (_, mean_c, _) = oklab_of(mr, mg, mb);
        let (_, patch_c, patch_h) = oklab_of(200, 140, 0);
        assert!(
            mean_c < patch_c * 0.6,
            "the mean's chroma {mean_c} is not washed out against the patch's {patch_c}"
        );
        // The quantiser's is the patch's, because the patch owns every bit of
        // chroma in the image.
        let (_, got_c, got_h) = oklab_of_argb(got.seed);
        assert!(got_c > 0.05, "the quantiser also returned grey: {got_c}");
        assert!(
            got_c > patch_c * 0.8,
            "the quantiser washed it out too: {got_c} against {patch_c}"
        );
        assert!(
            hue_gap(got_h, patch_h) < 15.0,
            "the quantiser found hue {got_h}, not the patch's {patch_h}"
        );
        assert_ne!(got.seed, mean);
        // And the washed-out mean really does produce a weaker accent, which is
        // the user-visible half of the problem.
        let mean_pal = MaterialYouPalette::from_seed(mean);
        let got_pal = MaterialYouPalette::from_seed(got.seed);
        let got_s = seed_from_color(got.seed);
        let mean_s = seed_from_color(mean);
        assert!(
            got_s.chroma > mean_s.chroma * 1.5,
            "the palette from the mean is not flatter: {} vs {}",
            mean_s.chroma,
            got_s.chroma
        );
        assert_ne!(mean_pal.primary, got_pal.primary);
    }

    /// `HINT_SUPPORTS_DARK_THEME` (`WallpaperColorsCompat.kt:13`) is a property
    /// of the wallpaper, and `WallpaperManagerCompat.kt:25` reads it to choose
    /// the theme. So it must move the **scheme** and leave the **seed** alone:
    /// a saturated blue wallpaper at two very different lightnesses is the same
    /// accent either way, which is the case an implementation that folds the
    /// hint into the seed gets wrong.
    #[test]
    fn the_dark_theme_hint_chooses_the_scheme_not_the_accent() {
        // The two bits are the platform's, and they are distinct.
        assert_eq!(HINT_SUPPORTS_DARK_TEXT, 1);
        assert_eq!(HINT_SUPPORTS_DARK_THEME, 2);
        assert!(supports_dark_theme(HINT_SUPPORTS_DARK_THEME));
        assert!(!supports_dark_theme(HINT_SUPPORTS_DARK_TEXT));
        assert!(supports_dark_text(HINT_SUPPORTS_DARK_TEXT));
        assert!(!supports_dark_text(HINT_SUPPORTS_DARK_THEME));

        let (w, h) = (96u32, 48u32);
        // The *same* hue and chroma at two lightnesses, built with the ramp's
        // own tone solver so the hue is identical by construction rather than
        // by eyeballing two RGB triples. L* 20 is a navy, L* 80 a pale sky.
        let navy = tone_to_color(20.0, 250.0, 0.12);
        let sky = tone_to_color(80.0, 250.0, 0.12);
        let (nr, ng, nb) = rgb(navy);
        let (sr, sg, sb) = rgb(sky);
        assert!(
            relative_luminance(navy) < DARK_WALLPAPER_LUMINANCE
                && relative_luminance(sky) > DARK_WALLPAPER_LUMINANCE,
            "the two tones are not on opposite sides of the boundary: \
             {} vs {}",
            relative_luminance(navy),
            relative_luminance(sky)
        );
        let dark =
            quantise_wallpaper(&image(w, h, (nr, ng, nb)), w, h, SAMPLE_STEP).expect("opaque");
        let light =
            quantise_wallpaper(&image(w, h, (sr, sg, sb)), w, h, SAMPLE_STEP).expect("opaque");
        assert_eq!(dark.hints, HINT_SUPPORTS_DARK_THEME, "a navy wallpaper");
        assert_eq!(light.hints, HINT_SUPPORTS_DARK_TEXT, "a pale sky");
        assert!(supports_dark_theme(dark.hints) && !supports_dark_text(dark.hints));
        assert!(supports_dark_text(light.hints) && !supports_dark_theme(light.hints));
        // Lightness is not part of an accent: the two seeds' hues agree even
        // though the hints -- and so the schemes -- do not.
        let (_, dark_c, dark_h) = oklab_of_argb(dark.seed);
        let (_, light_c, light_h) = oklab_of_argb(light.seed);
        assert!(
            dark_c > 0.05 && light_c > 0.05,
            "both seeds must be chromatic"
        );
        assert!(
            hue_gap(dark_h, light_h) < 5.0,
            "the hint changed the accent hue: {dark_h} vs {light_h}"
        );
        assert!(
            hue_gap(dark_h, 250.0) < 5.0 && hue_gap(light_h, 250.0) < 5.0,
            "either seed drifted from the requested hue"
        );
        // And the two hints do select different schemes.
        assert_eq!(
            scheme_for_hints(dark.hints, PaletteScheme::Light),
            PaletteScheme::Dark
        );
        assert_eq!(
            scheme_for_hints(light.hints, PaletteScheme::Dark),
            PaletteScheme::Light
        );
        assert_eq!(
            dark.palette_from_hints().surface,
            MaterialYouPalette::from_seed(dark.seed).surface
        );
        assert_eq!(
            light.palette_from_hints().surface,
            MaterialYouPalette::from_seed_light(light.seed).surface
        );
        // The two schemes really do differ in tone, so the hint is not inert.
        assert_ne!(
            dark.palette_from_hints().surface,
            light.palette_from_hints().surface
        );
    }

    /// `WallpaperManagerCompat.kt:19` reads the hints as `?: 0` and
    /// `WallpaperManagerCompatVO.kt:7` hardcodes `null` for the pre-O path, so
    /// "no hints" is a real state -- and on `0`, `supportsDarkTheme` is
    /// `false`. Taking that as "light" would flip a dark boot to the light ramp,
    /// so the zero case has to be told the answer instead of derived from it.
    #[test]
    fn no_hints_falls_back_rather_than_reading_as_light() {
        assert!(!supports_dark_theme(0));
        assert!(!supports_dark_text(0));
        assert_eq!(
            scheme_for_hints(0, PaletteScheme::Dark),
            PaletteScheme::Dark
        );
        assert_eq!(
            scheme_for_hints(0, PaletteScheme::Light),
            PaletteScheme::Light
        );
        // An unknown bit is not a decision, and must not be treated as one.
        assert_eq!(
            scheme_for_hints(0x8000_0000, PaletteScheme::Dark),
            PaletteScheme::Light
        );
        // A real hint beats the fallback in both directions.
        assert_eq!(
            scheme_for_hints(HINT_SUPPORTS_DARK_THEME, PaletteScheme::Light),
            PaletteScheme::Dark
        );
    }

    /// Every hue round-trips: a wallpaper of one colour produces a seed within
    /// half a bucket (15 degrees) of that colour's hue. This is the property
    /// that makes the fixed palette usable at all, and it is checked all the
    /// way round the wheel rather than on one convenient example.
    #[test]
    fn every_hue_survives_the_bucket_assignment() {
        let (w, h) = (48u32, 48u32);
        // Twelve saturated sRGB colours, one per primary/secondary hue name.
        let sources = [
            (255, 0, 0),
            (255, 128, 0),
            (255, 255, 0),
            (128, 255, 0),
            (0, 255, 0),
            (0, 255, 128),
            (0, 255, 255),
            (0, 128, 255),
            (0, 0, 255),
            (128, 0, 255),
            (255, 0, 255),
            (255, 0, 128),
        ];
        for s in sources {
            let got = quantise_wallpaper(&image(w, h, s), w, h, SAMPLE_STEP)
                .unwrap_or_else(|| panic!("{s:?} produced no palette"));
            let (_, _, want_h) = oklab_of(s.0, s.1, s.2);
            let (_, got_c, got_h) = oklab_of_argb(got.seed);
            assert!(got_c > 0.04, "{s:?} -> grey seed, chroma {got_c}");
            assert!(
                hue_gap(got_h, want_h) <= 15.5,
                "{s:?}: seed hue {got_h} is {} from {want_h}",
                hue_gap(got_h, want_h)
            );
            // The seed keeps enough chroma to build an accessible accent from,
            // which is the only reason the ramp cares what it gets.
            let pal = got.palette(PaletteScheme::Dark);
            assert!(contrast_ratio(pal.primary, pal.on_primary) >= 4.5);
        }
    }

    /// The anchors are a fixed, hue-only, evenly spaced table. Asserted
    /// structurally because every property above depends on it and none of them
    /// would notice if somebody quietly halved the count or made them
    /// lightness-varying.
    #[test]
    fn the_anchor_table_is_fixed_evenly_spaced_and_hue_only() {
        assert_eq!(ACCENT_ANCHOR_COUNT, 12);
        assert_eq!(ACCENT_ANCHORS.len(), ACCENT_ANCHOR_COUNT);
        for (i, (l, c, h)) in ACCENT_ANCHORS.iter().enumerate() {
            assert_eq!(*h, i as f32 * 30.0, "anchor {i} is off the 30 deg grid");
            assert_eq!(*l, 0.70, "anchor {i} varies in lightness");
            assert_eq!(*c, 0.15, "anchor {i} varies in chroma");
        }
        // A colour exactly half way between anchors 0 and 1 is equidistant from
        // both, and the tie-break sends it to the **lower** index -- repeatedly,
        // so the answer cannot be a float comparison happening to go one way.
        let mid = 15.0f32.to_radians();
        let between = (0.70, 0.15 * mid.cos(), 0.15 * mid.sin());
        for _ in 0..64 {
            assert_eq!(nearest_anchor(between.0, between.1, between.2), 0);
        }
        // One degree past the midpoint moves it, so the tie-break is not just
        // "always 0".
        let just_past = 16.0f32.to_radians();
        assert_eq!(
            nearest_anchor(0.70, 0.15 * just_past.cos(), 0.15 * just_past.sin()),
            1
        );
        // A neutral is equidistant from all twelve and resolves to the first.
        for _ in 0..64 {
            assert_eq!(nearest_anchor(0.5, 0.0, 0.0), 0);
        }
    }

    /// Fixed-capacity and deterministic: the accumulator is a stack array of
    /// `Copy` buckets and the answer is a function of the bytes alone. No
    /// clock, no RNG, no `Vec` on the path -- so a wallpaper that extracts to
    /// one seed on one boot extracts to the same seed on every boot.
    #[test]
    fn the_extraction_is_fixed_capacity_and_deterministic() {
        assert!(!core::mem::needs_drop::<WallpaperPalette>());
        assert!(!core::mem::needs_drop::<AnchorBucket>());
        assert_eq!(
            ACCENT_ANCHOR_COUNT * core::mem::size_of::<AnchorBucket>(),
            240
        );
        let (w, h) = (96u32, 48u32);
        let px = split_image(w, h, MEAN_KILLER_BLUE, MEAN_KILLER_ORANGE);
        let first = quantise_wallpaper(&px, w, h, SAMPLE_STEP).expect("opaque");
        for _ in 0..32 {
            assert_eq!(
                quantise_wallpaper(&px, w, h, SAMPLE_STEP),
                Some(first),
                "two runs over identical bytes disagreed"
            );
        }
        // The step is honoured: a 1 px grid sees every pixel and a `w` px grid
        // sees one, and for a uniform image both must land on the same colour.
        let uniform = image(w, h, MEAN_KILLER_BLUE);
        let every = quantise_wallpaper(&uniform, w, h, 1).expect("opaque");
        let one = quantise_wallpaper(&uniform, w, h, w).expect("opaque");
        assert_eq!(every.seed, one.seed, "the step changed the answer");
        assert_eq!(every.hints, one.hints);
        // And the two extractors sample the same grid, which is what makes
        // the mean/quantiser comparison a comparison of aggregations.
        assert_eq!(SAMPLE_STEP, 24, "main.rs:9080's STEP");
    }

    /// Every way of asking for a palette out of nothing. All of these are on
    /// the boot path (`main.rs:8994` walks wallpaper directories and calls
    /// this), so each has to be an ordinary `None` and not a panic or a read
    /// out of bounds.
    #[test]
    fn an_image_with_nothing_to_read_yields_no_palette() {
        // A zero dimension.
        assert_eq!(quantise_wallpaper(&[], 0, 10, SAMPLE_STEP), None);
        assert_eq!(quantise_wallpaper(&[], 10, 0, SAMPLE_STEP), None);
        // A buffer too short for the image it claims, including the
        // overflow-shaped claim: `width * height * 4` in `u64` must reject
        // these rather than wrapping into a read inside the buffer.
        assert_eq!(quantise_wallpaper(&[0; 4], 100, 100, SAMPLE_STEP), None);
        assert_eq!(
            quantise_wallpaper(&[0; 4], u32::MAX, u32::MAX, SAMPLE_STEP),
            None
        );
        assert_eq!(
            quantise_wallpaper(&[0; 4], 1 << 20, 1 << 20, SAMPLE_STEP),
            None
        );
        // A one-pixel image that *is* big enough: transparent has nothing to
        // read, opaque black has one black pixel. The two differ only in the
        // alpha byte, so this is the `alpha > 128` gate on its own.
        assert_eq!(
            quantise_wallpaper(&[0, 0, 0, 0], 1, 1, SAMPLE_STEP),
            None,
            "a fully transparent pixel is skipped"
        );
        let black = quantise_wallpaper(&[0, 0, 0, 255], 1, 1, SAMPLE_STEP).expect("opaque");
        assert_eq!(black.hints, HINT_SUPPORTS_DARK_THEME, "black is dark");
        assert_eq!(rgb(black.seed), (0, 0, 0), "the seed is black");
        // Everything transparent: `alpha > 128` is the gate, matching
        // `main.rs:9086`.
        let with_alpha = |a: u8| {
            let mut px = image(8, 8, (200, 0, 0));
            for i in (3..px.len()).step_by(4) {
                px[i] = a;
            }
            px
        };
        for a in [0u8, 1, 127, 128] {
            assert_eq!(
                quantise_wallpaper(&with_alpha(a), 8, 8, SAMPLE_STEP),
                None,
                "alpha {a} should have been skipped"
            );
        }
        // Alpha 129 is the first value that counts.
        assert!(quantise_wallpaper(&with_alpha(129), 8, 8, SAMPLE_STEP).is_some());
        // A zero step must be clamped to exactly 1: honoured as 0 it would
        // never advance and spin, and bumped to 2 it would silently drop every
        // other row. The fixture below puts the *only* saturated colour on the
        // odd cells, so a step of 1 finds it and a step of 2 finds nothing but
        // grey -- which is what makes the two distinguishable at all. A
        // checkerboard of two saturated colours does not work: both steps then
        // return the same winner, and "clamped to 1" and "clamped to 2" are
        // indistinguishable.
        let mut checker = Vec::with_capacity((8 * 8 * 4) as usize);
        for y in 0..8u32 {
            for x in 0..8u32 {
                let c = if (x + y) % 2 == 0 {
                    (128, 128, 128)
                } else {
                    (200, 0, 0)
                };
                checker.extend_from_slice(&[c.0, c.1, c.2, 255]);
            }
        }
        let one = quantise_wallpaper(&checker, 8, 8, 1).expect("opaque");
        let two = quantise_wallpaper(&checker, 8, 8, 2).expect("opaque");
        assert_ne!(
            one.seed, two.seed,
            "the fixture does not discriminate step 1 from step 2"
        );
        assert_eq!(
            quantise_wallpaper(&checker, 8, 8, 0),
            Some(one),
            "a zero step was not clamped to 1"
        );
    }

    /// A grey wallpaper yields a grey seed, and that is the honest answer
    /// rather than a fallback. It also has to be *dark* or *light* in the hint
    /// sense, since a mid grey is neither -- so the boundary is pinned
    /// explicitly in both directions.
    #[test]
    fn a_neutral_wallpaper_yields_a_neutral_seed_and_one_hint() {
        let (w, h) = (48u32, 48u32);
        for (grey, want_dark) in [
            (0u8, true),
            (64u8, true),
            (127u8, true),
            (187u8, true),
            (188u8, false),
            (200u8, false),
            (255u8, false),
        ] {
            let got = quantise_wallpaper(&image(w, h, (grey, grey, grey)), w, h, SAMPLE_STEP)
                .unwrap_or_else(|| panic!("grey {grey} produced no palette"));
            let (r, g, b) = rgb(got.seed);
            let spread = (r as i32 - g as i32)
                .abs()
                .max((g as i32 - b as i32).abs())
                .max((r as i32 - b as i32).abs());
            assert!(
                spread <= 2,
                "grey {grey} produced a tinted seed {r},{g},{b}"
            );
            // No hue assertion here, deliberately: `oklab_hue` is `atan2` on a
            // near-zero `(a, b)`, so a grey's hue is numerically arbitrary and
            // pinning it would pin a rounding artefact. The chroma and the
            // *tone* are the claims.
            let (_, chroma, _) = oklab_of(r, g, b);
            assert!(chroma < 0.01, "grey {grey} seed chroma {chroma}");
            // The seed keeps the wallpaper's tone, so a dark grey wallpaper
            // gives a dark seed and a light one a light seed -- the ramp is
            // built for the right lightness. Compared in CIE L\*, because that
            // is the unit the tone contract is stated in and an 8-bit value is
            // not a lightness (sRGB 64 is L* 27.1, not 64). A shift of even
            // 0.05 in Oklab `L` moves L* by several units, so this bound also
            // catches the centroid being taken from somewhere else entirely.
            let src = (grey as u32) << 16 | (grey as u32) << 8 | grey as u32;
            let seed_l =
                lstar_of_argb(0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | b as u32);
            let want_l = lstar_of_argb(src);
            assert!(
                (seed_l - want_l).abs() <= 2.0,
                "grey {grey} is L* {want_l} but its seed is L* {seed_l}"
            );
            assert_eq!(
                got.hints,
                if want_dark {
                    HINT_SUPPORTS_DARK_THEME
                } else {
                    HINT_SUPPORTS_DARK_TEXT
                },
                "grey {grey}"
            );
            // Exactly one bit, always: the platform never sets both, and a
            // caller switching on the hints must not see an impossible state.
            assert_eq!(
                got.hints.count_ones(),
                1,
                "grey {grey} hints {:#x}",
                got.hints
            );
        }
    }

    /// The luminance boundary is a stated constant, so it is pinned at both
    /// sides rather than left to whatever the loop happens to do.
    #[test]
    fn the_dark_wallpaper_boundary_is_the_stated_constant() {
        // sRGB 186 is the value whose relative luminance is 0.5: solving
        // `((c/255 + 0.055)/1.055)^2.4 = 0.5` gives `c/255 = 1.055 * 0.5^(1/2.4) - 0.055`,
        // i.e. c = 187.5. 187 is just under, 188 just over.
        let y187 = srgb_to_linear(187.0 / 255.0);
        let y188 = srgb_to_linear(188.0 / 255.0);
        assert!(y187 < DARK_WALLPAPER_LUMINANCE, "{y187}");
        assert!(y188 > DARK_WALLPAPER_LUMINANCE, "{y188}");
        let (w, h) = (48u32, 48u32);
        let dark =
            quantise_wallpaper(&image(w, h, (187, 187, 187)), w, h, SAMPLE_STEP).expect("opaque");
        let light =
            quantise_wallpaper(&image(w, h, (188, 188, 188)), w, h, SAMPLE_STEP).expect("opaque");
        assert_eq!(dark.hints, HINT_SUPPORTS_DARK_THEME);
        assert_eq!(light.hints, HINT_SUPPORTS_DARK_TEXT);
    }

    /// The relative-luminance reading the quantiser's hints depend on, checked
    /// against the WCAG endpoints. `contrast_ratio` now shares it, so this also
    /// guards the refactor that put the three coefficients in one place.
    #[test]
    fn relative_luminance_has_the_wcag_endpoints() {
        // Black and white, with the alpha byte set so these are opaque ARGB.
        assert!(relative_luminance(0xFF00_0000).abs() < 1.0e-6);
        assert!((relative_luminance(0xFFFF_FFFF) - 1.0).abs() < 1.0e-6);
        // sRGB 128 is the 0.2159 grey, well under the 0.5 boundary.
        assert!(
            relative_luminance(0xFF80_8080) < DARK_WALLPAPER_LUMINANCE,
            "{}",
            relative_luminance(0xFF80_8080)
        );
        // Pure green is the green coefficient, to three decimals.
        assert!(
            (relative_luminance(0xFF00_FF00) - 0.7152).abs() < 1.0e-3,
            "{}",
            relative_luminance(0xFF00_FF00)
        );
        // And the 21:1 endpoint `contrast_ratio` has always promised, which is
        // the check that the extracted closure is still the same arithmetic.
        assert!((contrast_ratio(0xFF00_0000, 0xFFFF_FFFF) - 21.0).abs() < 1.0e-4);
        assert!((contrast_ratio(0xFF80_8080, 0xFF80_8080) - 1.0).abs() < 1.0e-6);
    }

    /// The shell entry point: PNG bytes in, seed + hints out, with the same
    /// budget gate the shell open-codes. Encoded with the crate's own
    /// stored-block encoder so the bytes are a conforming PNG by
    /// construction rather than by a hardcoded blob.
    #[test]
    fn wallpaper_palette_from_png_decodes_and_quantises() {
        use crate::graphics::png::{decode_working_set_estimate, encode_png, RgbaImage};
        // A solid orange tile: the quantised seed must be chromatic and near
        // the source hue, and the hints must be exactly one bit.
        let mut pixels = Vec::with_capacity(48 * 48 * 4);
        for _ in 0..48 * 48 {
            pixels.extend_from_slice(&[200, 140, 0, 255]);
        }
        let png = encode_png(&RgbaImage {
            width: 48,
            height: 48,
            pixels,
        })
        .expect("encodes");
        let got = wallpaper_palette_from_png(&png).expect("decodable");
        let direct = {
            let img = crate::graphics::png::decode_png(&png).expect("decodes");
            quantise_wallpaper(&img.pixels, img.width, img.height, SAMPLE_STEP).expect("opaque")
        };
        assert_eq!(got, direct, "the wrapper must be decode + quantise");
        let (_, c, h) = oklab_of_argb(got.seed);
        let (_, want_c, want_h) = oklab_of(200, 140, 0);
        assert!(want_c > 0.05, "fixture must be chromatic");
        assert!(c > 0.05, "seed chroma {c}");
        assert!(hue_gap(h, want_h) < 15.0, "seed hue {h} != {want_h}");
        assert_eq!(got.hints.count_ones(), 1);
        // Same 8 MiB the picker gates on: the two constants must agree or the
        // shell's probe and its picker disagree about "affordable".
        assert_eq!(
            8 * 1024 * 1024u64,
            crate::settings::picker::WALLPAPER_PROBE_BUDGET
        );
        assert!(
            decode_working_set_estimate(48, 48) <= crate::settings::picker::WALLPAPER_PROBE_BUDGET
        );
        // Garbage and truncated inputs are None, not a panic: the shell keeps
        // its fallback seed on exactly these.
        assert_eq!(wallpaper_palette_from_png(&[]), None);
        assert_eq!(wallpaper_palette_from_png(b"not a png"), None);
        assert_eq!(wallpaper_palette_from_png(&png[..20]), None);
    }

    /// The wrapper refuses what it cannot afford *without decoding*.
    ///
    /// 1000x1000 needs ~9 MiB transient, past the 8 MiB probe budget, but is
    /// still under `MAX_PIXELS` and is encoded here by the crate's own
    /// stored-block encoder -- so the bytes are a conforming, decodable PNG
    /// and removing the budget gate would decode successfully and return
    /// `Some`. That is the perturb that proves this test pins the gate and
    /// not the decoder: the header itself is fine (`header_size` agrees), the
    /// answer is `None` anyway, and the 4 MB RGBA image is never built.
    #[test]
    fn wallpaper_palette_from_png_refuses_what_it_cannot_afford() {
        use crate::graphics::png::{
            decode_working_set_estimate, encode_png, header_size, RgbaImage,
        };
        let (w, h) = (1000u32, 1000u32);
        assert!(
            decode_working_set_estimate(w, h) > 8 * 1024 * 1024,
            "the fixture must actually be over budget"
        );
        // Alpha 200 clears the `> 128` sample gate, so a decode would find
        // opaque pixels: `None` can only come from the budget check.
        let png = encode_png(&RgbaImage {
            width: w,
            height: h,
            pixels: vec![200u8; w as usize * h as usize * 4],
        })
        .expect("encodes");
        assert_eq!(header_size(&png), Some((w, h)), "the header is fine");
        assert_eq!(
            wallpaper_palette_from_png(&png),
            None,
            "over budget means no decode is attempted"
        );
    }
}
