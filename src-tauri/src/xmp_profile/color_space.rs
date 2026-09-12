//! Pure color-space primitives for the Adobe `dng_rgb_table` renderer.
//!
//! This module owns everything the RGBTable stage needs that is *not* the LUT
//! itself, split into three independent, individually-testable pieces:
//!
//! 1. **[`PrimariesTransform`]** — the linear `working space <-> table primaries`
//!    3x3 hop. Constructed exactly the way [`super::look_table_apply`] builds the
//!    verified sRGB<->ProPhoto hop: replicate Adobe's `dng_color_space::SetMatrixToPCS`
//!    row normalisation of the published raw matrices, then compose
//!    `encode = Invert(tablePrimaries.PCS) * workingSpace.PCS`
//!    (`dng_color_space.cpp:205-228`; `dng_big_table.cpp:5244-5251`). For
//!    `primaries_sRGB` the hop is *provably* the identity, so it is represented by
//!    `None` and the multiply is **skipped entirely** — never performed with a
//!    computed near-identity matrix (that would perturb the approved 5A output).
//! 2. **[`Gamma`]** — the five `gamma_enum` transfer pairs, verbatim from
//!    `dng_color_space.cpp:21-155` and `dng_spline.h:22-50`, including the cubic
//!    Hermite spline toe for 1.8/2.2 and the secant-method numerical inverse
//!    (`dng_1d_function.cpp:33-69`).
//! 3. **[`gamut_clamp`]** — the `gamut_clip` / `gamut_extend` policy
//!    (`dng_reference.cpp:3611-3622`): clamp to `[0,1]` in linear table primaries
//!    in *both* modes, and for `gamut_extend` additionally record the discarded
//!    excursion `unclamped - clamped` so the caller can re-add it after the decode.
//!
//! ### Spec/SDK discrepancy (deliberate, recorded)
//!
//! The SDK's *runtime* gamma encode/decode are `dng_1d_table` sampled
//! interpolations (`dng_big_table.cpp:5346-5354`; default `kDefaultTableSize = 4096`,
//! `dng_1d_table.h:43`) of these same functions, not the analytic forms. The 5B
//! implementation spec mandates the exact analytic transfer functions (matching
//! 5A, which is bit-identical to the pre-5A primitive at `max_delta = 0.0`), so
//! this module implements the analytic forms and ignores the sampling.
//!
//! The measured divergence between Adobe's runtime 4096-entry sampled
//! interpolation and these analytic functions is NOT a single sub-1e-5 number;
//! the independently recomputed maxima of
//! `|4096-sample linear interpolation - analytic|` over `[0,1]` are per gamma:
//!
//! | gamma    | max divergence |
//! |----------|----------------|
//! | sRGB     | ~1.6e-5        |
//! | gamma 2.2| ~5.8e-5        |
//! | gamma 1.8| ~2.0e-4 (largest, in its spline toe) |
//! | Rec2020  | ~1.0e-6        |
//!
//! Any nonzero difference, even the smallest of these, would *break* the
//! required sRGB bit-identity if the sampled table were used.

use super::linear_rgb::{
    compose_working_to_primaries, invert3, mul3, to_f32, Matrix3, Matrix3F64, ADOBE_RGB_RAW,
    DISPLAY_P3_RAW, PROPHOTO_RAW, REC2020_RAW, SRGB_RAW,
};
use super::srgb_transfer::{srgb_decode, srgb_encode};

// -------------------------------------------------------------------- enums
//
// `dng_big_table.h:627-661`, big-table WIRE order (`GetStream`/`PutStream`
// write the in-memory enum directly, `dng_big_table.cpp:2567-2592`).

/// `primaries_enum` values, big-table wire order.
pub(super) const PRIMARIES_SRGB: u32 = 0;
pub(super) const PRIMARIES_ADOBE_RGB: u32 = 1;
pub(super) const PRIMARIES_PROPHOTO: u32 = 2;
pub(super) const PRIMARIES_DISPLAY_P3: u32 = 3;
pub(super) const PRIMARIES_REC2020: u32 = 4;

/// `gamma_enum` values, big-table wire order.
pub(super) const GAMMA_LINEAR: u32 = 0;
pub(super) const GAMMA_SRGB: u32 = 1;
pub(super) const GAMMA_1_8: u32 = 2;
pub(super) const GAMMA_2_2: u32 = 3;
pub(super) const GAMMA_REC2020: u32 = 4;

/// `gamut_enum` values, big-table wire order.
pub(super) const GAMUT_CLIP: u32 = 0;
pub(super) const GAMUT_EXTEND: u32 = 1;

/// Every supported `primaries_enum`, in wire order. Used by the exhaustive tests.
#[cfg(test)]
pub(super) const ALL_PRIMARIES: [Primaries; 5] = [
    Primaries::Srgb,
    Primaries::AdobeRgb,
    Primaries::ProPhoto,
    Primaries::DisplayP3,
    Primaries::Rec2020,
];

/// Every supported `gamma_enum`, in wire order. Used by the exhaustive tests.
#[cfg(test)]
pub(super) const ALL_GAMMAS: [Gamma; 5] = [
    Gamma::Linear,
    Gamma::Srgb,
    Gamma::OnePointEight,
    Gamma::TwoPointTwo,
    Gamma::Rec2020,
];

/// Every supported `gamut_enum`, in wire order. Used by the exhaustive tests.
#[cfg(test)]
pub(super) const ALL_GAMUTS: [Gamut; 2] = [Gamut::Clip, Gamut::Extend];

/// A validated `primaries_enum`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Primaries {
    Srgb,
    AdobeRgb,
    ProPhoto,
    DisplayP3,
    Rec2020,
}

impl Primaries {
    /// Parses the wire enum, rejecting anything outside `0..=4` with a clear,
    /// contextual error. There is deliberately **no** fallback to sRGB.
    pub(super) fn from_enum(value: u32) -> Result<Self, String> {
        match value {
            PRIMARIES_SRGB => Ok(Self::Srgb),
            PRIMARIES_ADOBE_RGB => Ok(Self::AdobeRgb),
            PRIMARIES_PROPHOTO => Ok(Self::ProPhoto),
            PRIMARIES_DISPLAY_P3 => Ok(Self::DisplayP3),
            PRIMARIES_REC2020 => Ok(Self::Rec2020),
            other => Err(format!("unsupported RGBTable primaries: {other}")),
        }
    }

    /// The wire enum value. Used by the exhaustive tests to build tables from the
    /// typed enums.
    #[cfg(test)]
    pub(super) fn wire_value(self) -> u32 {
        match self {
            Self::Srgb => PRIMARIES_SRGB,
            Self::AdobeRgb => PRIMARIES_ADOBE_RGB,
            Self::ProPhoto => PRIMARIES_PROPHOTO,
            Self::DisplayP3 => PRIMARIES_DISPLAY_P3,
            Self::Rec2020 => PRIMARIES_REC2020,
        }
    }
}

/// A validated `gamma_enum`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Gamma {
    Linear,
    Srgb,
    OnePointEight,
    TwoPointTwo,
    Rec2020,
}

impl Gamma {
    /// Parses the wire enum, rejecting anything outside `0..=4`.
    pub(super) fn from_enum(value: u32) -> Result<Self, String> {
        match value {
            GAMMA_LINEAR => Ok(Self::Linear),
            GAMMA_SRGB => Ok(Self::Srgb),
            GAMMA_1_8 => Ok(Self::OnePointEight),
            GAMMA_2_2 => Ok(Self::TwoPointTwo),
            GAMMA_REC2020 => Ok(Self::Rec2020),
            other => Err(format!("unsupported RGBTable gamma: {other}")),
        }
    }

    /// The wire enum value. Used by the exhaustive tests.
    #[cfg(test)]
    pub(super) fn wire_value(self) -> u32 {
        match self {
            Self::Linear => GAMMA_LINEAR,
            Self::Srgb => GAMMA_SRGB,
            Self::OnePointEight => GAMMA_1_8,
            Self::TwoPointTwo => GAMMA_2_2,
            Self::Rec2020 => GAMMA_REC2020,
        }
    }

    /// Transfer encode: linear -> encoded (`dng_color_space.cpp`).
    pub(super) fn encode(self, linear: f32) -> f32 {
        match self {
            // gamma_Linear: the SDK builds no encode table at all.
            Self::Linear => linear,
            // Reuses the shared pair so the sRGB path stays byte-for-byte the
            // 5A implementation.
            Self::Srgb => srgb_encode(linear),
            Self::OnePointEight => gamma_1_8_encode(f64::from(linear)) as f32,
            Self::TwoPointTwo => gamma_2_2_encode(f64::from(linear)) as f32,
            Self::Rec2020 => rec2020_encode(f64::from(linear)) as f32,
        }
    }

    /// Transfer decode: encoded -> linear (the inverse of [`Gamma::encode`]).
    pub(super) fn decode(self, encoded: f32) -> f32 {
        match self {
            Self::Linear => encoded,
            Self::Srgb => srgb_decode(encoded),
            Self::OnePointEight => gamma_1_8_decode(f64::from(encoded)) as f32,
            Self::TwoPointTwo => gamma_2_2_decode(f64::from(encoded)) as f32,
            Self::Rec2020 => rec2020_decode(f64::from(encoded)) as f32,
        }
    }
}

/// A validated `gamut_enum`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Gamut {
    Clip,
    Extend,
}

impl Gamut {
    /// Parses the wire enum, rejecting anything outside `0..=1`.
    pub(super) fn from_enum(value: u32) -> Result<Self, String> {
        match value {
            GAMUT_CLIP => Ok(Self::Clip),
            GAMUT_EXTEND => Ok(Self::Extend),
            other => Err(format!("unsupported RGBTable gamut: {other}")),
        }
    }

    /// The wire enum value. Used by the exhaustive tests.
    #[cfg(test)]
    pub(super) fn wire_value(self) -> u32 {
        match self {
            Self::Clip => GAMUT_CLIP,
            Self::Extend => GAMUT_EXTEND,
        }
    }
}

// -------------------------------------------------------------- primaries 3x3
//
// The D50 PCS white, the raw device->PCS constructor constants and the
// `SetMatrixToPCS`/inverse/multiply helpers live ONCE in [`super::linear_rgb`],
// shared with the LookTable stage's sRGB<->ProPhoto hop. This module keeps only
// the `Primaries` enum -> raw-constant mapping and the composition call.

/// The raw device->PCS matrix Adobe passes to `SetMatrixToPCS` for `primaries`.
fn raw_primaries_matrix(primaries: Primaries) -> Matrix3F64 {
    match primaries {
        Primaries::Srgb => SRGB_RAW,
        Primaries::AdobeRgb => ADOBE_RGB_RAW,
        Primaries::ProPhoto => PROPHOTO_RAW,
        Primaries::DisplayP3 => DISPLAY_P3_RAW,
        Primaries::Rec2020 => REC2020_RAW,
    }
}

/// `M_working -> table primaries` in f64 = `Invert(tablePrimaries.PCS) * sRGB.PCS`,
/// with `workingSpace` = Once-Lab's linear sRGB/D65 (`dng_big_table.cpp:5244-5251`,
/// generalised from `look_table_apply.rs`). The composition itself is the single
/// shared [`super::linear_rgb::compose_working_to_primaries`]; for
/// `primaries_ProPhoto` it therefore yields exactly the matrix the LookTable
/// stage uses.
fn working_to_primaries_f64(primaries: Primaries) -> Matrix3F64 {
    compose_working_to_primaries(raw_primaries_matrix(primaries), SRGB_RAW)
}

/// The linear `working space (Once-Lab: linear sRGB/D65) <-> table primaries` hop.
///
/// For `primaries_sRGB` the composed transform is mathematically the identity;
/// it is stored as `None` so the per-pixel path *skips the multiply* rather than
/// multiplying by a computed near-identity matrix. That is what keeps
/// `primaries_sRGB / gamma_sRGB / gamut_clip` bit-identical to 5A.
#[derive(Clone, Copy, Debug)]
pub(super) struct PrimariesTransform {
    encode: Option<Matrix3>,
    decode: Option<Matrix3>,
}

impl PrimariesTransform {
    pub(super) fn new(primaries: Primaries) -> Self {
        if primaries == Primaries::Srgb {
            return Self {
                encode: None,
                decode: None,
            };
        }

        let encode = working_to_primaries_f64(primaries);
        Self {
            encode: Some(to_f32(encode)),
            decode: Some(to_f32(invert3(encode))),
        }
    }

    /// `working space -> table primaries` (linear). Skips the multiply for sRGB.
    pub(super) fn encode(self, rgb: [f32; 3]) -> [f32; 3] {
        match self.encode {
            Some(matrix) => mul3(matrix, rgb),
            None => rgb,
        }
    }

    /// `table primaries -> working space` (linear). Skips the multiply for sRGB.
    pub(super) fn decode(self, rgb: [f32; 3]) -> [f32; 3] {
        match self.decode {
            Some(matrix) => mul3(matrix, rgb),
            None => rgb,
        }
    }

    /// True when the hop is the sRGB identity shortcut (`primaries_sRGB`).
    #[cfg(test)]
    pub(super) fn is_identity_shortcut(self) -> bool {
        self.encode.is_none()
    }

    /// The stored `working -> table primaries` matrix, or `None` for the sRGB
    /// identity shortcut. Test-only, so `linear_rgb`'s cross-check can pin that
    /// the ProPhoto hop equals the shared composition.
    #[cfg(test)]
    pub(super) fn encode_matrix(self) -> Option<Matrix3> {
        self.encode
    }

    /// The stored `table primaries -> working` matrix, or `None` for the sRGB
    /// identity shortcut. Test-only (see [`Self::encode_matrix`]).
    #[cfg(test)]
    pub(super) fn decode_matrix(self) -> Option<Matrix3> {
        self.decode
    }
}

// The row-major 3x3 multiply this module would otherwise need lives once, in
// [`super::linear_rgb::mul3`]; the sRGB bit-identity path never calls it.

// -------------------------------------------------------- transfer functions

/// `dng_function_GammaEncode_1_8` constants (`dng_color_space.cpp:58-66`).
const GAMMA_1_8_EXPONENT: f64 = 1.0 / 1.8;
const GAMMA_1_8_SLOPE0: f64 = 32.0;
const GAMMA_1_8_X1: f64 = 8.2118790552e-4;
const GAMMA_1_8_Y1: f64 = 0.019310851;
const GAMMA_1_8_SLOPE1: f64 = 13.064306598;

/// `dng_function_GammaEncode_2_2` constants (`dng_color_space.cpp:114-122`).
const GAMMA_2_2_EXPONENT: f64 = 1.0 / 2.2;
const GAMMA_2_2_SLOPE0: f64 = 32.0;
const GAMMA_2_2_X1: f64 = 0.0034800731;
const GAMMA_2_2_Y1: f64 = 0.0763027458;
const GAMMA_2_2_SLOPE1: f64 = 9.9661890075;

/// `dng_function_GammaEncode_Rec709` parameters (`dng_color_space.h:129-146`);
/// `Rec2020` is a `typedef` of `Rec709`, so it uses the same curve.
const REC2020_ALPHA: f64 = 1.0992968268094429;
const REC2020_BETA: f64 = 0.0180539685108078;
const REC2020_SLOPE: f64 = 4.5;
const REC2020_GAMMA: f64 = 0.45;

/// `EvaluateSplineSegment` (`dng_spline.h:22-50`): one cubic Hermite segment
/// between `(x0, y0, slope s0)` and `(x1, y1, slope s1)`.
fn evaluate_spline_segment(
    x: f64,
    x0: f64,
    y0: f64,
    s0: f64,
    x1: f64,
    y1: f64,
    s1: f64,
) -> f64 {
    let a = x1 - x0;
    let b = (x - x0) / a;
    let c = (x1 - x) / a;
    ((y0 * (2.0 - c + b) + (s0 * a * b)) * (c * c)) + ((y1 * (2.0 - b + c) - (s1 * a * c)) * (b * b))
}

/// `dng_function_GammaEncode_1_8::Evaluate` (`dng_color_space.cpp:51-69`).
fn gamma_1_8_encode(x: f64) -> f64 {
    if x <= GAMMA_1_8_X1 {
        evaluate_spline_segment(
            x,
            0.0,
            0.0,
            GAMMA_1_8_SLOPE0,
            GAMMA_1_8_X1,
            GAMMA_1_8_Y1,
            GAMMA_1_8_SLOPE1,
        )
    } else {
        x.powf(GAMMA_1_8_EXPONENT)
    }
}

/// `dng_function_GammaEncode_2_2::Evaluate` (`dng_color_space.cpp:107-125`).
fn gamma_2_2_encode(x: f64) -> f64 {
    if x <= GAMMA_2_2_X1 {
        evaluate_spline_segment(
            x,
            0.0,
            0.0,
            GAMMA_2_2_SLOPE0,
            GAMMA_2_2_X1,
            GAMMA_2_2_Y1,
            GAMMA_2_2_SLOPE1,
        )
    } else {
        x.powf(GAMMA_2_2_EXPONENT)
    }
}

/// `dng_1d_function::EvaluateInverse` (`dng_1d_function.cpp:33-69`): the secant
/// method, replicated exactly (30 iterations, `kNearZero = 1e-10`, `x` pinned to
/// `[0,1]`, seeded `x0 = 0, x1 = 1`) so the decode is deterministic and
/// Adobe-faithful. `evaluate` is the forward transfer being inverted.
fn numerical_inverse(target: f64, evaluate: impl Fn(f64) -> f64) -> f64 {
    const MAX_ITERATIONS: u32 = 30;
    const NEAR_ZERO: f64 = 1.0e-10;

    let mut x0 = 0.0f64;
    let mut y0 = evaluate(x0);

    let mut x1 = 1.0f64;
    let mut y1 = evaluate(x1);

    for _ in 0..MAX_ITERATIONS {
        if (y1 - y0).abs() < NEAR_ZERO {
            break;
        }

        let x2 = (x1 + (target - y1) * (x1 - x0) / (y1 - y0)).clamp(0.0, 1.0);
        let y2 = evaluate(x2);

        x0 = x1;
        y0 = y1;
        x1 = x2;
        y1 = y2;
    }

    x1
}

/// `dng_function_GammaEncode_1_8::EvaluateInverse` (`dng_color_space.cpp:74-83`).
fn gamma_1_8_decode(y: f64) -> f64 {
    if y > 0.0 && y < GAMMA_1_8_Y1 {
        return numerical_inverse(y, gamma_1_8_encode);
    }
    y.powf(1.8)
}

/// `dng_function_GammaEncode_2_2::EvaluateInverse` (`dng_color_space.cpp:130-139`).
fn gamma_2_2_decode(y: f64) -> f64 {
    if y > 0.0 && y < GAMMA_2_2_Y1 {
        return numerical_inverse(y, gamma_2_2_encode);
    }
    y.powf(2.2)
}

/// `dng_function_GammaEncode_TwoPart::Evaluate` with the `Rec709`/`Rec2020`
/// parameters (`dng_color_space.h:91-98, 129-146`).
fn rec2020_encode(x: f64) -> f64 {
    if x <= REC2020_BETA {
        REC2020_SLOPE * x
    } else {
        REC2020_ALPHA * x.powf(REC2020_GAMMA) - (REC2020_ALPHA - 1.0)
    }
}

/// `dng_function_GammaEncode_TwoPart::EvaluateInverse` (`dng_color_space.h:100-108`).
fn rec2020_decode(y: f64) -> f64 {
    if y <= REC2020_SLOPE * REC2020_BETA {
        y / REC2020_SLOPE
    } else {
        ((y + (REC2020_ALPHA - 1.0)) / REC2020_ALPHA).powf(1.0 / REC2020_GAMMA)
    }
}

// -------------------------------------------------------------- gamut policy

/// `dng_reference.cpp:3611-3622`: clamp the linear table-primaries vector to
/// `[0,1]` in BOTH gamut modes, and for `gamut_extend` also return the discarded
/// excursion `unclamped - clamped` (zero for `gamut_clip`).
///
/// `gamut_extend` is *not* "skip the clamp" — it clamps identically and lets the
/// caller re-add the delta after the gamma decode.
///
/// **Deliberate generalisation (Adobe's `hasMatrix` coupling).** In the SDK the
/// clamp and the `gamutDelta` sit *inside* `if (hasMatrix)`
/// (`dng_reference.cpp:3595-3622`), and `hasMatrix` is false whenever the
/// encode/decode matrices are absent. Adobe passes `NULL` for those exactly when
/// the table's primaries equal its own built-in working space — linear ProPhoto —
/// so `fNeedMatrix = (space != NULL)` is false for `primaries_ProPhoto`
/// (`dng_big_table.cpp:5236-5242`, dispatched at `:5416-5417`); Adobe documents
/// the same shortcut in `dng_rgb_table::IsNOP()` (`dng_big_table.cpp:2353`).
/// **That suppression is only valid because Adobe's working space IS linear
/// ProPhoto** — a ProPhoto table needs no device-to-table hop, so there is nothing
/// to clamp or to extend. Once-Lab's working space is linear sRGB/D65, so a
/// ProPhoto table genuinely *does* need a matrix here: `hasMatrix` is effectively
/// always true for us. Applying the spec model (`matrix -> clamp -> …`) uniformly
/// to every primaries enum is therefore the correct generalisation for our
/// working space, not a literal reproduction of Adobe's null-space fast path.
/// See §I of `5c-support-matrix.md`.
pub(super) fn gamut_clamp(linear: [f32; 3], gamut: Gamut) -> ([f32; 3], [f32; 3]) {
    let clamped = [
        linear[0].clamp(0.0, 1.0),
        linear[1].clamp(0.0, 1.0),
        linear[2].clamp(0.0, 1.0),
    ];

    let delta = match gamut {
        Gamut::Clip => [0.0f32; 3],
        Gamut::Extend => [
            linear[0] - clamped[0],
            linear[1] - clamped[1],
            linear[2] - clamped[2],
        ],
    };

    (clamped, delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// f32 tolerance for a round trip that should be mathematically exact: the
    /// transfer pair, a 3x3 inverse product, or a neutral through the hop.
    const ROUND_TRIP_TOLERANCE: f32 = 1e-5;

    fn assert_close(actual: f32, expected: f32, tolerance: f32, context: &str) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{context}: expected {expected}, got {actual}"
        );
    }

    // ------------------------------------------------------- transfer encode

    #[test]
    fn every_gamma_round_trips_at_and_around_its_breakpoint() {
        // Boundary-aware probe sets: for each curve, the linear breakpoint, the
        // encoded breakpoint, and points on both sides of each.
        let cases: [(Gamma, &[f32]); 5] = [
            (Gamma::Linear, &[0.0, 0.25, 0.5, 1.0]),
            (Gamma::Srgb, &[0.0, 0.0031308, 0.04045, 0.18, 0.5, 1.0]),
            (
                Gamma::OnePointEight,
                &[0.0, GAMMA_1_8_X1 as f32, 0.019310851, 0.18, 0.5, 1.0],
            ),
            (
                Gamma::TwoPointTwo,
                &[0.0, GAMMA_2_2_X1 as f32, 0.0763027458, 0.18, 0.5, 1.0],
            ),
            (
                Gamma::Rec2020,
                &[0.0, 0.0180539685108078, 0.0812428583, 0.18, 0.5, 1.0],
            ),
        ];

        for (gamma, probes) in cases {
            for &value in probes {
                let encoded = gamma.encode(value);
                // The encode must map [0,1] onto [0,1] so the LUT input contract
                // holds for every gamma.
                assert!(
                    (0.0..=1.0).contains(&encoded),
                    "{gamma:?}: encode({value}) = {encoded} left [0,1]"
                );

                let round_trip = gamma.decode(encoded);
                assert_close(
                    round_trip,
                    value,
                    ROUND_TRIP_TOLERANCE,
                    &format!("{gamma:?} round trip of {value}"),
                );
            }
        }

        // Non-vacuity: Linear must be an exact identity and sRGB must NOT be,
        // otherwise the per-gamma dispatch could be a single curve.
        assert_eq!(Gamma::Linear.encode(0.25), 0.25);
        assert!(
            (Gamma::Srgb.encode(0.25) - 0.25).abs() > 1e-3,
            "sRGB encode must move a mid grey"
        );
    }

    #[test]
    fn gamma_1_8_and_2_2_use_a_curved_spline_toe_not_a_power_or_line() {
        // At the toe endpoint the spline must meet the power segment exactly.
        assert_close(
            gamma_1_8_encode(GAMMA_1_8_X1) as f32,
            GAMMA_1_8_Y1 as f32,
            1e-9,
            "1.8 spline meets its endpoint",
        );
        assert_close(
            gamma_2_2_encode(GAMMA_2_2_X1) as f32,
            GAMMA_2_2_Y1 as f32,
            1e-9,
            "2.2 spline meets its endpoint",
        );

        // Below the breakpoint the spline is CURVED, so it must differ from both
        // the pure power and the straight `slope0 * x` toe. A pure-power or
        // linear-toe implementation would fail these.
        for &(x, x1, y1, power, name) in &[
            (GAMMA_1_8_X1 * 0.5, GAMMA_1_8_X1, GAMMA_1_8_Y1, GAMMA_1_8_EXPONENT, "1.8"),
            (GAMMA_2_2_X1 * 0.5, GAMMA_2_2_X1, GAMMA_2_2_Y1, GAMMA_2_2_EXPONENT, "2.2"),
        ] {
            let toe = if name == "1.8" {
                gamma_1_8_encode(x)
            } else {
                gamma_2_2_encode(x)
            };
            let power_only = x.powf(power);
            let linear_toe = GAMMA_1_8_SLOPE0 * x;

            assert!(
                (toe - power_only).abs() > 1e-4,
                "{name}: spline toe {toe} must differ from the pure power {power_only}"
            );
            assert!(
                (toe - linear_toe).abs() > 1e-4,
                "{name}: spline toe {toe} must differ from the straight toe {linear_toe}"
            );
            // Sanity: the spline starts at 0 (it is anchored at (0,0)).
            let _ = (x1, y1);
        }

        // The start slope is 32: near x = 0 the derivative ~ 32.
        let slope = gamma_1_8_encode(1e-9) / 1e-9;
        assert!(
            (slope - GAMMA_1_8_SLOPE0).abs() < 1e-3,
            "spline start slope should be ~32, got {slope}"
        );
    }

    #[test]
    fn gamma_1_8_numerical_inverse_matches_adobe_secant_and_beats_a_power() {
        // Re-implement the SDK secant inline, independently of the production
        // helper, and require exact agreement on a spread of toe values.
        fn adobe_secant(target: f64, evaluate: impl Fn(f64) -> f64) -> f64 {
            let (mut x0, mut y0) = (0.0f64, evaluate(0.0f64));
            let (mut x1, mut y1) = (1.0f64, evaluate(1.0f64));
            for _ in 0..30u32 {
                if (y1 - y0).abs() < 1.0e-10 {
                    break;
                }
                let x2 = (x1 + (target - y1) * (x1 - x0) / (y1 - y0)).clamp(0.0, 1.0);
                let y2 = evaluate(x2);
                (x0, y0) = (x1, y1);
                (x1, y1) = (x2, y2);
            }
            x1
        }

        // The secant must agree exactly with an independent replica of Adobe's
        // loop, and somewhere in the toe the true (curved) inverse must depart
        // from the naive pure power `y^1.8` - otherwise this test would pass for a
        // wrong, pure-power decode. The two coincide only at the toe endpoint
        // (`Y1^1.8 = x1`), so the gap is measured as a maximum over the toe.
        let mut max_naive_gap = 0.0f32;

        for step in 1..20 {
            let y = GAMMA_1_8_Y1 * f64::from(step) / 20.0;
            let expected = adobe_secant(y, gamma_1_8_encode) as f32;

            let inverse = gamma_1_8_decode(y) as f32;
            assert_close(
                inverse,
                expected,
                1e-9,
                &format!("1.8 secant inverse of {y}"),
            );

            max_naive_gap = max_naive_gap.max((inverse - y.powf(1.8) as f32).abs());
        }

        assert!(
            max_naive_gap > 1e-6,
            "the 1.8 toe inverse must differ from the pure power y^1.8 somewhere, \
             but the largest gap was only {max_naive_gap}"
        );
    }

    #[test]
    fn gamma_rec2020_breakpoint_is_the_slope_times_beta() {
        let breakpoint = (REC2020_SLOPE * REC2020_BETA) as f32;
        assert_close(
            Gamma::Rec2020.encode(REC2020_BETA as f32),
            breakpoint,
            1e-6,
            "Rec2020 encode breakpoint",
        );
        // The two branches agree at the breakpoint within f32 noise.
        assert_close(
            Gamma::Rec2020.decode(breakpoint),
            REC2020_BETA as f32,
            1e-6,
            "Rec2020 decode breakpoint",
        );
    }

    /// Every transfer function must be finite and stay inside `[0,1]` at its own
    /// breakpoints and on BOTH sides of them, and the two analytic branches must
    /// agree across each breakpoint (no jump, no NaN). The encoded breakpoint is
    /// the linear breakpoint passed through the curve.
    #[test]
    fn every_transfer_is_finite_and_continuous_on_both_sides_of_each_breakpoint() {
        const SIDE: f32 = 1.0e-3;
        // Tolerance for the (small) gap between the two analytic branches at a
        // breakpoint, which must be continuous.
        const BREAK_TOLERANCE: f32 = 1.0e-3;

        let cases: [(Gamma, f32, f32); 5] = [
            (Gamma::Linear, 0.5, 0.5),
            (Gamma::Srgb, 0.0031308, 0.04045),
            (Gamma::OnePointEight, GAMMA_1_8_X1 as f32, GAMMA_1_8_Y1 as f32),
            (Gamma::TwoPointTwo, GAMMA_2_2_X1 as f32, GAMMA_2_2_Y1 as f32),
            (
                Gamma::Rec2020,
                REC2020_BETA as f32,
                (REC2020_SLOPE * REC2020_BETA) as f32,
            ),
        ];

        for (gamma, linear_bp, encoded_bp) in cases {
            // Encode side: the endpoints of [0,1], the breakpoint and both sides.
            let below = gamma.encode(linear_bp * (1.0 - SIDE));
            let above = gamma.encode(linear_bp * (1.0 + SIDE));

            for value in [0.0f32, 1.0, linear_bp, linear_bp * (1.0 - SIDE), linear_bp * (1.0 + SIDE)]
            {
                let encoded = gamma.encode(value);
                assert!(
                    encoded.is_finite(),
                    "{gamma:?}: encode({value}) is not finite"
                );
                assert!(
                    (0.0..=1.0).contains(&encoded),
                    "{gamma:?}: encode({value}) = {encoded} left [0,1]"
                );
            }

            let encode_gap = (above - below).abs();

            // Decode side: both sides of the encoded breakpoint must be finite and
            // in-range.
            let decode_below = gamma.decode(encoded_bp * (1.0 - SIDE));
            let decode_above = gamma.decode(encoded_bp * (1.0 + SIDE));

            for value in [encoded_bp, encoded_bp * (1.0 - SIDE), encoded_bp * (1.0 + SIDE)] {
                let linear = gamma.decode(value);
                assert!(
                    linear.is_finite(),
                    "{gamma:?}: decode({value}) is not finite"
                );
                assert!(
                    (0.0..=1.0).contains(&linear),
                    "{gamma:?}: decode({value}) = {linear} left [0,1]"
                );
            }

            let decode_gap = (decode_above - decode_below).abs();

            // Linear has no breakpoint (it is the identity), so the gap is simply
            // the slope over the probe window; only the piecewise curves express a
            // branch that must meet continuously.
            if gamma != Gamma::Linear {
                assert!(
                    encode_gap < BREAK_TOLERANCE,
                    "{gamma:?}: encode jumps by {encode_gap} across its breakpoint {linear_bp}"
                );
                assert!(
                    decode_gap < BREAK_TOLERANCE,
                    "{gamma:?}: decode jumps by {decode_gap} across its breakpoint {encoded_bp}"
                );
            }
        }

        // Non-vacuity: the per-gamma dispatch must not be a single curve. At the
        // sRGB linear breakpoint the sRGB encode and the Linear encode differ.
        assert!(
            (Gamma::Srgb.encode(0.0031308) - Gamma::Linear.encode(0.0031308)).abs() > 1e-3,
            "sRGB and Linear must genuinely differ at the sRGB breakpoint"
        );
    }

    // -------------------------------------------------------- primaries hop

    #[test]
    fn primaries_transform_round_trips_and_preserves_neutral_for_all_five() {
        for primaries in ALL_PRIMARIES {
            let transform = PrimariesTransform::new(primaries);

            // Neutrals and white must map to themselves across the hop: the
            // normalisation exists precisely to make that true.
            for neutral in [[0.0f32, 0.0, 0.0], [0.18, 0.18, 0.18], [0.5, 0.5, 0.5], [1.0, 1.0, 1.0]] {
                let forward = transform.encode(neutral);
                assert_close_vec(forward, neutral, ROUND_TRIP_TOLERANCE, "forward neutral");

                let back = transform.decode(forward);
                assert_close_vec(back, neutral, ROUND_TRIP_TOLERANCE, "round trip neutral");
            }

            // Full 3x3 round trip for a chromatic colour (modulo clamping, which
            // is not part of the hop).
            let chromatic = [0.4f32, 0.7, 0.2];
            let back = transform.decode(transform.encode(chromatic));
            assert_close_vec(back, chromatic, ROUND_TRIP_TOLERANCE, "chromatic round trip");

            // Non-vacuity: only sRGB may be the identity; the other four must
            // genuinely move a saturated primary.
            let moved = transform.encode([1.0f32, 0.0, 0.0]);
            if primaries == Primaries::Srgb {
                assert!(
                    transform.is_identity_shortcut(),
                    "sRGB must use the identity shortcut"
                );
                assert_eq!(moved, [1.0f32, 0.0, 0.0], "sRGB red must be a fixed point");
            } else {
                assert!(
                    !transform.is_identity_shortcut(),
                    "{primaries:?} must not use the identity shortcut"
                );
                let delta = (moved[0] - 1.0).abs() + moved[1].abs() + moved[2].abs();
                assert!(
                    delta > 0.05,
                    "{primaries:?} must move pure red materially, got {moved:?}"
                );
            }
        }
    }

    /// f32 tolerance for pinning a composed primaries matrix to an
    /// independently recomputed literal. The expected values are given to 7
    /// decimals (error <= 5e-8) and the implementation rounds the f64
    /// derivation to f32 (error <= ~6e-8), so 1e-6 is a tight but safe bound.
    const PRIMARIES_MATRIX_TOLERANCE: f32 = 1e-6;

    /// f32 tolerance for the f32-rounded `decode * encode` product, which should
    /// be the identity but accumulates a few ULP of rounding.
    const PRIMARIES_INVERSE_TOLERANCE: f32 = 1e-5;

    fn assert_matrix_close(actual: Matrix3, expected: Matrix3, context: &str) {
        for row in 0..3 {
            for col in 0..3 {
                assert!(
                    (actual[row][col] - expected[row][col]).abs() <= PRIMARIES_MATRIX_TOLERANCE,
                    "{context}[{row}][{col}]: expected {}, got {}",
                    expected[row][col],
                    actual[row][col]
                );
            }
        }
    }

    fn mul_matrix3(a: Matrix3, b: Matrix3) -> Matrix3 {
        let mut out = [[0.0f32; 3]; 3];
        for row in 0..3 {
            for col in 0..3 {
                out[row][col] = (0..3).map(|k| a[row][k] * b[k][col]).sum();
            }
        }
        out
    }

    #[test]
    fn primaries_encode_matrices_are_numerically_pinned_to_the_sdk_constants() {
        // Independently recomputed from the SDK's raw constants -
        // `dng_space_AdobeRGB` / `ProPhoto` / `DisplayP3` / `Rec2020`
        // constructors in `dng_color_space.cpp` (the LIVE `#else` branches),
        // each passed through `SetMatrixToPCS` (D50 row normalisation,
        // `dng_color_space.cpp:205-228`), then composed as
        // `Invert(table.PCS) * sRGB.PCS`. This pins the VALUES, so a
        // plausible-but-wrong substitution (e.g. the dead DCI-P3
        // chromaticity-derived constants in the `#if 0` block at
        // `dng_color_space.cpp:757-786`) can no longer pass the suite.
        let expected: [(Primaries, Matrix3); 4] = [
            (
                Primaries::AdobeRgb,
                [
                    [0.7152104, 0.2847598, 0.0000298],
                    [0.0000035, 1.0000177, -0.0000211],
                    [-0.0000651, 0.0411420, 0.9589231],
                ],
            ),
            (
                Primaries::ProPhoto,
                [
                    [0.5292993, 0.3300508, 0.1406499],
                    [0.0984129, 0.8734845, 0.0281026],
                    [0.0168464, 0.1176827, 0.8654709],
                ],
            ),
            (
                Primaries::DisplayP3,
                [
                    [0.8225481, 0.1773356, 0.0001162],
                    [0.0331828, 0.9669250, -0.0001078],
                    [0.0170010, 0.0723893, 0.9106097],
                ],
            ),
            (
                Primaries::Rec2020,
                [
                    [0.6274929, 0.3291938, 0.0433133],
                    [0.0690386, 0.9196046, 0.0113569],
                    [0.0163377, 0.0880054, 0.8956569],
                ],
            ),
        ];

        for (primaries, expected) in expected {
            let encode_f64 = working_to_primaries_f64(primaries);
            let encode = to_f32(encode_f64);
            assert_matrix_close(encode, expected, &format!("{primaries:?} encode"));

            // The D50 row normalisation makes device white `(1,1,1)` land on the
            // PCS white, so the composed hop maps neutrals to themselves.
            assert_close_vec(
                mul3(encode, [1.0, 1.0, 1.0]),
                [1.0, 1.0, 1.0],
                PRIMARIES_MATRIX_TOLERANCE,
                &format!("{primaries:?} encode maps (1,1,1)"),
            );

            // The decode matrix is the inverse of the encode matrix.
            let decode = to_f32(invert3(encode_f64));
            let product = mul_matrix3(decode, encode);
            for row in 0..3 {
                for col in 0..3 {
                    let identity = if row == col { 1.0f32 } else { 0.0f32 };
                    assert!(
                        (product[row][col] - identity).abs() <= PRIMARIES_INVERSE_TOLERANCE,
                        "{primaries:?} decode*encode[{row}][{col}]: expected {identity}, got {}",
                        product[row][col]
                    );
                }
            }
        }
    }

    #[test]
    fn srgb_primaries_uses_the_identity_shortcut_with_no_multiply() {
        // sRGB is proved to be the identity, so the implementation stores `None`
        // and skips the matrix multiply entirely (a computed near-identity matrix
        // would perturb the approved 5A output). Assert the no-multiply path.
        let transform = PrimariesTransform::new(Primaries::Srgb);
        assert!(
            transform.is_identity_shortcut(),
            "sRGB must use the identity shortcut (stored None), not a computed matrix"
        );

        for probe in [
            [0.0f32, 0.0, 0.0],
            [0.25, 0.5, 0.75],
            [1.0, 1.0, 1.0],
            [0.4, 0.7, 0.2],
            [-0.5, 0.25, 1.5],
        ] {
            // Bit-identical pass-through in both directions.
            assert_eq!(transform.encode(probe), probe, "encode must not multiply");
            assert_eq!(transform.decode(probe), probe, "decode must not multiply");
        }

        // Non-vacuity: a non-sRGB hop must NOT be the identity shortcut and must
        // genuinely move a probe, so the assertions above are meaningful.
        let p3 = PrimariesTransform::new(Primaries::DisplayP3);
        assert!(!p3.is_identity_shortcut());
        let moved = p3.encode([0.4, 0.7, 0.2]);
        assert!(
            (moved[0] - 0.4).abs() + (moved[1] - 0.7).abs() + (moved[2] - 0.2).abs() > 1e-3,
            "DisplayP3 must actually move the probe, got {moved:?}"
        );
    }

    fn assert_close_vec(actual: [f32; 3], expected: [f32; 3], tolerance: f32, context: &str) {
        for channel in 0..3 {
            assert_close(
                actual[channel],
                expected[channel],
                tolerance,
                &format!("{context} channel {channel}"),
            );
        }
    }

    // ------------------------------------------------------------- gamut

    #[test]
    fn gamut_clip_discards_and_extend_records_the_excursion() {
        let linear = [-0.25f32, 0.5, 1.75];

        let (clipped, clip_delta) = gamut_clamp(linear, Gamut::Clip);
        assert_eq!(clipped, [0.0f32, 0.5, 1.0]);
        assert_eq!(clip_delta, [0.0f32; 3], "clip records no delta");

        let (extended, extend_delta) = gamut_clamp(linear, Gamut::Extend);

        // extend clamps IDENTICALLY...
        assert_eq!(
            extended, clipped,
            "extend must clamp identically to clip, not skip the clamp"
        );
        // ...and records the discarded excursion in linear table primaries.
        assert_eq!(extend_delta, [-0.25f32, 0.0, 0.75]);
        for channel in 0..3 {
            assert_eq!(linear[channel] - clipped[channel], extend_delta[channel]);
        }

        // Non-vacuity: the delta is non-zero, so "extend == clip" would be a
        // meaningful failure.
        assert!(
            extend_delta.iter().any(|d| *d != 0.0),
            "the test input must produce a non-zero excursion"
        );
    }

    /// The gamut policy must hold for the extremes as well: an all-negative and an
    /// all-huge vector clamp to the same `[0,1]` corners in BOTH modes, clip always
    /// discards the excursion, and extend records it exactly -- finitely, for
    /// `f32::MIN`/`f32::MAX` too.
    #[test]
    fn gamut_clamp_handles_all_negative_and_all_huge_inputs() {
        let cases: [([f32; 3], [f32; 3], [f32; 3]); 4] = [
            ([-1.0, -2.0, -3.0], [0.0, 0.0, 0.0], [-1.0, -2.0, -3.0]),
            ([100.0, 200.0, 300.0], [1.0, 1.0, 1.0], [99.0, 199.0, 299.0]),
            ([f32::MIN; 3], [0.0, 0.0, 0.0], [f32::MIN; 3]),
            ([f32::MAX; 3], [1.0, 1.0, 1.0], [f32::MAX; 3]),
        ];

        for (linear, expected_clamped, expected_delta) in cases {
            let (clipped, clip_delta) = gamut_clamp(linear, Gamut::Clip);
            assert_eq!(
                clipped, expected_clamped,
                "clip must clamp {linear:?} to [0,1]"
            );
            assert_eq!(clip_delta, [0.0f32; 3], "clip records no excursion");

            let (extended, extend_delta) = gamut_clamp(linear, Gamut::Extend);
            assert_eq!(
                extended, clipped,
                "extend must clamp {linear:?} identically to clip"
            );
            assert_eq!(
                extend_delta, expected_delta,
                "extend must record the exact excursion of {linear:?}"
            );

            for value in clipped.iter().chain(extend_delta.iter()) {
                assert!(
                    value.is_finite(),
                    "gamut handling of {linear:?} produced a non-finite value"
                );
            }
        }
    }

    // ---------------------------------------------------------- enum parsing

    #[test]
    fn unsupported_enum_values_fail_with_context_and_no_fallback() {
        // Exactly the supported ranges are accepted...
        for value in 0..=4u32 {
            assert!(Primaries::from_enum(value).is_ok(), "primaries {value}");
            assert!(Gamma::from_enum(value).is_ok(), "gamma {value}");
        }
        for value in 0..=1u32 {
            assert!(Gamut::from_enum(value).is_ok(), "gamut {value}");
        }

        // ...and everything else fails loudly, with a contextual message.
        assert_eq!(
            Primaries::from_enum(5).unwrap_err(),
            "unsupported RGBTable primaries: 5"
        );
        assert_eq!(
            Gamma::from_enum(9).unwrap_err(),
            "unsupported RGBTable gamma: 9"
        );
        assert_eq!(
            Gamut::from_enum(7).unwrap_err(),
            "unsupported RGBTable gamut: 7"
        );
        assert!(Primaries::from_enum(u32::MAX).is_err());
        assert!(Gamma::from_enum(u32::MAX).is_err());
        assert!(Gamut::from_enum(u32::MAX).is_err());
    }
}
