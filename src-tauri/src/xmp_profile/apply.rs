//! Standalone renderer for Adobe `dng_rgb_table` look profiles.
//!
//! Scope of this milestone: the surrounding working representation is LINEAR
//! sRGB / D65, and only the Au/Cu metadata combination is supported
//! (`primaries_sRGB` / `gamma_sRGB` / `gamut_clip`). The processing sequence is
//! fixed by the architecture decision:
//!
//! ```text
//! linear sRGB
//!   -> clamp to [0,1]
//!   -> sRGB transfer encode
//!   -> tetrahedral 3D RGBTable interpolation
//!   -> clamp to [0,1]
//!   -> amount blend in encoded space
//!   -> clamp to [0,1]
//!   -> sRGB transfer decode
//!   -> linear sRGB
//! ```

use crate::xmp_profile::rgb_table::RgbTable;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Supported `primaries_enum` value: `primaries_sRGB`.
const SUPPORTED_PRIMARIES: u32 = 0;

/// Supported `gamma_enum` value: `gamma_sRGB`.
const SUPPORTED_GAMMA: u32 = 1;

/// Supported `gamut_enum` value: `gamut_clip`.
const SUPPORTED_GAMUT: u32 = 0;

const MIN_SIZE: usize = 2;
const MAX_SIZE: usize = 32;

// Test-only counter of how many times the full (O(size^3)) table validation
// runs. It is thread-local so tests running in parallel cannot observe each
// other's validations.
#[cfg(test)]
thread_local! {
    static FULL_TABLE_VALIDATIONS: AtomicUsize = const { AtomicUsize::new(0) };
}

/// Records one run of the full table validation.
///
/// Outside `cfg(test)` this expands to nothing, so non-test builds carry no
/// counting overhead.
#[inline(always)]
fn record_full_table_validation() {
    #[cfg(test)]
    FULL_TABLE_VALIDATIONS.with(|count| {
        count.fetch_add(1, Ordering::SeqCst);
    });
}

#[cfg(test)]
fn reset_full_table_validations() {
    FULL_TABLE_VALIDATIONS.with(|count| count.store(0, Ordering::SeqCst));
}

#[cfg(test)]
fn full_table_validations() -> usize {
    FULL_TABLE_VALIDATIONS.with(|count| count.load(Ordering::SeqCst))
}

/// A validated rendering context for a single Adobe RGBTable.
///
/// All expensive, profile-level work happens exactly once in
/// [`RgbTableApplyContext::new`]: metadata support, amount finiteness and
/// bounds, amount clamping, table size, checked node count, exact LUT length
/// and full LUT finiteness. [`RgbTableApplyContext::apply`] then performs only
/// per-pixel work, so rendering a whole image does not repeat the O(size^3)
/// validation for every pixel.
///
/// The table is borrowed, never copied.
pub(crate) struct RgbTableApplyContext<'a> {
    table: &'a RgbTable,
    effective_amount: f32,
}

impl<'a> RgbTableApplyContext<'a> {
    pub(crate) fn new(table: &'a RgbTable, amount: f32) -> Result<Self, String> {
        validate_render_metadata(table)?;

        if !amount.is_finite() {
            return Err("invalid RGBTable amount".to_string());
        }

        if !table.min_amount.is_finite()
            || !table.max_amount.is_finite()
            || !(0.0..=1.0).contains(&table.min_amount)
            || table.max_amount < 1.0
        {
            return Err("invalid RGBTable amount bounds".to_string());
        }

        let effective_amount = f64::from(amount).clamp(table.min_amount, table.max_amount) as f32;

        validate_table_structure(table)?;

        Ok(Self {
            table,
            effective_amount,
        })
    }

    /// Renders one pixel. Performs no profile-level validation.
    pub(crate) fn apply(&self, rgb: [f32; 3]) -> Result<[f32; 3], String> {
        // gamut_clip: any excursion outside [0,1] is discarded before the table.
        let mut encoded = [0.0f32; 3];

        for channel in 0..3 {
            let value = rgb[channel];

            if !value.is_finite() {
                return Err("RGBTable input contains non-finite value".to_string());
            }

            encoded[channel] = srgb_encode(value.clamp(0.0, 1.0));
        }

        let lut = interpolate(self.table, encoded)?;

        let mut out = [0.0f32; 3];

        for channel in 0..3 {
            let lut_value = lut[channel].clamp(0.0, 1.0);
            let input = encoded[channel];

            let blended = (input + self.effective_amount * (lut_value - input)).clamp(0.0, 1.0);

            out[channel] = srgb_decode(blended);
        }

        Ok(out)
    }
}

/// Convenience wrapper: validates the table, renders a single pixel.
///
/// For more than a handful of pixels prefer [`RgbTableApplyContext`], which
/// validates once and then renders many pixels.
pub(crate) fn apply_rgb_table(
    rgb: [f32; 3],
    table: &RgbTable,
    amount: f32,
) -> Result<[f32; 3], String> {
    RgbTableApplyContext::new(table, amount)?.apply(rgb)
}

fn validate_render_metadata(table: &RgbTable) -> Result<(), String> {
    if table.color_space != SUPPORTED_PRIMARIES {
        return Err(format!(
            "unsupported RGBTable primaries: {}",
            table.color_space
        ));
    }

    if table.gamma != SUPPORTED_GAMMA {
        return Err(format!("unsupported RGBTable gamma: {}", table.gamma));
    }

    if table.gamut != SUPPORTED_GAMUT {
        return Err(format!("unsupported RGBTable gamut: {}", table.gamut));
    }

    Ok(())
}

fn validate_table_structure(table: &RgbTable) -> Result<(), String> {
    record_full_table_validation();

    let size = table.size;

    if !(MIN_SIZE..=MAX_SIZE).contains(&size) {
        return Err(format!("unsupported RGBTable size: {size}"));
    }

    let node_count = size
        .checked_mul(size)
        .and_then(|nodes| nodes.checked_mul(size))
        .ok_or_else(|| "RGBTable node count overflows usize".to_string())?;

    if table.values.len() != node_count {
        return Err(format!(
            "malformed RGBTable: expected {node_count} nodes, got {}",
            table.values.len()
        ));
    }

    // Every node of the table must be finite, not just the corners a particular
    // lookup happens to sample: a malformed table must be rejected before any
    // pixel is rendered.
    for node in &table.values {
        for value in node {
            if !value.is_finite() {
                return Err("RGBTable contains non-finite sample".to_string());
            }
        }
    }

    Ok(())
}

/// sRGB transfer encode (linear -> encoded). The input is expected to already
/// be clamped to [0,1].
fn srgb_encode(linear: f32) -> f32 {
    if linear <= 0.0031308 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB transfer decode (encoded -> linear). The input is expected to already
/// be clamped to [0,1].
fn srgb_decode(encoded: f32) -> f32 {
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear node index for a corner of the table.
///
/// The stored ordering is B fastest, G next, R slowest.
fn node_index(size: usize, r_index: usize, g_index: usize, b_index: usize) -> usize {
    r_index * size * size + g_index * size + b_index
}

/// Map an encoded coordinate to the table cell base index and its fraction.
fn cell_coordinate(encoded: f32, size: usize) -> (usize, f32) {
    let scale = (size - 1) as f32;
    let max_base = size - 2;

    let scaled = encoded * scale;

    let base = (scaled as i32).clamp(0, max_base as i32) as usize;

    (base, scaled - base as f32)
}

/// Tetrahedral interpolation of the 3D table at an encoded coordinate.
fn interpolate(table: &RgbTable, encoded: [f32; 3]) -> Result<[f32; 3], String> {
    let size = table.size;

    let (r_base, fr) = cell_coordinate(encoded[0], size);
    let (g_base, fg) = cell_coordinate(encoded[1], size);
    let (b_base, fb) = cell_coordinate(encoded[2], size);

    let r_high = r_base + 1;
    let g_high = g_base + 1;
    let b_high = b_base + 1;

    let c000 = table.values[node_index(size, r_base, g_base, b_base)];
    let c100 = table.values[node_index(size, r_high, g_base, b_base)];
    let c010 = table.values[node_index(size, r_base, g_high, b_base)];
    let c001 = table.values[node_index(size, r_base, g_base, b_high)];
    let c110 = table.values[node_index(size, r_high, g_high, b_base)];
    let c101 = table.values[node_index(size, r_high, g_base, b_high)];
    let c011 = table.values[node_index(size, r_base, g_high, b_high)];
    let c111 = table.values[node_index(size, r_high, g_high, b_high)];

    let mut out = [0.0f32; 3];

    for channel in 0..3 {
        let base = c000[channel];

        // Six tetrahedral cases over the orderings of (fr, fg, fb).
        out[channel] = if fr >= fg && fg >= fb {
            base + fr * (c100[channel] - base)
                + fg * (c110[channel] - c100[channel])
                + fb * (c111[channel] - c110[channel])
        } else if fr >= fb && fb >= fg {
            base + fr * (c100[channel] - base)
                + fb * (c101[channel] - c100[channel])
                + fg * (c111[channel] - c101[channel])
        } else if fb >= fr && fr >= fg {
            base + fb * (c001[channel] - base)
                + fr * (c101[channel] - c001[channel])
                + fg * (c111[channel] - c101[channel])
        } else if fg >= fr && fr >= fb {
            base + fg * (c010[channel] - base)
                + fr * (c110[channel] - c010[channel])
                + fb * (c111[channel] - c110[channel])
        } else if fg >= fb && fb >= fr {
            base + fg * (c010[channel] - base)
                + fb * (c011[channel] - c010[channel])
                + fr * (c111[channel] - c011[channel])
        } else {
            base + fb * (c001[channel] - base)
                + fg * (c011[channel] - c001[channel])
                + fr * (c111[channel] - c011[channel])
        };
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY_2X2X2: [[f32; 3]; 8] = [
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 1.0, 0.0],
        [0.0, 1.0, 1.0],
        [1.0, 0.0, 0.0],
        [1.0, 0.0, 1.0],
        [1.0, 1.0, 0.0],
        [1.0, 1.0, 1.0],
    ];

    const CONSTANT_2X2X2: [[f32; 3]; 8] = [[0.25, 0.5, 0.75]; 8];

    fn make_table(
        size: usize,
        values: Vec<[f32; 3]>,
        min_amount: f64,
        max_amount: f64,
    ) -> RgbTable {
        RgbTable {
            size,
            values,
            color_space: 0,
            gamma: 1,
            gamut: 0,
            min_amount,
            max_amount,
        }
    }

    fn table_2x2x2(values: [[f32; 3]; 8], min_amount: f64, max_amount: f64) -> RgbTable {
        make_table(2, values.to_vec(), min_amount, max_amount)
    }

    fn assert_close(actual: f32, expected: f32, epsilon: f32, context: &str) {
        assert!(
            (actual - expected).abs() < epsilon,
            "{context}: expected {expected}, got {actual}"
        );
    }

    fn assert_rgb_close(actual: [f32; 3], expected: [f32; 3], epsilon: f32, context: &str) {
        for channel in 0..3 {
            assert_close(
                actual[channel],
                expected[channel],
                epsilon,
                &format!("{context} channel {channel}"),
            );
        }
    }

    /// CASE 1 (fr >= fg >= fb) evaluated with the hand-written formula.
    fn case1(
        c000: [f32; 3],
        c100: [f32; 3],
        c110: [f32; 3],
        c111: [f32; 3],
        fr: f32,
        fg: f32,
        fb: f32,
    ) -> [f32; 3] {
        let mut out = [0.0f32; 3];
        for channel in 0..3 {
            out[channel] = c000[channel]
                + fr * (c100[channel] - c000[channel])
                + fg * (c110[channel] - c100[channel])
                + fb * (c111[channel] - c110[channel]);
        }
        out
    }

    // ---------------------------------------------------------------- sRGB

    #[test]
    fn srgb_encode_decode_round_trip() {
        for value in [0.0f32, 0.001, 0.0031308, 0.01, 0.18, 0.5, 1.0] {
            let decoded = srgb_decode(srgb_encode(value));
            assert_close(decoded, value, 1e-6, &format!("round trip of {value}"));
        }
    }

    // ------------------------------------------------------------- identity

    #[test]
    fn identity_lut_preserves_linear_rgb() {
        let table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        let input = [0.18f32, 0.5, 0.9];

        let output = apply_rgb_table(input, &table, 1.0).expect("identity table should apply");
        assert_rgb_close(output, input, 1e-5, "identity LUT");
    }

    // --------------------------------------------------------------- amount

    #[test]
    fn amount_zero_preserves_input() {
        // A deliberately non-identity table, so that a bug which ignores the
        // amount would produce a visibly different result.
        let mut values = CONSTANT_2X2X2;
        values[0] = [0.9, 0.1, 0.4];
        let table = table_2x2x2(values, 0.0, 1.0);

        let input = [0.18f32, 0.5, 0.9];
        let output = apply_rgb_table(input, &table, 0.0).expect("amount 0 should apply");

        assert_rgb_close(output, input, 1e-6, "amount 0");
    }

    #[test]
    fn amount_one_uses_full_lut_result() {
        let table = table_2x2x2(CONSTANT_2X2X2, 0.0, 1.0);

        let input = [0.1f32, 0.2, 0.3];
        let output = apply_rgb_table(input, &table, 1.0).expect("amount 1 should apply");

        let expected = [srgb_decode(0.25), srgb_decode(0.5), srgb_decode(0.75)];
        assert_rgb_close(output, expected, 1e-6, "amount 1");
    }

    #[test]
    fn amount_above_one_extrapolates_then_clamps() {
        // Constant encoded LUT; the blue channel is high enough that
        // extrapolating at 1.5 overshoots 1.0 and must be clamped.
        let constant = [[0.25f32, 0.5, 1.0]; 8];
        let table = table_2x2x2(constant, 0.0, 1.5);

        let input = [0.1f32, 0.2, 0.3];
        let output = apply_rgb_table(input, &table, 1.5).expect("amount 1.5 should apply");

        let encoded_input = [
            srgb_encode(input[0]),
            srgb_encode(input[1]),
            srgb_encode(input[2]),
        ];
        let lut = [0.25f32, 0.5, 1.0];
        let amount = 1.5f32;

        let mut expected = [0.0f32; 3];
        for channel in 0..3 {
            let blended = encoded_input[channel] + amount * (lut[channel] - encoded_input[channel]);
            expected[channel] = srgb_decode(blended.clamp(0.0, 1.0));
        }

        assert_rgb_close(output, expected, 1e-6, "amount above one");
        // The blue channel must actually have been clamped, otherwise this test
        // would not exercise the clamp path.
        assert!(
            encoded_input[2] + amount * (lut[2] - encoded_input[2]) > 1.0,
            "test setup should overshoot 1.0 in the blue channel"
        );
    }

    #[test]
    fn amount_is_clamped_to_table_max() {
        let table = table_2x2x2(CONSTANT_2X2X2, 0.0, 1.5);

        let at_max = apply_rgb_table([0.1, 0.2, 0.3], &table, 1.5).expect("amount 1.5");
        let above_max = apply_rgb_table([0.1, 0.2, 0.3], &table, 9.0).expect("amount 9.0");

        assert_rgb_close(above_max, at_max, 1e-6, "amount clamp to max");
    }

    #[test]
    fn amount_is_clamped_to_table_min() {
        let table = table_2x2x2(CONSTANT_2X2X2, 0.25, 1.5);

        let at_min = apply_rgb_table([0.1, 0.2, 0.3], &table, 0.25).expect("amount 0.25");
        let below_min = apply_rgb_table([0.1, 0.2, 0.3], &table, -10.0).expect("amount -10.0");

        assert_rgb_close(below_min, at_min, 1e-6, "amount clamp to min");
    }

    // ------------------------------------------------------------- clipping

    #[test]
    fn gamut_clip_clamps_linear_input_before_encoding() {
        let table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);

        let output =
            apply_rgb_table([-1.0, 0.5, 3.0], &table, 1.0).expect("gamut clip should apply");

        assert_rgb_close(output, [0.0, 0.5, 1.0], 1e-5, "gamut clip");
    }

    // -------------------------------------------------------- interpolation

    #[test]
    fn tetrahedral_interpolation_matches_known_cube() {
        // index = r * 4 + g * 2 + b  (B fastest, R slowest)
        let values = [
            [0.00f32, 0.00, 0.00], // C000
            [0.20, 0.30, 0.70],    // C001
            [0.10, 0.80, 0.30],    // C010
            [0.40, 0.10, 0.90],    // C011
            [0.90, 0.20, 0.10],    // C100
            [0.30, 0.90, 0.20],    // C101
            [0.70, 0.60, 0.40],    // C110
            [0.60, 0.50, 1.00],    // C111
        ];
        let table = table_2x2x2(values, 0.0, 1.0);

        // Encoded coordinates with unequal fractions: fr = 0.8, fg = 0.5, fb = 0.2
        // which satisfies fr >= fg >= fb, i.e. CASE 1.
        let fr = 0.8f32;
        let fg = 0.5f32;
        let fb = 0.2f32;

        let linear_input = [srgb_decode(fr), srgb_decode(fg), srgb_decode(fb)];
        let output = apply_rgb_table(linear_input, &table, 1.0).expect("known cube should apply");

        let expected_encoded = case1(values[0], values[4], values[6], values[7], fr, fg, fb);
        let expected = [
            srgb_decode(expected_encoded[0]),
            srgb_decode(expected_encoded[1]),
            srgb_decode(expected_encoded[2]),
        ];
        assert_rgb_close(output, expected, 1e-6, "tetrahedral CASE 1");

        // Ordinary trilinear, computed here in the test only, must differ
        // meaningfully - otherwise this LUT would not distinguish the two
        // interpolation methods.
        let w000 = (1.0 - fr) * (1.0 - fg) * (1.0 - fb);
        let w100 = fr * (1.0 - fg) * (1.0 - fb);
        let w010 = (1.0 - fr) * fg * (1.0 - fb);
        let w001 = (1.0 - fr) * (1.0 - fg) * fb;
        let w110 = fr * fg * (1.0 - fb);
        let w101 = fr * (1.0 - fg) * fb;
        let w011 = (1.0 - fr) * fg * fb;
        let w111 = fr * fg * fb;
        let weights = [w000, w100, w010, w001, w110, w101, w011, w111];

        let mut trilinear = [0.0f32; 3];
        for (weight, corner) in weights.iter().zip(values.iter()) {
            for channel in 0..3 {
                trilinear[channel] += weight * corner[channel];
            }
        }

        let max_difference = (0..3)
            .map(|channel| (trilinear[channel] - expected_encoded[channel]).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_difference > 1e-3,
            "tetrahedral and trilinear should differ on this cube, max difference was {max_difference}"
        );
    }

    #[test]
    fn lut_index_order_is_b_fastest() {
        // Eight unique, recognizable corner values.
        let values = [
            [0.01f32, 0.02, 0.03], // r0 g0 b0
            [0.11, 0.12, 0.13],    // r0 g0 b1
            [0.21, 0.22, 0.23],    // r0 g1 b0
            [0.31, 0.32, 0.33],    // r0 g1 b1
            [0.41, 0.42, 0.43],    // r1 g0 b0
            [0.51, 0.52, 0.53],    // r1 g0 b1
            [0.61, 0.62, 0.63],    // r1 g1 b0
            [0.71, 0.72, 0.73],    // r1 g1 b1
        ];
        let table = table_2x2x2(values, 0.0, 1.0);

        // Testing the private helper directly keeps the exact encoded
        // coordinates visible to the test.
        for (coordinate_index, (r, g, b)) in [
            (0usize, 0u32, 0u32),
            (0, 0, 1),
            (0, 1, 0),
            (0, 1, 1),
            (1, 0, 0),
            (1, 0, 1),
            (1, 1, 0),
            (1, 1, 1),
        ]
        .iter()
        .enumerate()
        {
            let encoded = [*r as f32, *g as f32, *b as f32];
            let actual = interpolate(&table, encoded).expect("corner lookup");

            assert_rgb_close(
                actual,
                values[coordinate_index],
                1e-6,
                &format!("corner ({r},{g},{b})"),
            );
        }
    }

    // ---------------------------------------------------------------- errors

    #[test]
    fn rejects_unsupported_primaries() {
        let mut table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        table.color_space = 2;

        let error = apply_rgb_table([0.5, 0.5, 0.5], &table, 1.0)
            .expect_err("unsupported primaries must be rejected");
        assert_eq!(error, "unsupported RGBTable primaries: 2");
    }

    #[test]
    fn rejects_unsupported_gamma() {
        let mut table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        table.gamma = 0;

        let error = apply_rgb_table([0.5, 0.5, 0.5], &table, 1.0)
            .expect_err("unsupported gamma must be rejected");
        assert_eq!(error, "unsupported RGBTable gamma: 0");
    }

    #[test]
    fn rejects_unsupported_gamut() {
        let mut table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        table.gamut = 1;

        let error = apply_rgb_table([0.5, 0.5, 0.5], &table, 1.0)
            .expect_err("unsupported gamut must be rejected");
        assert_eq!(error, "unsupported RGBTable gamut: 1");
    }

    #[test]
    fn rejects_non_finite_input() {
        let table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);

        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let error = apply_rgb_table([bad, 0.5, 0.5], &table, 1.0)
                .expect_err("non-finite input must be rejected");
            assert_eq!(error, "RGBTable input contains non-finite value");
        }
    }

    #[test]
    fn rejects_non_finite_amount() {
        let table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);

        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let error = apply_rgb_table([0.5, 0.5, 0.5], &table, bad)
                .expect_err("non-finite amount must be rejected");
            assert_eq!(error, "invalid RGBTable amount");
        }
    }

    #[test]
    fn rejects_invalid_amount_bounds() {
        let input = [0.5f32, 0.5, 0.5];

        // max_amount < 1.0
        let low_max = table_2x2x2(IDENTITY_2X2X2, 0.0, 0.8);
        assert_eq!(
            apply_rgb_table(input, &low_max, 1.0).expect_err("max below 1.0"),
            "invalid RGBTable amount bounds"
        );

        // min_amount > 1.0
        let high_min = table_2x2x2(IDENTITY_2X2X2, 1.5, 2.0);
        assert_eq!(
            apply_rgb_table(input, &high_min, 1.0).expect_err("min above 1.0"),
            "invalid RGBTable amount bounds"
        );

        // negative min_amount
        let negative_min = table_2x2x2(IDENTITY_2X2X2, -0.5, 1.5);
        assert_eq!(
            apply_rgb_table(input, &negative_min, 1.0).expect_err("negative min"),
            "invalid RGBTable amount bounds"
        );

        // non-finite bounds
        let nan_min = table_2x2x2(IDENTITY_2X2X2, f64::NAN, 1.5);
        assert_eq!(
            apply_rgb_table(input, &nan_min, 1.0).expect_err("NaN min"),
            "invalid RGBTable amount bounds"
        );
    }

    #[test]
    fn rejects_malformed_lut_length() {
        let mut values = IDENTITY_2X2X2.to_vec();
        values.pop();
        let table = make_table(2, values, 0.0, 1.0);

        assert!(apply_rgb_table([0.5, 0.5, 0.5], &table, 1.0).is_err());
    }

    #[test]
    fn rejects_non_finite_lut_sample() {
        let mut values = IDENTITY_2X2X2;
        values[7] = [f32::NAN, 0.0, 0.0];
        let table = table_2x2x2(values, 0.0, 1.0);

        let error = apply_rgb_table([0.18, 0.5, 0.9], &table, 1.0)
            .expect_err("non-finite sample must be rejected");
        assert_eq!(error, "RGBTable contains non-finite sample");
    }

    #[test]
    fn rejects_non_finite_unsampled_lut_node() {
        // 3x3x3 table: node index = r * 9 + g * 3 + b
        let mut values = vec![[0.5f32, 0.5, 0.5]; 27];

        for r in 0..3 {
            for g in 0..3 {
                for b in 0..3 {
                    values[r * 9 + g * 3 + b] = [r as f32 / 2.0, g as f32 / 2.0, b as f32 / 2.0];
                }
            }
        }

        // The far corner at coordinate (2, 2, 2).
        let poisoned_index = 2 * 9 + 2 * 3 + 2;
        assert_eq!(poisoned_index, 26);

        // Linear input that encodes to roughly [0.24, 0.35, 0.43] -> scaled by
        // (size - 1) = 2 that is [0.48, 0.70, 0.86], so every cell base is 0 and
        // the lookup only ever samples the cube spanning indices 0..1 (highest
        // touched node is (1, 1, 1) = index 13). Node 26 is never sampled.
        let input = [0.05f32, 0.10, 0.15];

        let mut poisoned = values.clone();
        poisoned[poisoned_index] = [f32::NAN, 0.5, 0.5];
        let poisoned_table = make_table(3, poisoned, 0.0, 1.0);

        let error = apply_rgb_table(input, &poisoned_table, 1.0)
            .expect_err("non-finite unsampled node must be rejected");
        assert_eq!(error, "RGBTable contains non-finite sample");

        // Sanity: the same input renders fine once the unsampled node is finite,
        // proving the failure is caused by the NaN itself and not by the table
        // shape or by the chosen input falling outside the table.
        let mut clean = values;
        clean[poisoned_index] = [1.0, 1.0, 1.0];
        let clean_table = make_table(3, clean, 0.0, 1.0);
        assert!(
            apply_rgb_table(input, &clean_table, 1.0).is_ok(),
            "the clean table must render, otherwise this test proves nothing"
        );
    }

    #[test]
    fn context_validates_table_only_once_for_multiple_pixels() {
        reset_full_table_validations();

        let table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        let context = RgbTableApplyContext::new(&table, 1.0).expect("valid context");

        assert_eq!(
            full_table_validations(),
            1,
            "constructing the context must validate the table exactly once"
        );

        for index in 0..200 {
            let x = index as f32 / 200.0;
            let output = context.apply([x, 0.5, 0.25]).expect("apply");
            for channel in output {
                assert!(channel.is_finite());
            }
        }

        assert_eq!(
            full_table_validations(),
            1,
            "applying many pixels must not revalidate the table"
        );
    }

    #[test]
    fn context_apply_matches_apply_rgb_table() {
        let inputs = [
            [0.0f32, 0.0, 0.0],
            [0.18, 0.5, 0.9],
            [1.0, 1.0, 1.0],
            [-0.5, 0.25, 1.5],
            [0.05, 0.10, 0.15],
            [0.9, 0.4, 0.1],
        ];

        // A non-identity table plus every amount regime the renderer supports,
        // including extrapolation above 1 and clamping at the bounds.
        let cases: [([[f32; 3]; 8], f64, f64, f32); 5] = [
            (IDENTITY_2X2X2, 0.0, 1.0, 1.0),
            (IDENTITY_2X2X2, 0.0, 1.5, 1.5),
            (CONSTANT_2X2X2, 0.0, 1.0, 0.0),
            (CONSTANT_2X2X2, 0.25, 1.5, 0.6),
            (
                [
                    [0.00, 0.00, 0.00],
                    [0.20, 0.30, 0.70],
                    [0.10, 0.80, 0.30],
                    [0.40, 0.10, 0.90],
                    [0.90, 0.20, 0.10],
                    [0.30, 0.90, 0.20],
                    [0.70, 0.60, 0.40],
                    [0.60, 0.50, 1.00],
                ],
                0.0,
                1.5,
                1.5,
            ),
        ];

        for (values, min_amount, max_amount, amount) in cases {
            let table = table_2x2x2(values, min_amount, max_amount);
            let context = RgbTableApplyContext::new(&table, amount).expect("valid context");

            for input in inputs {
                let from_context = context.apply(input).expect("context apply");
                let from_wrapper = apply_rgb_table(input, &table, amount).expect("wrapper apply");

                for channel in 0..3 {
                    assert!(
                        (from_context[channel] - from_wrapper[channel]).abs() < 1e-7,
                        "input {input:?} channel {channel}: context {} vs wrapper {}",
                        from_context[channel],
                        from_wrapper[channel]
                    );
                }
            }
        }
    }

    #[test]
    fn context_constructor_rejects_invalid_tables_without_touching_pixels() {
        reset_full_table_validations();

        let mut table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        table.color_space = 2;

        let result = RgbTableApplyContext::new(&table, 1.0);
        assert_eq!(
            result.err().expect("unsupported primaries"),
            "unsupported RGBTable primaries: 2"
        );

        // A rejected table must not even reach the full-table scan.
        assert_eq!(
            full_table_validations(),
            0,
            "invalid metadata should be rejected before the full table scan"
        );
    }
}
