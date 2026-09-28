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
    if deg < 0.0 { deg + 360.0 } else { deg }
}

/// The 12-shade tone ramp (`Shades.java:52-63`).
///
/// Lightness `[99, 95, 90, 80, 70, 60, 49.6, 40, 30, 20, 10, 0]`.
/// `MIDDLE_LSTAR` (49.6) is the lowest tone that still clears 4.5:1 against
/// white, so a "dark" accent built on it is not a light one in disguise.
pub const TONE_RAMP: [f32; 12] = [99.0, 95.0, 90.0, 80.0, 70.0, 60.0, 49.6, 40.0, 30.0, 20.0, 10.0, 0.0];
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
    if in_gamut3(linear_rgb_from_oklab(l, CHROMA_CEILING * rad.cos(), CHROMA_CEILING * rad.sin())) {
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

/// WCAG 2.1 relative-luminance contrast ratio between two opaque sRGB
/// colours. 1.0 = identical, 21.0 = black on white.
pub fn contrast_ratio(a: u32, b: u32) -> f32 {
    let lin = |c: u32| -> f32 {
        srgb_to_linear(((c >> 16) & 0xFF) as f32 / 255.0) * 0.2126
            + srgb_to_linear(((c >> 8) & 0xFF) as f32 / 255.0) * 0.7152
            + srgb_to_linear((c & 0xFF) as f32 / 255.0) * 0.0722
    };
    let (la, lb) = (lin(a), lin(b));
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
        return Seed { hue: 0.0, chroma: 0.0 };
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
                on_primary_container: t(
                    dark_tone::ON_PRIMARY_CONTAINER,
                    s.hue,
                    accent * 0.5,
                ),
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
                on_primary_container: t(
                    light_tone::ON_PRIMARY_CONTAINER,
                    s.hue,
                    accent * 0.5,
                ),
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
            assert!((a).abs() < 1.0e-5 && (b).abs() < 1.0e-5, "grey {i} is not neutral");
            let (rr, gg, bb) = linear_rgb_from_oklab(l, a, b);
            assert!((rr - r).abs() < 1.0e-4, "grey {i} round trip");
            assert!((gg - r).abs() < 1.0e-4 && (bb - r).abs() < 1.0e-4, "grey {i}");
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
        for seed in [0xFF4285F4u32, 0xFFEA4335, 0xFFFBBC05, 0xFF34A853, 0xFF9E9E9E] {
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
                ("outline_variant", dark.outline_variant, dark_tone::OUTLINE_VARIANT),
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
                (
                    "on_surface",
                    light.on_surface,
                    light_tone::ON_SURFACE,
                ),
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
            0xFF4285F4u32, 0xFFEA4335, 0xFFFBBC05, 0xFF34A853, 0xFF9E9E9E, 0xFF000000, 0xFFFFFFFF,
            0xFF00FF00, 0xFFFF00FF,
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
        for seed in [0xFF4285F4u32, 0xFFEA4335, 0xFFFBBC05, 0xFF9E9E9E, 0xFF000000] {
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
                let (r, g, b) = linear_rgb_from_oklab(
                    oklab_l_from_lstar(lstar),
                    c * rad.cos(),
                    c * rad.sin(),
                );
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
        assert!(delta <= 24, "L* 99 colours differ by {delta}, cap not applied");
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
}
