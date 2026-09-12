//! Standalone renderer for Adobe `dng_rgb_table` look profiles.
//!
//! The surrounding working representation is LINEAR sRGB / D65. Every legal
//! `(primaries, gamma, gamut)` combination is supported; the exact processing
//! sequence is the one proven from `dng_reference.cpp:3476-3860`
//! (`RefRGBtoRGBTable3D`):
//!
//! ```text
//! linear sRGB (working space)
//!   -> matrix: working -> table primaries    (SKIPPED for primaries_sRGB)
//!   -> clamp to [0,1]
//!   -> if gamut_extend: record delta = unclamped - clamped (linear table primaries)
//!   -> gamma ENCODE
//!   -> tetrahedral 3D RGBTable interpolation
//!   -> AMOUNT BLEND (encoded domain, BEFORE the decode)
//!   -> clamp to [0,1]
//!   -> gamma DECODE
//!   -> if gamut_extend: add delta back
//!   -> matrix: table primaries -> working    (SKIPPED for primaries_sRGB)
//!   -> clamp to [0,1]
//! ```
//!
//! The pure color-space math lives in [`super::color_space`]; this module owns
//! the validated per-profile context, the table validation and the per-pixel
//! pipeline.
//!
//! `primaries_sRGB / gamma_sRGB / gamut_clip` stays BIT-IDENTICAL to the 5A
//! renderer: the sRGB primaries hop is an identity shortcut that **skips the
//! matrix multiply entirely** (it never multiplies by a computed near-identity
//! matrix), and the sRGB transfer pair is the shared [`super::srgb_transfer`]
//! one. The trailing clamp is a provable no-op there (see [`Self::apply`]).

use crate::xmp_profile::rgb_table::RgbTable;

use super::color_space::{self, Gamma, Gamut, Primaries, PrimariesTransform};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

const MIN_SIZE: usize = 2;
/// Adobe's read/write/import ceiling for a 3-D table
/// (`dng_rgb_table::kMaxDivisions3D_InMemory`, `dng_big_table.h:623`). The
/// renderer accepts the same 2..=64 range the wire decoder does; the smaller
/// `kMaxDivisions3D` (32) is only Adobe's default resample resolution.
const MAX_SIZE: usize = 64;

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
/// [`RgbTableApplyContext::new`]: metadata support (the three enums are parsed
/// here, with no fallback), amount finiteness and bounds, amount clamping, the
/// derived primaries transform, table size, checked node count, exact LUT
/// length and full LUT finiteness. [`RgbTableApplyContext::apply`] then performs
/// only per-pixel work, so rendering a whole image does not repeat the O(size^3)
/// validation for every pixel.
///
/// The table is borrowed, never copied.
pub(crate) struct RgbTableApplyContext<'a> {
    table: &'a RgbTable,
    effective_amount: f32,
    primaries: PrimariesTransform,
    gamma: Gamma,
    gamut: Gamut,
}

impl<'a> RgbTableApplyContext<'a> {
    pub(crate) fn new(table: &'a RgbTable, amount: f32) -> Result<Self, String> {
        // Unsupported enums fail loudly, BEFORE any other work: this mirrors the
        // 5A ordering (metadata first) so a bad metadata value never reaches the
        // O(size^3) full-table scan.
        let primaries = Primaries::from_enum(table.color_space)?;
        let gamma = Gamma::from_enum(table.gamma)?;
        let gamut = Gamut::from_enum(table.gamut)?;

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
            primaries: PrimariesTransform::new(primaries),
            gamma,
            gamut,
        })
    }

    /// Renders one pixel. Performs no profile-level validation.
    pub(crate) fn apply(&self, rgb: [f32; 3]) -> Result<[f32; 3], String> {
        for value in rgb {
            if !value.is_finite() {
                return Err("RGBTable input contains non-finite value".to_string());
            }
        }

        // (a) matrix: working space -> linear table primaries. For primaries_sRGB
        // this is the identity shortcut, so the multiply is skipped outright.
        let linear = self.primaries.encode(rgb);

        // (b) clamp to [0,1] (BOTH gamut modes) and (b') the extend-mode delta,
        // recorded in linear table primaries.
        //
        // Deliberate generalisation of Adobe's `hasMatrix` coupling: the SDK only
        // runs the clamp and the gamutDelta when `hasMatrix` is true
        // (`dng_reference.cpp:3595-3622`), and its reference build sets
        // `fNeedMatrix = false` for `primaries_ProPhoto` by leaving `space = NULL`
        // (`dng_big_table.cpp:5236-5242`) — because ADOBE'S OWN WORKING SPACE IS
        // LINEAR ProPhoto, so a ProPhoto table has no device-to-table hop. Once-Lab
        // works in linear sRGB/D65, so ProPhoto genuinely needs a matrix and
        // `hasMatrix` is effectively always true for us; clamping + extending
        // uniformly is the correct spec-model behaviour, not a mis-copy of Adobe's
        // null-space fast path. See §I of `5c-support-matrix.md`.
        let (clamped, delta) = color_space::gamut_clamp(linear, self.gamut);

        // (c) gamma ENCODE.
        let mut encoded = [0.0f32; 3];
        for channel in 0..3 {
            encoded[channel] = self.gamma.encode(clamped[channel]);
        }

        // (d) tetrahedral 3D LUT.
        let lut = interpolate(self.table, encoded)?;

        // (e) AMOUNT BLEND in the ENCODED domain (BEFORE the decode), then
        // (f) clamp to [0,1].
        let mut blended = [0.0f32; 3];
        for channel in 0..3 {
            let lut_value = lut[channel].clamp(0.0, 1.0);
            let input = encoded[channel];
            blended[channel] =
                (input + self.effective_amount * (lut_value - input)).clamp(0.0, 1.0);
        }

        // (g) gamma DECODE.
        let mut decoded = [
            self.gamma.decode(blended[0]),
            self.gamma.decode(blended[1]),
            self.gamma.decode(blended[2]),
        ];

        // (h) gamut_extend re-adds the recorded excursion (in linear table
        // primaries). gamut_clip discards it, so nothing is added.
        if self.gamut == Gamut::Extend {
            decoded[0] += delta[0];
            decoded[1] += delta[1];
            decoded[2] += delta[2];
        }

        // (i) matrix: table primaries -> working space (identity shortcut for sRGB).
        let out = self.primaries.decode(decoded);

        // Final SDR clip. For the sRGB/sRGB/clip path this is a NO-OP on the bits:
        // the input to `srgb_decode` was clamped to [0,1], and `srgb_decode` is
        // monotone with `decode(0) = 0`, `decode(1) = 1`, so every decoded channel
        // already lies in [0,1] and `clamp` returns it unchanged.
        Ok([
            out[0].clamp(0.0, 1.0),
            out[1].clamp(0.0, 1.0),
            out[2].clamp(0.0, 1.0),
        ])
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
    use crate::xmp_profile::color_space::{ALL_GAMMAS, ALL_GAMUTS, ALL_PRIMARIES};
    use crate::xmp_profile::srgb_transfer::{srgb_decode, srgb_encode};

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

    /// Residual tolerated for an identity RGBTable rendered under ANY supported
    /// (primaries, gamma, gamut) combination: the transfer round trip and the
    /// 3x3 inverse product are not bit-exact in f32.
    const IDENTITY_COMBINATION_TOLERANCE: f32 = 1e-4;

    /// A change big enough to prove a synthetic profile genuinely altered a
    /// probe (rather than differing by float noise).
    const MATERIAL_DIFFERENCE: f32 = 1e-3;

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

    /// A 2x2x2 table with explicit metadata enums, for the exhaustive tests.
    fn table_with_enums(
        values: [[f32; 3]; 8],
        color_space: u32,
        gamma: u32,
        gamut: u32,
    ) -> RgbTable {
        RgbTable {
            size: 2,
            values: values.to_vec(),
            color_space,
            gamma,
            gamut,
            min_amount: 0.0,
            max_amount: 1.0,
        }
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

    #[test]
    fn extreme_and_negative_pixels_render_finitely_in_both_gamut_modes() {
        // Every value is finite, so each must render a finite, in-range pixel in
        // BOTH gamut modes and for both an identity and a non-identity table --
        // no panic, no NaN/Inf, no out-of-range escape.
        let inputs = [
            [-1.0f32, -2.0, -3.0],
            [-1.0e30, -1.0e30, -1.0e30],
            [1.0e30, 1.0e30, 1.0e30],
            [f32::MIN, f32::MIN, f32::MIN],
            [f32::MAX, f32::MAX, f32::MAX],
        ];

        for (label, values) in [("identity", IDENTITY_2X2X2), ("constant", CONSTANT_2X2X2)] {
            for gamut in [0u32, 1u32] {
                let table = table_with_enums(values, 0, 1, gamut);
                let context = RgbTableApplyContext::new(&table, 1.0).expect("context");

                for input in inputs {
                    let output = context
                        .apply(input)
                        .unwrap_or_else(|error| panic!("{label}/{gamut} {input:?}: {error}"));

                    for channel in output {
                        assert!(
                            channel.is_finite(),
                            "{label}/{gamut} input {input:?} produced non-finite {output:?}"
                        );
                        assert!(
                            (0.0..=1.0).contains(&channel),
                            "{label}/{gamut} input {input:?} left [0,1]: {output:?}"
                        );
                    }
                }
            }
        }

        // Non-vacuity: with a non-identity table the two gamut modes genuinely
        // differ on a saturated extreme, so the finite/in-range checks above are
        // exercising both real branches rather than a shared clamp.
        let extreme = [1.0e30f32, 1.0e30, 1.0e30];
        let clip = RgbTableApplyContext::new(&table_with_enums(CONSTANT_2X2X2, 0, 1, 0), 1.0)
            .expect("clip")
            .apply(extreme)
            .expect("clip apply");
        let extend = RgbTableApplyContext::new(&table_with_enums(CONSTANT_2X2X2, 0, 1, 1), 1.0)
            .expect("extend")
            .apply(extreme)
            .expect("extend apply");
        assert!(
            max_channel_delta(clip, extend) > MATERIAL_DIFFERENCE,
            "the two gamut modes must differ on a saturated extreme, got {clip:?} vs {extend:?}"
        );
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
        // `primaries_enum` 5 is out of range (0..=4 are supported). It must fail
        // with a clear, contextual error and NEVER fall back to sRGB.
        let mut table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        table.color_space = 5;

        let error = apply_rgb_table([0.5, 0.5, 0.5], &table, 1.0)
            .expect_err("unsupported primaries must be rejected");
        assert_eq!(error, "unsupported RGBTable primaries: 5");

        // Non-vacuity: value 2 (ProPhoto) is the wide-gamut primaries that 5B
        // newly supports, so it must be ACCEPTED - proving this test fails for
        // the enum bound and not for some unrelated table problem.
        let mut prophoto = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        prophoto.color_space = 2;
        assert!(
            apply_rgb_table([0.5, 0.5, 0.5], &prophoto, 1.0).is_ok(),
            "ProPhoto primaries must now be supported"
        );
    }

    #[test]
    fn rejects_unsupported_gamma() {
        // `gamma_enum` 9 is out of range (0..=4 are supported).
        let mut table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        table.gamma = 9;

        let error = apply_rgb_table([0.5, 0.5, 0.5], &table, 1.0)
            .expect_err("unsupported gamma must be rejected");
        assert_eq!(error, "unsupported RGBTable gamma: 9");

        // Non-vacuity: gamma 0 (Linear) is now supported and must be accepted.
        let mut linear = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        linear.gamma = 0;
        assert!(
            apply_rgb_table([0.5, 0.5, 0.5], &linear, 1.0).is_ok(),
            "Linear gamma must now be supported"
        );
    }

    #[test]
    fn rejects_unsupported_gamut() {
        // `gamut_enum` 7 is out of range (0..=1 are supported).
        let mut table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        table.gamut = 7;

        let error = apply_rgb_table([0.5, 0.5, 0.5], &table, 1.0)
            .expect_err("unsupported gamut must be rejected");
        assert_eq!(error, "unsupported RGBTable gamut: 7");

        // Non-vacuity: gamut 1 (extend) is now supported and must be accepted.
        let mut extend = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);
        extend.gamut = 1;
        assert!(
            apply_rgb_table([0.5, 0.5, 0.5], &extend, 1.0).is_ok(),
            "gamut_extend must now be supported"
        );
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
    fn rejects_non_finite_input_on_every_channel() {
        let table = table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0);

        for channel in 0..3 {
            for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut input = [0.5f32, 0.5, 0.5];
                input[channel] = bad;

                let error = apply_rgb_table(input, &table, 1.0)
                    .expect_err("non-finite input on any channel must be rejected");
                assert_eq!(
                    error, "RGBTable input contains non-finite value",
                    "channel {channel} with {bad}"
                );
            }
        }

        // Non-vacuity: the same probes render when finite.
        assert!(apply_rgb_table([0.5, 0.5, 0.5], &table, 1.0).is_ok());
    }

    #[test]
    fn rejects_out_of_range_table_size() {
        // 2..=64 are the supported sizes (Adobe's kMaxDivisions3D_InMemory); 0, 1
        // and 65 are not. The error names the size and there is deliberately NO
        // fallback to a default size.
        for size in [0usize, 1, 65] {
            let table = make_table(size, vec![[0.5f32, 0.5, 0.5]; 8], 0.0, 1.0);

            let error = apply_rgb_table([0.5, 0.5, 0.5], &table, 1.0)
                .expect_err("an out-of-range table size must be rejected");
            assert_eq!(error, format!("unsupported RGBTable size: {size}"));
        }

        // Non-vacuity: the boundary sizes 2, 32, 33 and 64 are accepted (with a
        // correctly sized identity table). 33 and 64 were rejected before the
        // ceiling was raised from 32 to Adobe's 64.
        for size in [2usize, 32, 33, 64] {
            let node_count = size * size * size;
            let values = vec![[0.5f32, 0.5, 0.5]; node_count];
            let table = make_table(size, values, 0.0, 1.0);
            assert!(
                apply_rgb_table([0.5, 0.5, 0.5], &table, 1.0).is_ok(),
                "size {size} must be supported"
            );
        }
    }

    /// A true identity 3-D table of `size` divisions: node `(r,g,b)` maps to
    /// `(r, g, b) / (size - 1)`. Node order matches the renderer's
    /// `node_index` (B fastest, R slowest).
    fn identity_table_of_size(size: usize) -> RgbTable {
        let scale = (size - 1) as f32;
        let mut values = Vec::with_capacity(size * size * size);
        for r in 0..size {
            for g in 0..size {
                for b in 0..size {
                    values.push([r as f32 / scale, g as f32 / scale, b as f32 / scale]);
                }
            }
        }
        make_table(size, values, 0.0, 1.0)
    }

    #[test]
    fn accepts_64_division_table_and_renders() {
        // The old ceiling (32) rejected this Adobe-legal shape outright. A 64-cube
        // identity table must now render near-identically under the default
        // (sRGB / sRGB / clip) metadata, and a non-identity 64-cube must visibly
        // change a probe (so the near-identity checks are not vacuous).
        let identity = identity_table_of_size(64);
        for probe in [
            [0.0f32, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.25, 0.5, 0.75],
            [0.4, 0.7, 0.2],
            [0.9, 0.1, 0.6],
        ] {
            let output = apply_rgb_table(probe, &identity, 1.0)
                .expect("a 64-division table must render");
            assert_rgb_close(
                output,
                probe,
                IDENTITY_COMBINATION_TOLERANCE,
                &format!("64-division identity {probe:?}"),
            );
        }

        let constant = make_table(64, vec![[0.9f32, 0.1, 0.1]; 64 * 64 * 64], 0.0, 1.0);
        let moved = apply_rgb_table([0.4, 0.7, 0.2], &constant, 1.0).expect("render");
        assert!(
            max_channel_delta(moved, [0.4, 0.7, 0.2]) > MATERIAL_DIFFERENCE,
            "a non-identity 64-division table must move the probe, got {moved:?}"
        );
    }

    #[test]
    fn accepts_33_division_table_previously_over_the_ceiling() {
        // 33 divisions sat just above the old 32 ceiling and used to be rejected.
        let table = identity_table_of_size(33);
        let probe = [0.4f32, 0.7, 0.2];
        let output = apply_rgb_table(probe, &table, 1.0).expect("33-division table must render");
        assert_rgb_close(output, probe, IDENTITY_COMBINATION_TOLERANCE, "33-division identity");
    }

    #[test]
    fn rejects_non_finite_amount_bounds() {
        let input = [0.5f32, 0.5, 0.5];

        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let nan_min = table_2x2x2(IDENTITY_2X2X2, bad, 1.5);
            assert_eq!(
                apply_rgb_table(input, &nan_min, 1.0).expect_err("non-finite min"),
                "invalid RGBTable amount bounds",
                "min = {bad}"
            );

            let nan_max = table_2x2x2(IDENTITY_2X2X2, 0.0, bad);
            assert_eq!(
                apply_rgb_table(input, &nan_max, 1.0).expect_err("non-finite max"),
                "invalid RGBTable amount bounds",
                "max = {bad}"
            );
        }
    }

    #[test]
    fn amount_at_the_min_and_max_edges_renders_finitely() {
        // A table with a non-zero minimum: amounts at, below and above the edges
        // must clamp to the authored bounds and still render finite, in-range
        // pixels in both gamut modes.
        for gamut in [0u32, 1u32] {
            let mut table = table_2x2x2(CONSTANT_2X2X2, 0.25, 1.5);
            table.gamut = gamut;

            for amount in [0.25f32, 1.5, -1.0e9, 1.0e9] {
                let effective =
                    f64::from(amount).clamp(table.min_amount, table.max_amount) as f32;
                let expected = apply_rgb_table([0.2, 0.4, 0.6], &table, effective)
                    .expect("edge amount should apply");
                let actual = apply_rgb_table([0.2, 0.4, 0.6], &table, amount)
                    .expect("edge amount should apply");

                for channel in 0..3 {
                    assert!(
                        actual[channel].is_finite() && (0.0..=1.0).contains(&actual[channel]),
                        "gamut {gamut}, amount {amount}: non-finite/out-of-range {actual:?}"
                    );
                    assert_close(
                        actual[channel],
                        expected[channel],
                        1e-7,
                        &format!("gamut {gamut} amount {amount} clamping channel {channel}"),
                    );
                }
            }
        }

        // Non-vacuity: at the min and max edges the amount genuinely changes the
        // result, so the clamp assertions above are not all identical outputs.
        let table = table_2x2x2(CONSTANT_2X2X2, 0.25, 1.5);
        let at_min = apply_rgb_table([0.2, 0.4, 0.6], &table, 0.25).expect("min");
        let at_max = apply_rgb_table([0.2, 0.4, 0.6], &table, 1.5).expect("max");
        assert!(
            max_channel_delta(at_min, at_max) > MATERIAL_DIFFERENCE,
            "min and max amounts must render differently, got {at_min:?} vs {at_max:?}"
        );
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
        table.color_space = 5;

        let result = RgbTableApplyContext::new(&table, 1.0);
        assert_eq!(
            result.err().expect("unsupported primaries"),
            "unsupported RGBTable primaries: 5"
        );

        // A rejected table must not even reach the full-table scan.
        assert_eq!(
            full_table_validations(),
            0,
            "invalid metadata should be rejected before the full table scan"
        );
    }

    // ----------------------------------- full pipeline: every metadata combination

    fn max_channel_delta(a: [f32; 3], b: [f32; 3]) -> f32 {
        (0..3).map(|c| (a[c] - b[c]).abs()).fold(0.0f32, f32::max)
    }

    /// The pre-5B (5A) per-pixel renderer, reproduced verbatim as an INDEPENDENT
    /// reference: clamp -> sRGB encode -> tetrahedral LUT -> clamp -> amount blend
    /// in the encoded domain -> clamp -> sRGB decode. No matrix, no gamut delta.
    fn reference_5a(table: &RgbTable, effective_amount: f32, rgb: [f32; 3]) -> [f32; 3] {
        let mut encoded = [0.0f32; 3];
        for channel in 0..3 {
            encoded[channel] = srgb_encode(rgb[channel].clamp(0.0, 1.0));
        }

        let lut = interpolate(table, encoded).expect("reference LUT");

        let mut out = [0.0f32; 3];
        for channel in 0..3 {
            let lut_value = lut[channel].clamp(0.0, 1.0);
            let input = encoded[channel];
            let blended = (input + effective_amount * (lut_value - input)).clamp(0.0, 1.0);
            out[channel] = srgb_decode(blended);
        }
        out
    }

    #[test]
    fn srgb_srgb_clip_is_bit_identical_to_the_5a_primitive() {
        let tables = [
            table_2x2x2(IDENTITY_2X2X2, 0.0, 1.0),
            table_2x2x2(CONSTANT_2X2X2, 0.0, 1.5),
            table_2x2x2(
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
            ),
        ];

        let inputs = [
            [0.0f32, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.18, 0.5, 0.9],
            [0.02, 0.75, 0.33],
            [-0.5, 0.25, 1.5],
            [0.5, 0.5, 0.5],
            [0.9, 0.4, 0.1],
            [0.0, 1.0, 0.5],
        ];

        let mut saw_material_change = false;

        for table in &tables {
            for amount in [0.0f32, 0.6, 1.0, 1.5] {
                let effective_amount =
                    f64::from(amount).clamp(table.min_amount, table.max_amount) as f32;

                let context = RgbTableApplyContext::new(table, amount).expect("valid metadata");

                for input in inputs {
                    let actual = context.apply(input).expect("apply");
                    let expected = reference_5a(table, effective_amount, input);

                    for channel in 0..3 {
                        assert_eq!(
                            actual[channel].to_bits(),
                            expected[channel].to_bits(),
                            "sRGB/sRGB/clip must be BIT-IDENTICAL to the 5A primitive for input \
                             {input:?} (amount {amount}) channel {channel}: {actual:?} vs {expected:?}"
                        );
                    }

                    if max_channel_delta(actual, input) > MATERIAL_DIFFERENCE {
                        saw_material_change = true;
                    }
                }
            }
        }

        // Non-vacuity: at least one probe must be materially changed, otherwise the
        // bit-identity above would hold for a renderer that just returns its input.
        assert!(
            saw_material_change,
            "the sRGB/sRGB/clip cases must include a material change, else bit-identity is vacuous"
        );
    }

    #[test]
    fn identity_rgb_table_renders_near_identity_under_every_metadata_combination() {
        // In-gamut probes: neutrals map to themselves through EVERY primaries hop
        // (the PCS normalisation exists for exactly that), so an identity LUT must
        // be a near no-op for all 50 combinations.
        let probes = [
            [0.0f32, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.18, 0.18, 0.18],
            [0.5, 0.5, 0.5],
            [0.75, 0.75, 0.75],
        ];

        let mut combinations = 0;

        for primaries in ALL_PRIMARIES {
            for gamma in ALL_GAMMAS {
                for gamut in ALL_GAMUTS {
                    combinations += 1;

                    let table = table_with_enums(
                        IDENTITY_2X2X2,
                        primaries.wire_value(),
                        gamma.wire_value(),
                        gamut.wire_value(),
                    );
                    let context = RgbTableApplyContext::new(&table, 1.0).expect("context");

                    for probe in probes {
                        let out = context.apply(probe).expect("apply");
                        assert_rgb_close(
                            out,
                            probe,
                            IDENTITY_COMBINATION_TOLERANCE,
                            &format!("{primaries:?}/{gamma:?}/{gamut:?} identity {probe:?}"),
                        );
                    }
                }
            }
        }

        assert_eq!(
            combinations, 50,
            "every 5 primaries x 5 gammas x 2 gamuts combination must be exercised"
        );
    }

    #[test]
    fn synthetic_profile_exercises_every_primaries_enum() {
        // A constant, non-identity profile: every encoded coordinate maps to
        // (0.25, 0.5, 0.75). At amount 1.0 the result is the decoded constant
        // pushed back through the primaries decode matrix, so it depends on the
        // primaries enum.
        const PROBE: [f32; 3] = [0.4, 0.6, 0.2];

        let mut srgb_reference: Option<[f32; 3]> = None;
        let mut seen = 0;

        for primaries in ALL_PRIMARIES {
            let table = table_with_enums(CONSTANT_2X2X2, primaries.wire_value(), 1, 0);
            let out = RgbTableApplyContext::new(&table, 1.0)
                .expect("context")
                .apply(PROBE)
                .expect("apply");

            assert!(
                out.iter().all(|channel| channel.is_finite()),
                "{primaries:?} must render finitely, got {out:?}"
            );

            match srgb_reference {
                None => {
                    assert_eq!(primaries.wire_value(), 0, "sRGB must be first in wire order");
                    srgb_reference = Some(out);
                }
                Some(reference) => assert!(
                    max_channel_delta(out, reference) > MATERIAL_DIFFERENCE,
                    "{primaries:?} must render differently from sRGB, got {out:?} vs {reference:?}"
                ),
            }

            seen += 1;
        }

        assert_eq!(seen, 5, "all five primaries enums must be exercised");
    }

    #[test]
    fn synthetic_profile_exercises_every_gamma_enum() {
        // Constant profile (0.25, 0.5, 0.75) with sRGB identity primaries and
        // amount 1.0: the result is exactly the constant decoded by the gamma, so
        // every gamma enum must render distinctly from Linear.
        const PROBE: [f32; 3] = [0.3, 0.3, 0.3];

        let mut linear_reference: Option<[f32; 3]> = None;
        let mut seen = 0;

        for gamma in ALL_GAMMAS {
            let table = table_with_enums(CONSTANT_2X2X2, 0, gamma.wire_value(), 0);
            let out = RgbTableApplyContext::new(&table, 1.0)
                .expect("context")
                .apply(PROBE)
                .expect("apply");

            assert!(
                out.iter().all(|channel| channel.is_finite()),
                "{gamma:?} must render finitely, got {out:?}"
            );

            match linear_reference {
                None => {
                    assert_eq!(gamma.wire_value(), 0, "Linear must be first in wire order");
                    linear_reference = Some(out);
                }
                Some(reference) => assert!(
                    max_channel_delta(out, reference) > MATERIAL_DIFFERENCE,
                    "{gamma:?} must render differently from Linear, got {out:?} vs {reference:?}"
                ),
            }

            seen += 1;
        }

        assert_eq!(seen, 5, "all five gamma enums must be exercised");
    }

    #[test]
    fn gamut_extend_re_adds_the_excursion_after_the_decode() {
        // sRGB primaries keep the hop an identity, so the "linear table primaries"
        // domain IS the pipeline domain and the recorded delta is exactly
        // `(input - clamp(input))`.
        const INPUT: [f32; 3] = [-0.25, 0.5, 1.75];

        let clip = RgbTableApplyContext::new(&table_with_enums(CONSTANT_2X2X2, 0, 1, 0), 1.0)
            .expect("clip context")
            .apply(INPUT)
            .expect("clip apply");

        let extend = RgbTableApplyContext::new(&table_with_enums(CONSTANT_2X2X2, 0, 1, 1), 1.0)
            .expect("extend context")
            .apply(INPUT)
            .expect("extend apply");

        // Common path: the clamped input is gamma-encoded, the constant LUT is
        // blended at amount 1.0, and the result is gamma-decoded.
        let clamped_linear = [0.0f32, 0.5, 1.0];
        let base = [
            srgb_decode(0.25),
            srgb_decode(0.5),
            srgb_decode(0.75),
        ];
        let delta = [
            INPUT[0] - clamped_linear[0],
            INPUT[1] - clamped_linear[1],
            INPUT[2] - clamped_linear[2],
        ];

        for channel in 0..3 {
            // Clip discards the excursion...
            assert_close(
                clip[channel],
                base[channel],
                1e-7,
                &format!("gamut_clip channel {channel}"),
            );
            // ...extend re-adds it AFTER the decode, then the final clamp applies.
            assert_close(
                extend[channel],
                (base[channel] + delta[channel]).clamp(0.0, 1.0),
                1e-6,
                &format!("gamut_extend channel {channel}"),
            );
        }

        // The channels that left [0,1] did so exactly: clip keeps the decoded
        // base, extend moves it by the recorded excursion.
        assert_eq!(clip[0], base[0], "clip discards the below-zero excursion");
        assert_eq!(clip[2], base[2], "clip discards the above-one excursion");
        assert_close(extend[0], 0.0, 1e-6, "extend re-adds a -0.25 excursion");
        assert_close(extend[2], 1.0, 1e-6, "extend re-adds a +0.75 excursion");

        // Non-vacuity: the two modes must differ on the out-of-range channels,
        // otherwise the assertions above are indistinguishable. (The below-zero
        // channel only moves by `srgb_decode(0.25) = 0.0509`, hence the smaller
        // but still material threshold.)
        assert!(
            (extend[0] - clip[0]).abs() > 0.01,
            "extend and clip must differ on the below-zero channel, got {} vs {}",
            extend[0],
            clip[0]
        );
        assert!(
            (extend[2] - clip[2]).abs() > 0.1,
            "extend and clip must differ on the above-one channel, got {} vs {}",
            extend[2],
            clip[2]
        );
    }

    /// Tolerance for the `gamut_extend` order pin. The reference rebuilds the
    /// production pipeline with the same f32 primitives and only the ordering of
    /// the delta re-add and the decode matrix differs, so a tight bound applies.
    const EXTEND_ORDER_TOLERANCE: f32 = 1e-6;

    fn clamp_vec(v: [f32; 3]) -> [f32; 3] {
        [
            v[0].clamp(0.0, 1.0),
            v[1].clamp(0.0, 1.0),
            v[2].clamp(0.0, 1.0),
        ]
    }

    #[test]
    fn gamut_extend_adds_the_delta_before_the_decode_matrix_for_non_srgb_primaries() {
        // The sRGB `gamut_extend` test cannot distinguish the SDK order
        // (`decode matrix -> add delta`) from the reversed order
        // (`add delta -> decode matrix`): for the sRGB primaries the hop is the
        // identity, so the two commute. A NON-sRGB hop whose decode matrix has
        // negative off-diagonal coefficients (DisplayP3) makes the two orders
        // genuinely differ. `dng_reference.cpp:3800-3821` fixes the order as
        // add-delta (h) THEN the decode matrix (i).
        const PRIMARIES: Primaries = Primaries::DisplayP3;
        // Out of range in the working space, so `gamut_extend` records a
        // non-zero excursion in the linear table-primaries domain.
        const INPUT: [f32; 3] = [1.4, 0.4, -0.1];

        let transform = PrimariesTransform::new(PRIMARIES);
        assert!(
            !transform.is_identity_shortcut(),
            "this test must use a non-identity primaries hop"
        );

        // Rebuild steps (a)-(g) of the pipeline exactly, at amount 1.0 with the
        // constant LUT (so the gamma encode of the clamped input is irrelevant to
        // the LUT result and only the delta and the decode matrix matter).
        let linear = transform.encode(INPUT); // (a)
        let (clamped, delta) = color_space::gamut_clamp(linear, Gamut::Extend); // (b)+(b')
        let encoded_input = [
            srgb_encode(clamped[0]),
            srgb_encode(clamped[1]),
            srgb_encode(clamped[2]),
        ]; // (c)

        // (d)-(f): constant encoded LUT blended at amount 1.0, then clamped.
        const LUT: [f32; 3] = [0.25, 0.5, 0.75];
        let mut blended = [0.0f32; 3];
        for channel in 0..3 {
            let lut_value = LUT[channel].clamp(0.0, 1.0);
            let input = encoded_input[channel];
            blended[channel] = (input + 1.0 * (lut_value - input)).clamp(0.0, 1.0);
        }

        // (g): gamma decode.
        let base = [
            srgb_decode(blended[0]),
            srgb_decode(blended[1]),
            srgb_decode(blended[2]),
        ];

        // SDK ORDER: add delta (h) THEN the decode matrix (i) THEN final clamp.
        let sdk_order = clamp_vec(transform.decode([
            base[0] + delta[0],
            base[1] + delta[1],
            base[2] + delta[2],
        ]));

        // REVERSED ORDER: decode matrix before the delta re-add.
        let decoded_base = transform.decode(base);
        let reversed_order = clamp_vec([
            decoded_base[0] + delta[0],
            decoded_base[1] + delta[1],
            decoded_base[2] + delta[2],
        ]);

        // Non-vacuity: the two candidate orders MUST diverge materially for this
        // input, otherwise the test could not tell them apart.
        let order_gap = max_channel_delta(sdk_order, reversed_order);
        assert!(
            order_gap > MATERIAL_DIFFERENCE,
            "the two candidate orders must differ materially, got a gap of {order_gap}"
        );

        // Production must match the SDK order and must NOT match the reversed one.
        let actual = RgbTableApplyContext::new(
            &table_with_enums(CONSTANT_2X2X2, PRIMARIES.wire_value(), 1, 1),
            1.0,
        )
        .expect("extend context")
        .apply(INPUT)
        .expect("extend apply");

        assert_rgb_close(
            actual,
            sdk_order,
            EXTEND_ORDER_TOLERANCE,
            "gamut_extend must add the delta before the decode matrix",
        );
        assert!(
            max_channel_delta(actual, reversed_order) > MATERIAL_DIFFERENCE,
            "production must reject the reversed order, got {actual:?} vs {reversed_order:?}"
        );

        // The excursion is real (delta non-zero), so "extend == clip" could not
        // have produced this result.
        assert!(
            delta.iter().any(|d| *d != 0.0),
            "the input must leave [0,1] in the linear table-primaries domain"
        );
    }
}
