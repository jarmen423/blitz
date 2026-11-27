//! known-future (wave 1a): a gradient's colour stops, prepared as Blink prepares them before Skia interpolates.
//!
//! A CSS gradient that interpolates in a polar space (`in hsl`, `in hwb`, `in lch`, `in oklch`) is drawn by Skia,
//! here as in Chrome: Skia is handed sRGB stops and a `SkGradient::Interpolation`, converts the stops into the
//! interpolation space itself, fixes the hues up for the hue method and interpolates. The Skia of this build
//! (m157) and Chrome 152's have the same code for that (`SkGradientBaseShader.cpp`, read at both revisions). What
//! differed, wherever a picture of ours differed from Chrome's, is what Skia is *given*:
//!
//! * Blink does not hand Skia the colour the author wrote. `Gradient::FillSkiaStops` (Chrome 152,
//!   `platform/graphics/gradient.cc`) first converts every stop into the interpolation space with
//!   `Color::ConvertToColorSpaceForInterpolation` — its own arithmetic, `ui/gfx/color_conversions.cc`, with its
//!   own constants — and hands Skia that colour converted *back* to sRGB.
//! * A component that is missing after that conversion (`none` in the source, a missing component carried
//!   forward to its analogous one, or a hue Blink finds powerless: chroma <= 0.02 in lch and oklch, saturation
//!   0 in hsl, whiteness + blackness >= 1 in hwb) takes the neighbouring stop's value: the stop is split in two
//!   at its position, one colour resolved against the stop before it and one against the stop after it.
//!
//! Two things follow that no rule of CSS Color 4 gives, and both are in Chrome's pictures:
//!
//! * Two hues exactly 180 degrees apart. Which way round `shorter` and `longer` go is decided in Skia by
//!   `h2 - h1 > 180` on the hues it reads back from the sRGB floats, so by the last bit of Blink's
//!   `HSLToSRGB`. `hsl(20 90% 50%)` to `hsl(200 90% 50%)` reads back as just under 180 (shorter: up through
//!   110, longer: down through 290); `hsl(350 80% 60%)` to `hsl(170 80% 60%)` as just over (shorter: up
//!   through 80, where the specification's tie rule goes down through 260). The hsl and hwb arithmetic here is
//!   Blink's operation for operation in `f32`, so the same bit decides.
//! * A grey stop in `oklch` or `lch`. Blink's XYZ-D50 detour does not map a grey onto the lightness axis
//!   exactly (white has an lch chroma of 0.0188: the comment on `kAchromaticChromaThreshold`), so the stop it
//!   resolves — its small chroma kept, the neighbour's hue put in — is, once back in sRGB and read by Skia's
//!   exact conversion, that vector minus Blink's own offset: a hue that is neither the neighbour's nor the
//!   grey's. Chrome 152 draws `linear-gradient(in oklch, oklch(0.7 0.2 30), white)` turning from hue 30 to
//!   about 110. Black is exact in both, and is carried forward cleanly. That offset is thousands of times the
//!   rounding of an `f32`, so it is reproduced by using Blink's constants, not by matching bits.
//!
//! The stops of every other gradient (a rectangular interpolation space: srgb, oklab, ...) are not touched.
//!
//! Limits, said here once: a stop written in `color(display-p3 ...)`, a98-rgb, prophoto-rgb, rec2020 or an xyz
//! space reaches XYZ-D50 by Stylo's matrices, not Blink's (no hue tie or grey of those spaces was measured); and a
//! `hwb()` colour is prepared from Stylo's own (h, w, b), as Blink keeps it.

use style::color::{AbsoluteColor, ColorFlags, ColorSpace as StyloColorSpace};

type M3 = [[f32; 3]; 3];
type V3 = [f32; 3];

// ------------------------------------------------------------------ skcms (Skia's modules/skcms/skcms.cc) and
// Chromium's helpers on it (skia/ext/skcms_ext.cc)

/// `mv_mul`, and Chromium's `skcms::Matrix3x3_apply`
fn mv(m: &M3, v: V3) -> V3 {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// `skcms_Matrix3x3_concat`
fn concat(a: &M3, b: &M3) -> M3 {
    let mut m = [[0.0f32; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            m[r][c] = a[r][0] * b[0][c] + a[r][1] * b[1][c] + a[r][2] * b[2][c];
        }
    }
    m
}

/// `skcms_Matrix3x3_invert`: worked in `f64`, stored as `f32`
fn invert(src: &M3) -> M3 {
    let (a00, a01, a02) = (src[0][0] as f64, src[1][0] as f64, src[2][0] as f64);
    let (a10, a11, a12) = (src[0][1] as f64, src[1][1] as f64, src[2][1] as f64);
    let (a20, a21, a22) = (src[0][2] as f64, src[1][2] as f64, src[2][2] as f64);
    let (mut b0, mut b1, mut b2) = (a00 * a11 - a01 * a10, a00 * a12 - a02 * a10, a01 * a12 - a02 * a11);
    let (mut b3, mut b4, mut b5) = (a20, a21, a22);
    let determinant = b0 * b5 - b1 * b4 + b2 * b3;
    let invdet = 1.0 / determinant;
    b0 *= invdet;
    b1 *= invdet;
    b2 *= invdet;
    b3 *= invdet;
    b4 *= invdet;
    b5 *= invdet;
    let mut dst = [[0.0f32; 3]; 3];
    dst[0][0] = (a11 * b5 - a12 * b4) as f32;
    dst[1][0] = (a02 * b4 - a01 * b5) as f32;
    dst[2][0] = b2 as f32;
    dst[0][1] = (a12 * b3 - a10 * b5) as f32;
    dst[1][1] = (a00 * b5 - a02 * b3) as f32;
    dst[2][1] = (-b1) as f32;
    dst[0][2] = (a10 * b4 - a11 * b3) as f32;
    dst[1][2] = (a01 * b3 - a00 * b4) as f32;
    dst[2][2] = b0 as f32;
    dst
}

/// `skcms_TransferFunction`, the sRGB-ish kind: `y = (a x + b)^g + e` above `d`, `c x + f` up to it
#[derive(Clone, Copy, Debug)]
struct TransferFn {
    g: f32,
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

/// `SkNamedTransferFn::kSRGB`
fn srgb_tf() -> TransferFn {
    TransferFn { g: 2.4, a: (1.0f64 / 1.055) as f32, b: (0.055f64 / 1.055) as f32, c: (1.0f64 / 12.92) as f32, d: 0.04045, e: 0.0, f: 0.0 }
}

/// Chromium's `skcms::TransferFunction_eval` (skia/ext/skcms_ext.cc): the C library's `powf`, not skcms's own
/// approximation of it (whose results come in steps of about 1e-5, the size of what is at stake for a grey stop)
fn tf_eval(tf: &TransferFn, x: f32) -> f32 {
    let sign = if x < 0.0 { -1.0f32 } else { 1.0 };
    let abs_x = x.abs();
    if abs_x <= tf.d {
        return sign * (tf.c * abs_x + tf.f);
    }
    let base = (tf.a * abs_x + tf.b).max(0.0);
    if tf.g == 1.0 {
        return sign * (base + tf.e);
    }
    sign * (base.powf(tf.g) + tf.e)
}

/// Chromium's `skcms::TransferFunction_invert` (the same file)
fn tf_invert(src: &TransferFn) -> TransferFn {
    let mut inv = TransferFn { g: 0.0, a: 0.0, b: 0.0, c: 0.0, d: 0.0, e: 0.0, f: 0.0 };
    inv.d = src.c * src.d + src.f;
    if inv.d > 0.0 {
        inv.c = 1.0 / src.c;
        inv.f = -src.f / src.c;
    }
    let k = src.a.powf(-src.g);
    inv.g = 1.0 / src.g;
    inv.a = k;
    inv.b = -k * src.e;
    inv.e = -src.b / src.a;
    if inv.a * inv.d + inv.b < 0.0 {
        inv.b = -inv.a * inv.d;
    }
    let s = tf_eval(src, 1.0);
    let sign = if s < 0.0 { -1.0f32 } else { 1.0 };
    let s = s * sign;
    if s < inv.d {
        inv.f = 1.0 - sign * inv.c * s;
    } else {
        inv.e = 1.0 - sign * (inv.a * s + inv.b).powf(inv.g);
    }
    inv
}

/// `skcms_AdaptToXYZD50`: the Bradford matrix from a white point (x, y) to D50
fn adapt_to_xyzd50(wx: f32, wy: f32) -> M3 {
    let w_xyz = [wx / wy, 1.0, (1.0 - wx - wy) / wy];
    let w_xyz_d50 = [0.96422f32, 1.0, 0.82521];
    let xyz_to_lms: M3 = [[0.8951, 0.2664, -0.1614], [-0.7502, 1.7135, 0.0367], [0.0389, -0.0685, 1.0296]];
    let lms_to_xyz: M3 = [[0.9869929, -0.1470543, 0.1599627], [0.4323053, 0.5183603, 0.0492912], [-0.0085287, 0.0400428, 0.9684867]];
    let src_cone = mv(&xyz_to_lms, w_xyz);
    let dst_cone = mv(&xyz_to_lms, w_xyz_d50);
    let scale: M3 = [[dst_cone[0] / src_cone[0], 0.0, 0.0], [0.0, dst_cone[1] / src_cone[1], 0.0], [0.0, 0.0, dst_cone[2] / src_cone[2]]];
    concat(&lms_to_xyz, &concat(&scale, &xyz_to_lms))
}

// ------------------------------------------------------------------ Chromium's ui/gfx/color_conversions.cc

/// `SkNamedGamut::kSRGB`: sRGB to XYZ-D50, the ICC profile's 16.16 fixed-point numbers
const SRGB_TO_XYZD50: M3 = [
    [0x6FA2 as f32 / 65536.0, 0x6299 as f32 / 65536.0, 0x24A0 as f32 / 65536.0],
    [0x38F5 as f32 / 65536.0, 0xB785 as f32 / 65536.0, 0x0F84 as f32 / 65536.0],
    [0x0390 as f32 / 65536.0, 0x18DA as f32 / 65536.0, 0xB6CF as f32 / 65536.0],
];
const XYZD65_TO_LMS: M3 = [
    [0.819_022_443_216_431_9, 0.361_906_256_280_122_1, -0.128_873_782_612_164_14],
    [0.032_983_667_198_027_1, 0.929_286_846_896_554_6, 0.036_144_668_169_998_44],
    [0.048_177_199_566_046_255, 0.264_239_524_944_227_64, 0.633_547_825_813_693_7],
];
const OKLAB_TO_LMS: M3 = [
    [0.999_999_998_450_519_8, 0.396_337_792_173_767_86, 0.215_803_758_060_758_8],
    [1.000_000_008_881_760_8, -0.105_561_342_323_656_35, -0.063_854_174_771_705_9],
    [1.000_000_054_672_410_9, -0.089_484_182_094_965_76, -1.291_485_537_864_091_7],
];
const D50_X: f32 = 0.9642;
const D50_Y: f32 = 1.0;
const D50_Z: f32 = 0.8251;

fn d65_to_d50() -> M3 {
    adapt_to_xyzd50(0.3127, 0.3290)
}

fn pow_ext(x: f32, p: f32) -> f32 {
    if x < 0.0 { -(-x).powf(p) } else { x.powf(p) }
}

fn deg_to_rad(deg: f32) -> f32 {
    deg * std::f32::consts::PI / 180.0
}

fn rad_to_deg(rad: f32) -> f32 {
    rad * 180.0 / std::f32::consts::PI
}

fn lab_to_xyzd50(lab: V3) -> V3 {
    let y = (lab[0] + 16.0) / 116.0;
    let x = y + lab[1] / 500.0;
    let z = y - lab[2] / 200.0;
    let inverse = |t: f32| {
        let delta = 24.0f32 / 116.0;
        if t <= delta { (108.0f32 / 841.0) * (t - (16.0f32 / 116.0)) } else { t * t * t }
    };
    [inverse(x) * D50_X, inverse(y) * D50_Y, inverse(z) * D50_Z]
}

fn xyzd50_to_lab(xyz: V3) -> V3 {
    let transfer = |t: f32| {
        let delta_limit = (24.0f32 / 116.0) * (24.0f32 / 116.0) * (24.0f32 / 116.0);
        if t <= delta_limit { (841.0f32 / 108.0) * t + (16.0f32 / 116.0) } else { t.powf(1.0f32 / 3.0) }
    };
    let x = transfer(xyz[0] / D50_X);
    let y = transfer(xyz[1] / D50_Y);
    let z = transfer(xyz[2] / D50_Z);
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

fn oklab_to_xyzd50(lab: V3) -> V3 {
    let lms = mv(&OKLAB_TO_LMS, lab);
    let cubed = [lms[0] * lms[0] * lms[0], lms[1] * lms[1] * lms[1], lms[2] * lms[2] * lms[2]];
    let xyz_d65 = mv(&invert(&XYZD65_TO_LMS), cubed);
    mv(&d65_to_d50(), xyz_d65)
}

fn xyzd50_to_oklab(xyz: V3) -> V3 {
    let xyz_d65 = mv(&invert(&d65_to_d50()), xyz);
    let cubed = mv(&XYZD65_TO_LMS, xyz_d65);
    let third = 1.0f32 / 3.0;
    let lms = [pow_ext(cubed[0], third), pow_ext(cubed[1], third), pow_ext(cubed[2], third)];
    mv(&invert(&OKLAB_TO_LMS), lms)
}

fn lch_to_lab(lch: V3) -> V3 {
    [lch[0], lch[1] * deg_to_rad(lch[2]).cos(), lch[1] * deg_to_rad(lch[2]).sin()]
}

fn lab_to_lch(lab: V3) -> V3 {
    [lab[0], (lab[1] * lab[1] + lab[2] * lab[2]).sqrt(), rad_to_deg(lab[2].atan2(lab[1]))]
}

fn xyzd50_to_srgb(xyz: V3) -> V3 {
    let c = mv(&invert(&SRGB_TO_XYZD50), xyz);
    let inv = tf_invert(&srgb_tf());
    [tf_eval(&inv, c[0]), tf_eval(&inv, c[1]), tf_eval(&inv, c[2])]
}

fn srgb_to_xyzd50(rgb: V3) -> V3 {
    let tf = srgb_tf();
    mv(&SRGB_TO_XYZD50, [tf_eval(&tf, rgb[0]), tf_eval(&tf, rgb[1]), tf_eval(&tf, rgb[2])])
}

/// `gfx::HSLToSRGB` (s and l in 0..1)
fn hsl_to_srgb(hsl: V3) -> V3 {
    let (h, s, l) = (hsl[0], hsl[1], hsl[2]);
    if s == 0.0 {
        return [l, l, l];
    }
    let f = |n: f32| -> f32 {
        // (C++: `float k = fmod(n + h / 30.0f, 12.0)`, a double remainder narrowed to float)
        let k = (((n + h / 30.0) as f64) % 12.0) as f32;
        let a = s * l.min(1.0 - l);
        l - a * (-1.0f32).max((k - 3.0).min(9.0 - k).min(1.0))
    };
    [f(0.0), f(8.0), f(4.0)]
}

/// the hue of `gfx::SRGBToHSL` and `gfx::SRGBToHue`, before the wrap at 360
fn srgb_hue(r: f32, g: f32, b: f32, min: f32, max: f32) -> f32 {
    let d = max - min;
    let mut hue = 0.0f32;
    if d != 0.0 {
        if max == r {
            hue = (g - b) / d + if g < b { 6.0 } else { 0.0 };
        } else if max == g {
            hue = (b - r) / d + 2.0;
        } else {
            hue = (r - g) / d + 4.0;
        }
        hue *= 60.0;
    }
    hue
}

/// `gfx::SRGBToHSL`
fn srgb_to_hsl(rgb: V3) -> V3 {
    let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
    let (min, max) = (r.min(g).min(b), r.max(g).max(b));
    let lightness = (min + max) / 2.0;
    let d = max - min;
    let mut saturation = 0.0f32;
    let mut hue = srgb_hue(r, g, b, min, max);
    if d != 0.0 {
        saturation = if lightness == 0.0 || lightness == 1.0 { 0.0 } else { (max - lightness) / lightness.min(1.0 - lightness) };
    }
    if saturation < 0.0 {
        hue += 180.0;
        saturation = saturation.abs();
    }
    if hue >= 360.0 {
        hue -= 360.0;
    }
    [hue, saturation, lightness]
}

/// `gfx::HWBToSRGB` (w and b in 0..1)
fn hwb_to_srgb(hwb: V3) -> V3 {
    let (h, w, b) = (hwb[0], hwb[1], hwb[2]);
    if w + b >= 1.0 {
        let gray = w / (w + b);
        return [gray, gray, gray];
    }
    let mut rgb = hsl_to_srgb([h, 1.0, 0.5]);
    for c in &mut rgb {
        *c += w - (w + b) * *c;
    }
    rgb
}

/// `gfx::SRGBToHWB`
fn srgb_to_hwb(rgb: V3) -> V3 {
    let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
    let (min, max) = (r.min(g).min(b), r.max(g).max(b));
    let mut hue = srgb_hue(r, g, b, min, max);
    if hue >= 360.0 {
        hue -= 360.0;
    }
    [hue, min, 1.0 - max]
}

/// Blink's `AngleToUnitCircleDegrees`
fn angle_to_unit_circle(angle: f32) -> f32 {
    ((angle % 360.0) + 360.0) % 360.0
}

// ------------------------------------------------------------------ blink::Color, what a gradient needs of it

/// Blink's `kAchromaticChromaThreshold`: at or under this chroma a colour converted to lch or oklch has no hue
const ACHROMATIC_CHROMA_THRESHOLD: f32 = 0.02;

/// The colour spaces of `blink::Color` a stop is held in here. `XyzD50` stands for every space this file has no
/// arithmetic of Blink's for (module documentation): such a colour is taken to XYZ-D50 by Stylo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Space {
    SrgbLegacy,
    Srgb,
    Hsl,
    Hwb,
    Lab,
    Lch,
    Oklab,
    Oklch,
    XyzD50,
}

impl Space {
    fn is_rgb_or_xyz(self) -> bool {
        matches!(self, Space::SrgbLegacy | Space::Srgb | Space::XyzD50)
    }
    fn is_lab(self) -> bool {
        matches!(self, Space::Lab | Space::Oklab)
    }
    fn is_lch(self) -> bool {
        matches!(self, Space::Lch | Space::Oklch)
    }
}

/// The polar space of a gradient's `<color-interpolation-method>`, or `None` for a rectangular one
pub(crate) fn polar_space(space: StyloColorSpace) -> Option<Space> {
    match space {
        StyloColorSpace::Hsl => Some(Space::Hsl),
        StyloColorSpace::Hwb => Some(Space::Hwb),
        StyloColorSpace::Lch => Some(Space::Lch),
        StyloColorSpace::Oklch => Some(Space::Oklch),
        _ => None,
    }
}

/// `blink::Color`: three parameters in a colour space, each of which (and alpha) may be missing
#[derive(Clone, Copy, Debug)]
pub(crate) struct Color {
    space: Space,
    p: V3,
    none: [bool; 3],
    alpha: f32,
    alpha_none: bool,
}

/// A percentage Stylo holds as 0..100 in `f32`, as Blink's parser has it: a `double` divided by 100, narrowed
/// (the shortest decimal that gives back Stylo's `f32` is what the author wrote, where it has few digits)
fn percent_to_unit(v: f32) -> f32 {
    let written: f64 = format!("{v}").parse().unwrap_or(v as f64);
    (written / 100.0) as f32
}

impl Color {
    /// `Color::FromColorSpace`: lightness and chroma are clamped as there
    fn from_space(space: Space, p: [Option<f32>; 3], alpha: Option<f32>) -> Self {
        let mut c = Color {
            space,
            p: [p[0].unwrap_or(0.0), p[1].unwrap_or(0.0), p[2].unwrap_or(0.0)],
            none: [p[0].is_none(), p[1].is_none(), p[2].is_none()],
            alpha: alpha.map_or(0.0, |a| a.clamp(0.0, 1.0)),
            alpha_none: alpha.is_none(),
        };
        if matches!(space, Space::Lab | Space::Lch) && !c.p[0].is_nan() {
            c.p[0] = c.p[0].max(0.0).min(100.0);
        } else if matches!(space, Space::Oklab | Space::Oklch) && !c.p[0].is_nan() {
            c.p[0] = c.p[0].max(0.0).min(1.0);
        }
        if space.is_lch() {
            c.p[1] = c.p[1].max(0.0);
        }
        c
    }

    /// The colour Blink holds for a computed colour of Stylo's
    pub(crate) fn from_stylo(color: &AbsoluteColor) -> Self {
        let comp = [color.components.0, color.components.1, color.components.2];
        let none = [
            color.flags.contains(ColorFlags::C0_IS_NONE),
            color.flags.contains(ColorFlags::C1_IS_NONE),
            color.flags.contains(ColorFlags::C2_IS_NONE),
        ];
        let opt = |v: [f32; 3]| [(!none[0]).then_some(v[0]), (!none[1]).then_some(v[1]), (!none[2]).then_some(v[2])];
        let alpha = (!color.flags.contains(ColorFlags::ALPHA_IS_NONE)).then_some(color.alpha);
        match color.color_space {
            StyloColorSpace::Srgb if color.flags.contains(ColorFlags::IS_LEGACY_SRGB) => {
                // rgb(), a hex colour, a named colour: Blink holds 0..255 (kSRGBLegacy), whole numbers but for
                // a fractional rgb() channel
                let to255 = |c: f32| {
                    let v = c * 255.0;
                    if (v - v.round()).abs() < 1e-3 { v.round() } else { v }
                };
                Self::from_space(Space::SrgbLegacy, opt([to255(comp[0]), to255(comp[1]), to255(comp[2])]), alpha)
            }
            StyloColorSpace::Srgb => Self::from_space(Space::Srgb, opt(comp), alpha),
            StyloColorSpace::Hsl => Self::from_space(Space::Hsl, opt([comp[0], percent_to_unit(comp[1]), percent_to_unit(comp[2])]), alpha),
            StyloColorSpace::Hwb => Self::from_space(Space::Hwb, opt([comp[0], percent_to_unit(comp[1]), percent_to_unit(comp[2])]), alpha),
            StyloColorSpace::Lab => Self::from_space(Space::Lab, opt(comp), alpha),
            StyloColorSpace::Lch => Self::from_space(Space::Lch, opt(comp), alpha),
            StyloColorSpace::Oklab => Self::from_space(Space::Oklab, opt(comp), alpha),
            StyloColorSpace::Oklch => Self::from_space(Space::Oklch, opt(comp), alpha),
            _ => {
                let xyz = color.to_color_space(StyloColorSpace::XyzD50);
                Self::from_space(Space::XyzD50, [Some(xyz.components.0), Some(xyz.components.1), Some(xyz.components.2)], alpha)
            }
        }
    }

    fn has_none(&self) -> bool {
        self.none[0] || self.none[1] || self.none[2] || self.alpha_none
    }

    /// `Color::ToSRGB(gamut_map = false)`
    fn to_srgb(&self) -> V3 {
        match self.space {
            Space::Srgb => self.p,
            // (C++: `r / 255.0`, a double division narrowed to float)
            Space::SrgbLegacy => [(self.p[0] as f64 / 255.0) as f32, (self.p[1] as f64 / 255.0) as f32, (self.p[2] as f64 / 255.0) as f32],
            Space::Hsl => hsl_to_srgb(self.p),
            Space::Hwb => hwb_to_srgb(self.p),
            _ => xyzd50_to_srgb(self.to_xyzd50()),
        }
    }

    /// `Color::ToXYZD50(gamut_map = false)`
    fn to_xyzd50(&self) -> V3 {
        match self.space {
            Space::SrgbLegacy | Space::Srgb | Space::Hsl | Space::Hwb => srgb_to_xyzd50(self.to_srgb()),
            Space::Lab => lab_to_xyzd50(self.p),
            Space::Oklab => oklab_to_xyzd50(self.p),
            Space::Lch => lab_to_xyzd50(lch_to_lab(self.p)),
            Space::Oklch => oklab_to_xyzd50(lch_to_lab(self.p)),
            Space::XyzD50 => self.p,
        }
    }

    /// `Color::GetAnalogousMissingComponents`: which components of the colour in `dst` are missing because the
    /// analogous component of this colour is
    fn analogous_missing(&self, dst: Space) -> [bool; 3] {
        let n = self.none;
        match self.space {
            Space::SrgbLegacy | Space::Srgb | Space::XyzD50 if dst.is_rgb_or_xyz() => n,
            Space::Lab | Space::Oklab if dst.is_lab() => n,
            Space::Lab | Space::Oklab if dst.is_lch() => [n[0], false, false],
            Space::Lab | Space::Oklab if dst == Space::Hsl => [false, false, n[0]],
            Space::Lch | Space::Oklch if dst.is_lch() => n,
            Space::Lch | Space::Oklch if dst == Space::Hsl => [n[2], n[1], n[0]],
            Space::Lch | Space::Oklch if dst.is_lab() => [n[0], false, false],
            Space::Hsl if dst.is_lch() => [n[2], n[1], n[0]],
            Space::Hsl if dst.is_lab() => [n[2], false, false],
            Space::Hsl if dst == Space::Hwb => [n[0], false, false],
            Space::Hwb if dst == Space::Hsl => [n[0], false, false],
            Space::Hwb if dst.is_lch() => [false, false, n[0]],
            _ => [false; 3],
        }
    }

    /// `Color::ConvertToColorSpace`, for the four polar spaces a gradient interpolates in. A hue that is
    /// powerless in `dst` becomes missing; its parameter keeps the number the arithmetic gave, as in Blink.
    fn convert(&mut self, dst: Space) {
        if self.space == dst {
            return;
        }
        // (`ResolveMissingComponents`: a missing component is zero for a conversion)
        for i in 0..3 {
            if self.none[i] {
                self.p[i] = 0.0;
                self.none[i] = false;
            }
        }
        match dst {
            Space::Lch => {
                self.p = if self.space == Space::Lab { lab_to_lch(self.p) } else { lab_to_lch(xyzd50_to_lab(self.to_xyzd50())) };
                self.p[2] = angle_to_unit_circle(self.p[2]);
                if self.p[1] <= ACHROMATIC_CHROMA_THRESHOLD {
                    self.none[2] = true;
                }
            }
            Space::Oklch => {
                if self.space == Space::Oklab {
                    self.p = lab_to_lch(self.p);
                } else {
                    self.p = lab_to_lch(xyzd50_to_oklab(self.to_xyzd50()));
                    self.p[2] = angle_to_unit_circle(self.p[2]);
                }
                if self.p[1] <= ACHROMATIC_CHROMA_THRESHOLD {
                    self.none[2] = true;
                }
            }
            Space::Hsl => {
                self.p = srgb_to_hsl(self.to_srgb());
                if self.p[1] == 0.0 {
                    self.none[0] = true;
                }
            }
            Space::Hwb => {
                self.p = srgb_to_hwb(self.to_srgb());
                if self.p[1] + self.p[2] >= 1.0 {
                    self.none[0] = true;
                }
            }
            _ => unreachable!("a gradient's stops are prepared for a polar interpolation space only"),
        }
        self.space = dst;
    }

    /// `Color::ConvertToColorSpaceForInterpolation`
    fn convert_for_interpolation(&mut self, dst: Space) {
        if self.space == dst {
            return;
        }
        let analogous = self.analogous_missing(dst);
        self.convert(dst);
        for i in 0..3 {
            if analogous[i] {
                self.p[i] = 0.0;
                self.none[i] = true;
            }
        }
    }

    /// `Color::ToGradientStopSkColor4f`: sRGB, not mapped into its gamut, and alpha
    fn to_stop(&self) -> [f32; 4] {
        let [r, g, b] = self.to_srgb();
        [r, g, b, self.alpha]
    }
}

/// gradient.cc's `ResolveStopColorWithMissingParams`: `color`, in the interpolation space, with each missing
/// component taken from `neighbor` (converted for interpolation too)
fn resolve_with_neighbor(color: &Color, neighbor: &Color, space: Space) -> [f32; 4] {
    let mut converted = *neighbor;
    converted.convert_for_interpolation(space);
    let pick = |i: usize| Some(if color.none[i] { converted.p[i] } else { color.p[i] });
    let alpha = Some(if color.alpha_none { converted.alpha } else { color.alpha });
    Color::from_space(space, [pick(0), pick(1), pick(2)], alpha).to_stop()
}

/// What Skia is given for one stop of the gradient: one colour, or two at the stop's position when a missing
/// component took a different value from each neighbour. Straight (not premultiplied) sRGB and alpha.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PreparedStop {
    pub(crate) first: [f32; 4],
    pub(crate) second: Option<[f32; 4]>,
}

/// The stops of a gradient that interpolates in `space`, as `Gradient::FillSkiaStops` gives them to Skia, and
/// the colour Blink pads the start of the gradient with when its first stop is not at 0 (the first stop as it
/// was written, missing components as zeros: not the resolved one).
pub(crate) fn prepare_stops(colors: &[AbsoluteColor], space: Space) -> (Vec<PreparedStop>, [f32; 4]) {
    let stops: Vec<Color> = colors.iter().map(Color::from_stylo).collect();
    let front = stops.first().map_or([0.0; 4], Color::to_stop);
    let n = stops.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut color = stops[i];
        color.convert_for_interpolation(space);
        if !color.has_none() {
            out.push(PreparedStop { first: color.to_stop(), second: None });
        } else if n == 1 {
            let dummy = Color::from_space(space, [None, None, None], Some(1.0));
            out.push(PreparedStop { first: resolve_with_neighbor(&color, &dummy, space), second: None });
        } else {
            let left = (i != 0).then(|| resolve_with_neighbor(&color, &stops[i - 1], space));
            let right = (i != n - 1).then(|| resolve_with_neighbor(&color, &stops[i + 1], space));
            out.push(match (left, right) {
                (Some(l), r) => PreparedStop { first: l, second: r },
                (None, Some(r)) => PreparedStop { first: r, second: None },
                (None, None) => unreachable!(),
            });
        }
    }
    (out, front)
}

#[cfg(test)]
mod tests {
    use super::*;

    // the colours of the tests, as Stylo computes them from the CSS in the comments
    fn hsl(h: f32, s: f32, l: f32) -> AbsoluteColor {
        let mut c = AbsoluteColor::new(StyloColorSpace::Hsl, h, s, l, 1.0f32);
        c.flags.insert(ColorFlags::IS_LEGACY_SRGB);
        c
    }
    fn hsl_none(s: f32, l: f32) -> AbsoluteColor {
        AbsoluteColor::new(StyloColorSpace::Hsl, None::<f32>, s, l, 1.0f32)
    }
    fn hwb(h: Option<f32>, w: f32, b: f32) -> AbsoluteColor {
        let mut c = AbsoluteColor::new(StyloColorSpace::Hwb, h, w, b, 1.0f32);
        c.flags.insert(ColorFlags::IS_LEGACY_SRGB);
        c
    }
    fn rgb(r: u8, g: u8, b: u8) -> AbsoluteColor {
        AbsoluteColor::srgb_legacy(r, g, b, 1.0)
    }
    fn oklch(l: f32, c: f32, h: Option<f32>) -> AbsoluteColor {
        AbsoluteColor::new(StyloColorSpace::Oklch, l, c, h, 1.0f32)
    }
    const WHITE: fn() -> AbsoluteColor = || rgb(255, 255, 255);
    const BLACK: fn() -> AbsoluteColor = || rgb(0, 0, 0);

    fn prepared(space: Space, stops: &[AbsoluteColor]) -> Vec<PreparedStop> {
        prepare_stops(stops, space).0
    }

    /// Skia's `srgb_to_hsl` hue (SkGradientBaseShader.cpp), which is what decides a 180-degree tie
    fn skia_hsl_hue(c: [f32; 4]) -> f32 {
        let (r, g, b) = (c[0], c[1], c[2]);
        let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
        let d = mx - mn;
        if d == 0.0 {
            return 0.0;
        }
        let hue = if mx == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if mx == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        hue * 60.0
    }

    /// Skia's oklab (`lin_srgb_to_oklab`, on the exact sRGB transfer function): (hue in degrees, chroma)
    fn skia_oklch(c: [f32; 4]) -> (f32, f32) {
        let lin = |v: f64| {
            // (odd about zero: a colour outside the sRGB gamut has a negative channel)
            let a = v.abs();
            v.signum() * if a <= 0.04045 { a / 12.92 } else { ((a + 0.055) / 1.055).powf(2.4) }
        };
        let (r, g, b) = (lin(c[0] as f64), lin(c[1] as f64), lin(c[2] as f64));
        let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
        let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
        let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
        let a = 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s;
        let b2 = 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s;
        (((b2.atan2(a).to_degrees() + 360.0) % 360.0) as f32, (a * a + b2 * b2).sqrt() as f32)
    }

    #[test]
    fn skcms_arithmetic() {
        // the inverse of a matrix is its inverse
        for m in [SRGB_TO_XYZD50, XYZD65_TO_LMS, OKLAB_TO_LMS, d65_to_d50()] {
            let id = concat(&invert(&m), &m);
            for r in 0..3 {
                for c in 0..3 {
                    assert!((id[r][c] - if r == c { 1.0 } else { 0.0 }).abs() < 2e-6, "{id:?}");
                }
            }
        }
        // the transfer function and its inverse keep 0 and 1 (the inverse is tuned for 1)
        let (tf, inv) = (srgb_tf(), tf_invert(&srgb_tf()));
        assert_eq!(tf_eval(&inv, tf_eval(&tf, 1.0)), 1.0);
        assert_eq!(tf_eval(&inv, tf_eval(&tf, 0.0)), 0.0);
        for v in [0.02f32, 0.2, 0.5, 0.8] {
            assert!((tf_eval(&inv, tf_eval(&tf, v)) - v).abs() < 1e-5);
        }
    }

    /// The constants are Blink's: white is not on the lightness axis after its XYZ-D50 detour. Chromium's own
    /// comment on kAchromaticChromaThreshold gives white's lch chroma as 0.0188.
    #[test]
    fn white_has_blinks_chroma() {
        let mut white = Color::from_stylo(&WHITE());
        assert_eq!(white.space, Space::SrgbLegacy);
        assert_eq!(white.p, [255.0, 255.0, 255.0]);
        white.convert(Space::Lch);
        assert!((white.p[1] - 0.0188).abs() < 0.0005, "white's lch chroma {}", white.p[1]);
        assert!(white.none[2], "white's hue is powerless in lch");
        let mut white = Color::from_stylo(&WHITE());
        white.convert(Space::Oklch);
        assert!(white.p[1] > 1e-6 && white.p[1] < 1e-3, "white's oklch chroma {}", white.p[1]);
        assert!(white.none[2]);
        assert!((white.p[0] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn hsl_arithmetic_is_blinks() {
        // exact where it must be
        assert_eq!(hsl_to_srgb([0.0, 1.0, 0.5]), [1.0, 0.0, 0.0]);
        assert_eq!(hsl_to_srgb([120.0, 1.0, 0.5]), [0.0, 1.0, 0.0]);
        assert_eq!(hsl_to_srgb([240.0, 1.0, 0.5]), [0.0, 0.0, 1.0]);
        assert_eq!(hsl_to_srgb([77.0, 0.0, 0.25]), [0.25, 0.25, 0.25]);
        assert_eq!(srgb_to_hsl([1.0, 0.0, 0.0]), [0.0, 1.0, 0.5]);
        assert_eq!(srgb_to_hsl([0.5, 0.5, 0.5]), [0.0, 0.0, 0.5]);
        assert_eq!(hwb_to_srgb([40.0, 0.6, 0.6]), [0.5, 0.5, 0.5]);
        // and a round trip elsewhere
        for hsl in [[20.0f32, 0.9, 0.5], [200.0, 0.9, 0.5], [350.0, 0.8, 0.6], [170.0, 0.8, 0.6], [301.5, 0.33, 0.71]] {
            let back = srgb_to_hsl(hsl_to_srgb(hsl));
            assert!((back[0] - hsl[0]).abs() < 1e-3 && (back[1] - hsl[1]).abs() < 1e-5 && (back[2] - hsl[2]).abs() < 1e-5, "{hsl:?} -> {back:?}");
        }
        let back = srgb_to_hwb(hwb_to_srgb([200.0, 0.1, 0.2]));
        assert!((back[0] - 200.0).abs() < 1e-3 && (back[1] - 0.1).abs() < 1e-6 && (back[2] - 0.2).abs() < 1e-6);
        // an hsl() colour is Blink's (h, s, l), not Stylo's sRGB: hsl(20 90% 50%), hsl(200deg, 33.3%, 71%)
        let c = Color::from_stylo(&hsl(20.0, 90.0, 50.0));
        assert_eq!((c.space, c.p), (Space::Hsl, [20.0, 0.9, 0.5]));
        let c = Color::from_stylo(&hsl(200.0, 33.3, 71.0));
        assert_eq!((c.space, c.p), (Space::Hsl, [200.0, 0.333, 0.71]));
        // hwb(40 10% 20%)
        let c = Color::from_stylo(&hwb(Some(40.0), 10.0, 20.0));
        assert_eq!((c.space, c.p), (Space::Hwb, [40.0, 0.1, 0.2]));
        // #336699
        let c = Color::from_stylo(&rgb(0x33, 0x66, 0x99));
        assert_eq!((c.space, c.p), (Space::SrgbLegacy, [51.0, 102.0, 153.0]));
        assert_eq!(c.to_srgb(), [0.2, 0.4, 0.6]);
    }

    /// Two hsl hues exactly 180 degrees apart: Skia compares the hues it reads back from the sRGB floats with
    /// 180, and these are the readings Chrome 152's pictures show (the way round each gradient goes).
    #[test]
    fn hsl_antipodes_read_back_as_in_chrome() {
        let d = |a: AbsoluteColor, b: AbsoluteColor| {
            let s = prepared(Space::Hsl, &[a, b]);
            assert!(s[0].second.is_none() && s[1].second.is_none());
            skia_hsl_hue(s[1].first) - skia_hsl_hue(s[0].first)
        };
        // 20 -> 200: under 180, so `shorter` leaves it (up through 110) and `longer` goes the other way round
        let x = d(hsl(20.0, 90.0, 50.0), hsl(200.0, 90.0, 50.0));
        assert!(x < 180.0 && x > 179.99, "20 -> 200 reads {x}");
        let x = d(hsl(200.0, 90.0, 50.0), hsl(20.0, 90.0, 50.0));
        assert!(x > -180.0 && x < -179.99, "200 -> 20 reads {x}");
        // 350 -> 170: past 180, so `shorter` wraps: 350 up through 80 (440) to 170 (530)
        let x = d(hsl(350.0, 80.0, 60.0), hsl(170.0, 80.0, 60.0));
        assert!(x < -180.0 && x > -180.01, "350 -> 170 reads {x}");
        let x = d(hsl(170.0, 80.0, 60.0), hsl(350.0, 80.0, 60.0));
        assert!(x > 180.0 && x < 180.01, "170 -> 350 reads {x}");
        // exact pairs stay exact: neither wraps under `shorter`
        assert_eq!(d(hsl(0.0, 100.0, 50.0), hsl(180.0, 100.0, 50.0)), 180.0);
        assert_eq!(d(hsl(180.0, 100.0, 50.0), hsl(0.0, 100.0, 50.0)), -180.0);
        // not a tie: nothing near 180 to decide
        let x = d(hsl(20.0, 90.0, 50.0), hsl(199.0, 90.0, 50.0));
        assert!((x - 179.0).abs() < 0.01);
    }

    /// A stop with no hue of its own (`none`, or one Blink finds powerless) takes each neighbour's.
    #[test]
    fn a_missing_hue_is_the_neighbours() {
        // oklch(0.7 0.2 none), oklch(0.7 0.2 60): the same colour as the neighbour's hue gives
        let s = prepared(Space::Oklch, &[oklch(0.7, 0.2, None), oklch(0.7, 0.2, Some(60.0))]);
        assert!(s[0].second.is_none());
        for (a, b) in s[0].first.iter().zip(s[1].first.iter()) {
            assert!((a - b).abs() < 1e-5, "{:?} / {:?}", s[0].first, s[1].first);
        }
        let (hue, chroma) = skia_oklch(s[0].first);
        assert!((hue - 60.0).abs() < 0.5 && (chroma - 0.2).abs() < 0.005, "hue {hue} chroma {chroma}");
        // in the middle of three stops the stop is split, one colour per neighbour
        let s = prepared(Space::Oklch, &[oklch(0.7, 0.2, Some(30.0)), oklch(0.5, 0.1, None), oklch(0.7, 0.2, Some(250.0))]);
        assert!(s[0].second.is_none() && s[2].second.is_none());
        let (left, right) = (skia_oklch(s[1].first), skia_oklch(s[1].second.expect("the middle stop is split")));
        assert!((left.0 - 30.0).abs() < 0.5 && (right.0 - 250.0).abs() < 0.5, "{left:?} {right:?}");
        // black is exactly black whatever hue it is given: Skia finds its hue powerless and carries on
        let s = prepared(Space::Oklch, &[oklch(0.7, 0.2, Some(150.0)), BLACK()]);
        assert_eq!(s[1].first, [0.0, 0.0, 0.0, 1.0]);
        // a colour written in the interpolation space keeps the hue it was written with, chroma 0 or not
        let mut c = Color::from_stylo(&oklch(0.7, 0.0, Some(200.0)));
        c.convert_for_interpolation(Space::Oklch);
        assert!(!c.has_none());
        assert_eq!(c.p, [0.7, 0.0, 200.0]);
        // hsl: a grey has no hue, and Blink's resolved colour is the same grey exactly (saturation 0)
        let s = prepared(Space::Hsl, &[hsl(120.0, 80.0, 40.0), WHITE(), rgb(0x77, 0x77, 0x77)]);
        assert_eq!(s[1].first, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(s[1].second, Some([1.0, 1.0, 1.0, 1.0]));
        let g = (0x77 as f64 / 255.0) as f32;
        assert_eq!(s[2].first, [g, g, g, 1.0]);
        // a missing alpha is the neighbour's too: oklch(0.7 0.2 30 / none), oklch(0.7 0.2 60 / 0.25)
        let a = AbsoluteColor::new(StyloColorSpace::Oklch, 0.7f32, 0.2f32, 30.0f32, None::<f32>);
        let b = AbsoluteColor::new(StyloColorSpace::Oklch, 0.7f32, 0.2f32, 60.0f32, 0.25f32);
        assert_eq!(prepared(Space::Oklch, &[a, b])[0].first[3], 0.25);
    }

    /// A grey stop in oklch: Blink puts the neighbour's hue on the small chroma its conversion left the grey
    /// with, and Skia, whose own conversion has no such offset, reads a different hue. Chrome 152 draws
    /// `linear-gradient(90deg in oklch, oklch(0.7 0.2 30), white)` from hue 30 to about 110 (measured on its
    /// picture: tests/test_w1a_rust.py holds the comparison of the pictures themselves).
    #[test]
    fn a_grey_in_oklch_reads_as_chrome_draws_it() {
        let s = prepared(Space::Oklch, &[oklch(0.7, 0.2, Some(30.0)), WHITE()]);
        let (hue, chroma) = skia_oklch(s[1].first);
        assert!(chroma > 1e-6, "Skia finds white's hue powerless (chroma {chroma}): the stop is not Blink's");
        assert!((hue - 110.0).abs() < 12.0, "white after hue 30 reads as hue {hue}, Chrome's is about 110");
        assert!(s[1].first.iter().all(|c| (c - 1.0).abs() < 1e-3), "{:?}", s[1].first);
        // #808080 before hue 300: about 246 in Chrome's picture
        let s = prepared(Space::Oklch, &[rgb(0x80, 0x80, 0x80), oklch(0.7, 0.2, Some(300.0))]);
        let (hue, chroma) = skia_oklch(s[0].first);
        assert!(chroma > 1e-6);
        assert!((hue - 246.0).abs() < 12.0, "#808080 before hue 300 reads as hue {hue}, Chrome's is about 246");
    }

    #[test]
    fn analogous_components_carry_forward() {
        let c = |color: AbsoluteColor| Color::from_stylo(&color);
        // oklab(none 0.1 0.1), lch(50 none 20), oklch(0.5 0.1 none), hsl(none 50% 50%), hwb(none 10% 10%)
        let oklab = AbsoluteColor::new(StyloColorSpace::Oklab, None::<f32>, 0.1f32, 0.1f32, 1.0f32);
        let lch = AbsoluteColor::new(StyloColorSpace::Lch, 50.0f32, None::<f32>, 20.0f32, 1.0f32);
        let srgb = AbsoluteColor::new(StyloColorSpace::Srgb, None::<f32>, 0.5f32, 0.5f32, 1.0f32);
        assert_eq!(c(oklab).analogous_missing(Space::Oklch), [true, false, false]);
        assert_eq!(c(lch).analogous_missing(Space::Oklch), [false, true, false]);
        assert_eq!(c(oklch(0.5, 0.1, None)).analogous_missing(Space::Hsl), [true, false, false]);
        assert_eq!(c(hsl_none(50.0, 50.0)).analogous_missing(Space::Oklch), [false, false, true]);
        assert_eq!(c(hsl_none(50.0, 50.0)).analogous_missing(Space::Hwb), [true, false, false]);
        assert_eq!(c(hwb(None, 10.0, 10.0)).analogous_missing(Space::Lch), [false, false, true]);
        assert_eq!(c(srgb).analogous_missing(Space::Oklch), [false; 3]);
        // carried forward, a missing hue stays missing through the conversion
        let mut h = c(hsl_none(50.0, 50.0));
        h.convert_for_interpolation(Space::Oklch);
        assert_eq!(h.none, [false, false, true]);
        assert_eq!(h.p[2], 0.0);
    }

    #[test]
    fn rectangular_spaces_are_not_prepared() {
        assert_eq!(polar_space(StyloColorSpace::Srgb), None);
        assert_eq!(polar_space(StyloColorSpace::Oklab), None);
        assert_eq!(polar_space(StyloColorSpace::Oklch), Some(Space::Oklch));
        assert_eq!(polar_space(StyloColorSpace::Hsl), Some(Space::Hsl));
    }
}
