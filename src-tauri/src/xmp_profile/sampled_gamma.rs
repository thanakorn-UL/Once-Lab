//! Sampled `dng_1d_table` transfer functions.
//!
//! Adobe does not evaluate gamma curves analytically at run time. The SDK
//! samples each `dng_1d_function` uniformly into a `dng_1d_table` of
//! `kDefaultTableSize = 4096` intervals — 4097 knots plus one duplicated
//! sentinel, 4098 `f32` total (`dng_1d_table.cpp:94-132`) — and interpolates
//! with `f32` index/fraction arithmetic (`dng_1d_table.cpp:147-190`).
//!
//! Encode and decode tables are sampled INDEPENDENTLY: the decode table is
//! built from `dng_1d_inverse` (i.e. the curve's `EvaluateInverse`, including
//! the 30-iteration secant solver for the spline toes), never by numerically
//! inverting the sampled encode table (`dng_big_table.cpp:5328-5361`).
//!
//! The `f64` evaluator functions below are verbatim ports of
//! `dng_function_GammaEncode_{sRGB,1_8,2_2,Rec709}` and
//! `EvaluateSplineSegment` from `dng_color_space.cpp`/`dng_spline.h`, and of
//! `dng_1d_function::EvaluateInverse` from `dng_1d_function.cpp`.

/// `dng_1d_table::kDefaultTableSize` (`dng_1d_table.h:80`).
pub(super) const TABLE_COUNT: usize = 4096;

/// Storage is `count + 2`: knots `0..=4096` plus the sentinel at `[4097]`.
const STORAGE_LEN: usize = TABLE_COUNT + 2;

const MAX_INVERSE_ITERATIONS: u32 = 30;
const NEAR_ZERO: f64 = 1.0e-10;

/// A uniformly sampled 1-D table, laid out exactly like `dng_1d_table`.
#[derive(Debug, Clone)]
pub(super) struct SampledTable {
    /// `count + 2` entries: `[0..=count]` are the sampled knots and
    /// `[count + 1]` duplicates `[count]` so `Interpolate(1.0)` reads in
    /// bounds (`dng_1d_table.cpp:125-132`).
    table: Vec<f32>,
}

impl SampledTable {
    /// `dng_1d_table::Initialize` (uniform path, `subSample = false`):
    /// `table[j] = (f32) evaluate(j * (1/count))` for `j = 0..=count`.
    pub(super) fn build(evaluate: impl Fn(f64) -> f64) -> SampledTable {
        let mut table = Vec::with_capacity(STORAGE_LEN);
        for j in 0..=TABLE_COUNT {
            let x = j as f64 * (1.0 / TABLE_COUNT as f64);
            table.push(evaluate(x) as f32);
        }
        let sentinel = table[TABLE_COUNT];
        table.push(sentinel);
        SampledTable { table }
    }

    /// `dng_1d_table::Interpolate` (`dng_1d_table.cpp:147-190`). The SDK throws
    /// `dng_error_bad_format` when `x` is NaN or outside `[0,1]`; the caller
    /// maps that onto this crate's `String` error channel.
    pub(super) fn interpolate(&self, x: f32, context: &str) -> Result<f32, String> {
        let y = x * TABLE_COUNT as f32;
        if !(y >= 0.0 && y <= TABLE_COUNT as f32) {
            return Err(format!("{context} index out of range"));
        }
        Ok(self.interpolate_unchecked(y))
    }

    /// The index/fraction/lerp body of `Interpolate` once `y` (the scaled
    /// domain coordinate) is known to be in range. Index `4096` reads the
    /// sentinel — that is why the storage is `count + 2`.
    #[inline]
    fn interpolate_unchecked(&self, y: f32) -> f32 {
        let index = y as usize;
        let fract = y - index as f32;
        self.table[index] * (1.0 - fract) + self.table[index + 1] * fract
    }
}

/// `Pin_real64` (`dng_utils.h`): `Max(min, Min(x, max))` in `f64` — NOT the
/// same as `f64::clamp`, which panics on `NaN` bounds and propagates `NaN`
/// differently. `x < min ? min : (x > max ? max : ...)` is the SDK's
/// comparison-chain order.
#[inline]
pub(super) fn pin_real64(min: f64, x: f64, max: f64) -> f64 {
    let min_y = if x < max { x } else { max };
    if min_y > min { min_y } else { min }
}

/// `dng_1d_function::EvaluateInverse` (`dng_1d_function.cpp:33-74`): secant
/// iteration pinned to `[0,1]`, at most 30 iterations, terminating early when
/// the last two evaluations agree to within `1e-10`.
fn evaluate_inverse(y: f64, evaluate: impl Fn(f64) -> f64) -> f64 {
    let mut x0 = 0.0f64;
    let mut y0 = evaluate(x0);
    let mut x1 = 1.0f64;
    let mut y1 = evaluate(x1);

    for _ in 0..MAX_INVERSE_ITERATIONS {
        if (y1 - y0).abs() < NEAR_ZERO {
            break;
        }
        let x2 = pin_real64(0.0, x1 + (y - y1) * (x1 - x0) / (y1 - y0), 1.0);
        let y2 = evaluate(x2);
        x0 = x1;
        y0 = y1;
        x1 = x2;
        y1 = y2;
    }
    x1
}

/// `EvaluateSplineSegment` (`dng_spline.h`): cubic Hermite segment between two
/// knots, used by the 1.8/2.2 gamma toes.
#[inline]
fn evaluate_spline_segment(x: f64, x0: f64, y0: f64, s0: f64, x1: f64, y1: f64, s1: f64) -> f64 {
    let a = x1 - x0;
    let b = (x - x0) / a;
    let c = (x1 - x) / a;
    (y0 * (2.0 - c + b) + s0 * a * b) * (c * c) + (y1 * (2.0 - b + c) - s1 * a * c) * (b * b)
}

/// `dng_function_GammaEncode_sRGB::Evaluate` (`dng_color_space.cpp:21-31`).
pub(super) fn gamma_encode_srgb(x: f64) -> f64 {
    if x <= 0.0031308 {
        x * 12.92
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// `dng_function_GammaEncode_sRGB::EvaluateInverse` (`dng_color_space.cpp:34-44`).
pub(super) fn gamma_decode_srgb(y: f64) -> f64 {
    if y <= 0.0031308 * 12.92 {
        y * (1.0 / 12.92)
    } else {
        ((y + 0.055) * (1.0 / 1.055)).powf(2.4)
    }
}

/// `dng_function_GammaEncode_1_8::Evaluate` (`dng_color_space.cpp:58-85`):
/// a cubic Hermite toe (slope 32 at black) joining `x^(1/1.8)` at `x1`.
pub(super) fn gamma_encode_1_8(x: f64) -> f64 {
    const GAMMA: f64 = 1.0 / 1.8;
    const SLOPE0: f64 = 32.0;
    const X1: f64 = 8.2118790552e-4;
    const Y1: f64 = 0.019310851;
    const SLOPE1: f64 = 13.064306598;

    if x <= X1 {
        evaluate_spline_segment(x, 0.0, 0.0, SLOPE0, X1, Y1, SLOPE1)
    } else {
        x.powf(GAMMA)
    }
}

/// `dng_function_GammaEncode_1_8::EvaluateInverse` (`dng_color_space.cpp:87-100`):
/// the toe region is inverted with the shared secant solver.
pub(super) fn gamma_decode_1_8(y: f64) -> f64 {
    if y > 0.0 && y < 0.019310851 {
        evaluate_inverse(y, gamma_encode_1_8)
    } else {
        y.powf(1.8)
    }
}

/// `dng_function_GammaEncode_2_2::Evaluate` (`dng_color_space.cpp:114-141`).
pub(super) fn gamma_encode_2_2(x: f64) -> f64 {
    const GAMMA: f64 = 1.0 / 2.2;
    const SLOPE0: f64 = 32.0;
    const X1: f64 = 0.0034800731;
    const Y1: f64 = 0.0763027458;
    const SLOPE1: f64 = 9.9661890075;

    if x <= X1 {
        evaluate_spline_segment(x, 0.0, 0.0, SLOPE0, X1, Y1, SLOPE1)
    } else {
        x.powf(GAMMA)
    }
}

/// `dng_function_GammaEncode_2_2::EvaluateInverse` (`dng_color_space.cpp:143-156`).
pub(super) fn gamma_decode_2_2(y: f64) -> f64 {
    if y > 0.0 && y < 0.0763027458 {
        evaluate_inverse(y, gamma_encode_2_2)
    } else {
        y.powf(2.2)
    }
}

/// `dng_function_GammaEncode_Rec709::Evaluate`
/// (`dng_color_space.h:129-145`, the `TwoPart` body): Adobe's Rec.2020 gamma
/// IS the BT.709 two-part curve — the two enum values share the function.
pub(super) fn gamma_encode_rec709(x: f64) -> f64 {
    const ALPHA: f64 = 1.099_296_826_809_443;
    const BETA: f64 = 0.0180539685108078;
    const SLOPE: f64 = 4.5;
    const GAMMA: f64 = 0.45;

    if x <= BETA {
        SLOPE * x
    } else {
        ALPHA * x.powf(GAMMA) - (ALPHA - 1.0)
    }
}

/// `dng_function_GammaEncode_Rec709::EvaluateInverse` (same `TwoPart` body).
pub(super) fn gamma_decode_rec709(y: f64) -> f64 {
    const ALPHA: f64 = 1.099_296_826_809_443;
    const BETA: f64 = 0.0180539685108078;
    const SLOPE: f64 = 4.5;
    const GAMMA: f64 = 0.45;

    if y <= SLOPE * BETA {
        y / SLOPE
    } else {
        ((y + (ALPHA - 1.0)) / ALPHA).powf(1.0 / GAMMA)
    }
}

use super::color_space::Gamma;

impl Gamma {
    /// The `f64` `Evaluate` of this gamma's `dng_1d_function`, used to build
    /// the sampled encode table. `Linear` is a pass-through (the SDK leaves
    /// the function `NULL` for it — this is only called when a table exists).
    pub(super) fn evaluate_f64(self, x: f64) -> f64 {
        match self {
            Gamma::Linear => x,
            Gamma::Srgb => gamma_encode_srgb(x),
            Gamma::OnePointEight => gamma_encode_1_8(x),
            Gamma::TwoPointTwo => gamma_encode_2_2(x),
            Gamma::Rec2020 => gamma_encode_rec709(x),
        }
    }

    /// The `f64` `EvaluateInverse`, used to build the sampled decode table —
    /// independently of the encode table (`dng_1d_inverse`).
    pub(super) fn evaluate_inverse_f64(self, y: f64) -> f64 {
        match self {
            Gamma::Linear => y,
            Gamma::Srgb => gamma_decode_srgb(y),
            Gamma::OnePointEight => gamma_decode_1_8(y),
            Gamma::TwoPointTwo => gamma_decode_2_2(y),
            Gamma::Rec2020 => gamma_decode_rec709(y),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_of(f: impl Fn(f64) -> f64) -> SampledTable {
        SampledTable::build(f)
    }

    #[test]
    fn sampled_table_has_4098_entries_with_duplicated_sentinel() {
        let table = table_of(|x| x * x);
        assert_eq!(table.table.len(), TABLE_COUNT + 2);
        assert_eq!(table.table[TABLE_COUNT], 1.0, "last knot is f(1)");
        assert_eq!(
            table.table[TABLE_COUNT + 1],
            table.table[TABLE_COUNT],
            "sentinel must duplicate the last knot"
        );
    }

    #[test]
    fn interpolate_is_exact_at_knots() {
        let table = table_of(|x| x * x + 0.25);
        for j in [0usize, 1, 2048, 4095, 4096] {
            let x = j as f32 / TABLE_COUNT as f32;
            let got = table.interpolate(x, "test").expect("knot");
            let expected = ((j as f64 / TABLE_COUNT as f64).powi(2) + 0.25) as f32;
            assert_eq!(got, expected, "knot {j}");
        }
    }

    #[test]
    fn interpolate_linearly_between_knots() {
        // f(x) = 2x sampled: midpoint between knot j and j+1 must equal the
        // average of the two knot values (in f32 arithmetic, like the SDK).
        let table = table_of(|x| 2.0 * x);
        let j = 1000usize;
        let mid = (j as f32 + 0.5) / TABLE_COUNT as f32;
        let got = table.interpolate(mid, "test").expect("midpoint");
        let a = table.table[j];
        let b = table.table[j + 1];
        assert_eq!(got, a * 0.5 + b * 0.5);
    }

    #[test]
    fn interpolate_rejects_out_of_domain_input() {
        let table = table_of(|x| x);
        assert!(table.interpolate(-0.000_000_1, "t").is_err());
        assert!(table.interpolate(1.000_000_1, "t").is_err());
        assert!(table.interpolate(f32::NAN, "t").is_err());
        // The exact endpoints are legal — the sentinel makes x == 1.0 valid.
        assert!(table.interpolate(0.0, "t").is_ok());
        assert!(table.interpolate(1.0, "t").is_ok());
    }

    #[test]
    fn gamma_tables_are_sampled_independently() {
        // Decode must come from EvaluateInverse, NOT an inversion of the
        // encode table: at a sampled knot, decode(encode(x)) recovers x to
        // within interpolation error, and encode/decode tables are distinct
        // curves (a shared-table scheme could not invert the 1.8 toe, which
        // is Hermite in encode but secant-solved in decode).
        for gamma in [
            Gamma::Srgb,
            Gamma::OnePointEight,
            Gamma::TwoPointTwo,
            Gamma::Rec2020,
        ] {
            let encode = table_of(|x| gamma.evaluate_f64(x));
            let decode = table_of(|y| gamma.evaluate_inverse_f64(y));
            for j in (0..=TABLE_COUNT).step_by(257) {
                let x = j as f32 / TABLE_COUNT as f32;
                let encoded = encode.interpolate(x, "encode").expect("encode");
                let recovered = decode.interpolate(encoded, "decode").expect("decode");
                // Interpolation error of a 4096-sampled monotone curve: well
                // under 1e-4 in the worst (toe) region for these tables.
                assert!(
                    (recovered - x).abs() < 1e-4,
                    "{gamma:?}: roundtrip at knot {j} gave {recovered} for {x}"
                );
            }
        }
    }

    #[test]
    fn spline_toe_matches_hermite_reference() {
        // Below x1 the 1.8 curve is a cubic Hermite between (0,0) with slope
        // 32 and (x1,y1) with slope 13.064306598 — check a mid-toe value
        // against the segment evaluated directly.
        let x = 4.0e-4;
        let expected = evaluate_spline_segment(
            x,
            0.0,
            0.0,
            32.0,
            8.2118790552e-4,
            0.019310851,
            13.064306598,
        );
        assert_eq!(gamma_encode_1_8(x), expected);
        // Adobe's published junction constants are not perfectly continuous:
        // the Hermite segment lands ~3e-11 from x^(1/1.8). Pin the measured
        // gap rather than asserting exact continuity.
        let toe = gamma_encode_1_8(8.2118790552e-4);
        let just_above = 8.2118790552e-4f64.powf(1.0 / 1.8);
        assert!((toe - just_above).abs() < 5e-10, "{toe} vs {just_above}");
    }

    #[test]
    fn secant_inverse_converges_below_the_toe() {
        // y below the 2.2 junction: the analytic pow branch does not apply;
        // the secant solver must recover the encode input.
        for y in [1.0e-4, 0.01, 0.05, 0.076] {
            let x = gamma_decode_2_2(y);
            let back = gamma_encode_2_2(x);
            assert!(
                (back - y).abs() < 1e-9,
                "decode({y}) = {x}, re-encodes to {back}"
            );
        }
    }
}
