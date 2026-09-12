//! Renderer primitives for the Adobe `dng_look_table` ("LookTable") stage.
//!
//! Adobe applies the LookTable to **linear ProPhoto RGB with a D50 white point**
//! (the DNG "RIMM" connection space), regardless of any RGBTable `primaries`
//! (DNG Spec 1.7.1.0 p.103; `dng_render.cpp` calls `DoBaselineHueSatMap` on the
//! linear ProPhoto pixels). Once-Lab's working representation is **linear sRGB
//! D65**, so one fixed 3x3 hop brackets the stage:
//!
//! ```text
//! linear sRGB (D65)
//!   -> 3x3: M_sRGB(D65) -> ProPhoto(D50)
//!   -> RGB -> HSV            (V = max, S = (max-min)/max, H in [0, 6))
//!   -> tri-linear table lookup of (hueShiftDeg, satScale, valScale)
//!   -> h += hueShiftDeg * 6/360; s = min(s * satScale, 1); v = clamp(v * valScale, 0, 1)
//!   -> HSV -> RGB
//!   -> 3x3: M_ProPhoto(D50) -> sRGB(D65)
//! ```
//!
//! Both 3x3 matrices are *derived*, never hardcoded: they replicate Adobe's
//! `dng_color_space::SetMatrixToPCS` row normalisation of the published sRGB and
//! ProPhoto constants, so that device white and neutral grey round-trip exactly
//! (see [`super::linear_rgb::set_matrix_to_pcs`]).
//!
//! Only `encoding == 0` (Linear) and `encoding == 1` (sRGB) exist; for `sRGB`,
//! Adobe transfer-encodes **only** the `V` coordinate before the lookup and
//! decodes it afterwards (`BuildHueSatMapEncodingTable`, `dng_render.cpp:991-1000`
//! and `RefBaselineHueSatMap`, `dng_reference.cpp:1652-1655,1741-1743`). The
//! encode is confined to the `valDivisions >= 2` branch: the "2.5-D" branch
//! leaves `vEncoded = v` (`dng_reference.cpp:1584`) yet still decodes.

use super::linear_rgb::{
    compose_working_to_primaries, invert3, mul3, to_f32, Matrix3, PROPHOTO_RAW, SRGB_RAW,
};
use super::look_table::{
    ENCODING_LINEAR, ENCODING_SRGB, LookTable, MAX_HUE_DIVISIONS, MAX_SAT_DIVISIONS,
    MAX_TOTAL_SAMPLES, MAX_VAL_DIVISIONS, MIN_HUE_DIVISIONS, MIN_SAT_DIVISIONS, MIN_VAL_DIVISIONS,
};
use super::srgb_transfer::{srgb_decode, srgb_encode};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

// The row-major 3x3 `Matrix3` type is the shared `super::linear_rgb::Matrix3`.

// Test-only counter of how many times the full (O(samples)) table validation
// runs. Thread-local so tests running in parallel cannot observe each other's
// validations.
#[cfg(test)]
thread_local! {
    static FULL_TABLE_VALIDATIONS: AtomicUsize = const { AtomicUsize::new(0) };
}

/// Records one run of the full table validation. Expands to nothing outside
/// `cfg(test)`, so non-test builds carry no counting overhead.
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

/// A validated rendering context for a single Adobe LookTable.
///
/// All expensive, profile-level work happens exactly once in
/// [`LookTableApplyContext::new`]: encoding support, division bounds, sample
/// total, exact entry count, entry finiteness and the two derived space-hop
/// matrices. [`LookTableApplyContext::apply`] then performs only per-pixel work,
/// so rendering a whole image does not repeat the O(samples) validation.
///
/// The table is borrowed, never copied.
pub(crate) struct LookTableApplyContext<'a> {
    table: &'a LookTable,
    srgb_to_prophoto: Matrix3,
    prophoto_to_srgb: Matrix3,
}

impl<'a> LookTableApplyContext<'a> {
    pub(crate) fn new(table: &'a LookTable) -> Result<Self, String> {
        validate_render_metadata(table)?;
        validate_table_structure(table)?;

        Ok(Self {
            table,
            srgb_to_prophoto: linear_srgb_to_linear_prophoto(),
            prophoto_to_srgb: linear_prophoto_to_linear_srgb(),
        })
    }

    /// Renders one pixel. Performs no profile-level validation.
    pub(crate) fn apply(&self, rgb: [f32; 3]) -> Result<[f32; 3], String> {
        for component in rgb {
            if !component.is_finite() {
                return Err("LookTable input contains non-finite value".to_string());
            }
        }

        let prophoto = mul3(self.srgb_to_prophoto, rgb);
        let [h, s, v] = linear_rgb_to_hsv(prophoto);

        // Adobe transfer-encodes V only in the `valDivisions >= 2` branch
        // (`RefBaselineHueSatMap`, `dng_reference.cpp:1652-1655`); the "2.5-D"
        // branch leaves `vEncoded = v` untouched (`dng_reference.cpp:1584`) because
        // it never indexes a value axis. Both branches then DECODE
        // (`dng_reference.cpp:1741-1743`), so the encode must be skipped -- but the
        // decode kept -- whenever `valDivisions < 2`. Adobe pins V into [0,1]
        // before the encode (`Pin_real32`, `dng_reference.cpp:1654`).
        let decode_srgb = self.table.encoding == ENCODING_SRGB;
        let v_for_lookup = if decode_srgb && self.table.val_divisions >= 2 {
            srgb_encode(v.clamp(0.0, 1.0))
        } else {
            v
        };

        let [hue_shift_degrees, sat_scale, val_scale] = lookup(self.table, h, s, v_for_lookup);

        let h_out = h + hue_shift_degrees * (6.0 / 360.0);
        let s_out = (s * sat_scale).min(1.0);
        let mut v_out = (v_for_lookup * val_scale).clamp(0.0, 1.0);
        if decode_srgb {
            v_out = srgb_decode(v_out);
        }

        Ok(mul3(self.prophoto_to_srgb, hsv_to_linear_rgb(h_out, s_out, v_out)))
    }
}

/// Convenience wrapper: validates the table, renders a single pixel.
///
/// For more than a handful of pixels prefer [`LookTableApplyContext`], which
/// validates once and then renders many pixels.
pub(crate) fn apply_look_table(rgb: [f32; 3], table: &LookTable) -> Result<[f32; 3], String> {
    LookTableApplyContext::new(table)?.apply(rgb)
}

/// Checks the fields that decide whether the table can be rendered at all.
///
/// Kept separate from [`validate_table_structure`] so an unsupported encoding is
/// rejected before the O(samples) entry scan, mirroring how
/// `RgbTableApplyContext` rejects unsupported primaries/gamma/gamut first.
fn validate_render_metadata(table: &LookTable) -> Result<(), String> {
    if table.encoding != ENCODING_LINEAR && table.encoding != ENCODING_SRGB {
        return Err(format!("unsupported LookTable encoding: {}", table.encoding));
    }

    Ok(())
}

fn validate_table_structure(table: &LookTable) -> Result<(), String> {
    record_full_table_validation();

    let hue = table.hue_divisions;
    let sat = table.sat_divisions;
    let val = table.val_divisions;

    if !(MIN_HUE_DIVISIONS..=MAX_HUE_DIVISIONS).contains(&hue) {
        return Err(format!("unsupported LookTable hue divisions: {hue}"));
    }
    if !(MIN_SAT_DIVISIONS..=MAX_SAT_DIVISIONS).contains(&sat) {
        return Err(format!("unsupported LookTable sat divisions: {sat}"));
    }
    if !(MIN_VAL_DIVISIONS..=MAX_VAL_DIVISIONS).contains(&val) {
        return Err(format!("unsupported LookTable val divisions: {val}"));
    }

    let total = u64::from(hue) * u64::from(sat) * u64::from(val);
    if total > u64::from(MAX_TOTAL_SAMPLES) {
        return Err(format!(
            "LookTable sample count {total} exceeds supported maximum {MAX_TOTAL_SAMPLES}"
        ));
    }

    let expected = total as usize;
    if table.entries.len() != expected {
        return Err(format!(
            "malformed LookTable: expected {expected} entries, got {}",
            table.entries.len()
        ));
    }

    // Every entry must be finite, not just the cells a particular lookup happens
    // to sample: a malformed table must be rejected before any pixel renders.
    for (index, entry) in table.entries.iter().enumerate() {
        for component in entry {
            if !component.is_finite() {
                return Err(format!("LookTable contains non-finite entry at index {index}"));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------- color hop
//
// The sRGB(D65) <-> ProPhoto(D50) hop is built by the single shared
// [`super::linear_rgb`] toolbox, so the LookTable and RGBTable stages cannot
// drift apart. It is *derived*, never hardcoded:
// `Invert(SetMatrixToPCS(PROPHOTO_RAW)) * SetMatrixToPCS(SRGB_RAW)`.

/// Linear ProPhoto (D50) <= sRGB (D65) transfer, in f64, from the raw constants.
fn srgb_to_prophoto_f64() -> [[f64; 3]; 3] {
    compose_working_to_primaries(PROPHOTO_RAW, SRGB_RAW)
}

/// `M_sRGB(D65) -> ProPhoto(D50)` = `Invert(P) * S`, where `P`/`S` are the
/// PCS-normalised ProPhoto/sRGB matrices.
pub(super) fn linear_srgb_to_linear_prophoto() -> Matrix3 {
    to_f32(srgb_to_prophoto_f64())
}

/// `M_ProPhoto(D50) -> sRGB(D65)` = `Invert(M_sRGB -> ProPhoto)`.
pub(super) fn linear_prophoto_to_linear_srgb() -> Matrix3 {
    to_f32(invert3(srgb_to_prophoto_f64()))
}

// ---------------------------------------------------------------------- HSV

/// Linear RGB -> HSV, mirroring `DNG_RGBtoHSV` (`dng_utils.h:859-906`).
///
/// `V` is the maximum component, `S = (V - m) / V` (`0` when `V == 0`), and `H`
/// is a 6-sector coordinate in `[0, 6)`. Negative components are deliberately
/// **preserved**, not pinned to zero: the non-overrange reference path (i.e.
/// `fSupportOverrange == FALSE`, the only path this project implements) calls the
/// raw `DNG_RGBtoHSV` at `dng_reference.cpp:1582`. The `Max_real32 (…, 0.0f)`
/// pins at `dng_reference.cpp:1567-1569` sit inside the `if (supportOverrange)`
/// block, so they belong to Adobe's HDR path alone, and the `>= 0`-pinning
/// `DNG_PinnedNonnegativeRGBtoHSV` (`dng_utils.h:915`) is never called by
/// `RefBaselineHueSatMap` at all. Because the LookTable runs in linear ProPhoto,
/// an out-of-gamut component below zero is reachable and is therefore carried
/// through the stage unchanged, matching the SDK exactly.
fn linear_rgb_to_hsv(rgb: [f32; 3]) -> [f32; 3] {
    let r = rgb[0];
    let g = rgb[1];
    let b = rgb[2];

    let v = r.max(g).max(b);
    let m = r.min(g).min(b);
    let gap = v - m;

    if gap > 0.0 {
        let h = if r == v {
            let raw = (g - b) / gap;
            if raw < 0.0 { raw + 6.0 } else { raw }
        } else if g == v {
            2.0 + (b - r) / gap
        } else {
            4.0 + (r - g) / gap
        };

        [h, gap / v, v]
    } else {
        [0.0, 0.0, v]
    }
}

/// HSV -> linear RGB, mirroring `DNG_HSVtoRGB` (`dng_utils.h:938-996`).
///
/// `S <= 0` collapses to the grey value `V`; otherwise the hue is wrapped into
/// `[0, 6)` and quantised into one of six sectors, with the rare `i == 6` case
/// folded into `i == 0`.
fn hsv_to_linear_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    if s <= 0.0 {
        return [v, v, v];
    }

    let mut h = h % 6.0;
    if h < 0.0 {
        h += 6.0;
    }

    let i = h as i32;
    let f = h - i as f32;

    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));

    match i {
        0 | 6 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

// ------------------------------------------------------------------- lookup

/// Tri-linear LookTable lookup for one HSV coordinate.
///
/// Replicates `RefBaselineHueSatMap` (`dng_reference.cpp:1529-1535, 1657-1731`):
/// hue scales by `hueDivisions / 6` and indexes with **wrap-around** (the cell
/// above the last hue sample is the first), while saturation and value scale by
/// `divisions - 1` and **clamp** the cell base to `divisions - 2`. The grid is
/// stored value-outermost, so the strides are `hueStep = satDivisions` and
/// `valStep = hueDivisions * satDivisions`.
///
/// The caller has already validated the table (see
/// [`validate_table_structure`]), so this is infallible.
fn lookup(table: &LookTable, h: f32, s: f32, v: f32) -> [f32; 3] {
    let hue_divisions = table.hue_divisions;
    let sat_divisions = table.sat_divisions;
    let val_divisions = table.val_divisions;

    // Adobe collapses a single hue division to a constant hue (hScale = 0).
    let h_scale = if hue_divisions < 2 {
        0.0
    } else {
        hue_divisions as f32 / 6.0
    };
    let s_scale = (sat_divisions - 1) as f32;
    let v_scale = (val_divisions - 1) as f32;

    let max_hue_index0 = hue_divisions as i32 - 1;
    let max_sat_index0 = sat_divisions as i32 - 2;

    let hue_step = sat_divisions as usize;
    let val_step = (hue_divisions * sat_divisions) as usize;

    let h_scaled = h * h_scale;
    let s_scaled = s * s_scale;

    let mut h_index0 = (h_scaled as i32).clamp(0, max_hue_index0);
    let h_index1;
    if h_index0 >= max_hue_index0 {
        // Hue wraps: the last hue sample interpolates with the first.
        h_index0 = max_hue_index0;
        h_index1 = 0usize;
    } else {
        h_index1 = (h_index0 + 1) as usize;
    }
    let h_index0 = h_index0 as usize;

    let s_index0 = (s_scaled as i32).clamp(0, max_sat_index0) as usize;
    let s_index1 = s_index0 + 1;

    // A single value division is Adobe's "2.5-D" table: there is no value axis to
    // interpolate, so both value samples are the sole (v = 0) plane. Handled
    // separately because `valDivisions - 2` would otherwise be -1.
    let (v_index0, v_index1, v_fract1) = if val_divisions < 2 {
        (0usize, 0usize, 0.0f32)
    } else {
        let v_scaled = v * v_scale;
        let base = (v_scaled as i32).clamp(0, val_divisions as i32 - 2) as usize;
        (base, base + 1, v_scaled - base as f32)
    };

    let h_fract1 = h_scaled - h_index0 as f32;
    let s_fract1 = s_scaled - s_index0 as f32;

    let h_fract0 = 1.0 - h_fract1;
    let s_fract0 = 1.0 - s_fract1;
    let v_fract0 = 1.0 - v_fract1;

    let entry = |val: usize, hue: usize, sat: usize| -> [f32; 3] {
        table.entries[val * val_step + hue * hue_step + sat]
    };

    let c000 = entry(v_index0, h_index0, s_index0);
    let c001 = entry(v_index0, h_index0, s_index1);
    let c010 = entry(v_index0, h_index1, s_index0);
    let c011 = entry(v_index0, h_index1, s_index1);
    let c100 = entry(v_index1, h_index0, s_index0);
    let c101 = entry(v_index1, h_index0, s_index1);
    let c110 = entry(v_index1, h_index1, s_index0);
    let c111 = entry(v_index1, h_index1, s_index1);

    let mut out = [0.0f32; 3];
    for channel in 0..3 {
        // Same nesting as the SDK: sat innermost, then hue, then value.
        let low = h_fract0 * (s_fract0 * c000[channel] + s_fract1 * c001[channel])
            + h_fract1 * (s_fract0 * c010[channel] + s_fract1 * c011[channel]);
        let high = h_fract0 * (s_fract0 * c100[channel] + s_fract1 * c101[channel])
            + h_fract1 * (s_fract0 * c110[channel] + s_fract1 * c111[channel]);

        out[channel] = v_fract0 * low + v_fract1 * high;
    }

    out
}

// ------------------------------------------------------------- (sRGB transfer)
//
// The sRGB encode/decode pair used by `encoding == 1` lives once, in
// [`super::srgb_transfer`], and is shared with the RGBTable stage.

#[cfg(test)]
mod tests {
    use super::*;

    /// f32 tolerance for a round trip that should mathematically be exact.
    const ROUND_TRIP_TOLERANCE: f32 = 1e-5;

    /// Looser tolerance for the full identity-LookTable pipeline, which also runs
    /// an HSV<->RGB round trip and two matrix multiplies.
    const IDENTITY_NO_OP_TOLERANCE: f32 = 1e-4;

    /// f32 tolerance for a lookup value that is an exact weighted average of
    /// representable grid entries.
    const LOOKUP_TOLERANCE: f32 = 1e-6;

    fn make_table(
        hue: u32,
        sat: u32,
        val: u32,
        entries: Vec<[f32; 3]>,
        encoding: u32,
    ) -> LookTable {
        LookTable {
            hue_divisions: hue,
            sat_divisions: sat,
            val_divisions: val,
            entries,
            encoding,
            flags: None,
        }
    }

    /// A constant grid: every cell maps to the same modification.
    fn constant_table(hue: u32, sat: u32, val: u32, entry: [f32; 3]) -> LookTable {
        let count = (hue * sat * val) as usize;
        make_table(hue, sat, val, vec![entry; count], ENCODING_LINEAR)
    }

    fn identity_table(hue: u32, sat: u32, val: u32) -> LookTable {
        constant_table(hue, sat, val, [0.0, 1.0, 1.0])
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

    // ------------------------------------------------------------- hop 3x3

    #[test]
    fn space_hop_preserves_device_white_and_neutral_grey() {
        let forward = linear_srgb_to_linear_prophoto();
        let inverse = linear_prophoto_to_linear_srgb();

        for neutral in [[1.0f32, 1.0, 1.0], [0.18, 0.18, 0.18], [0.5, 0.5, 0.5], [0.0, 0.0, 0.0]] {
            let prophoto = mul3(forward, neutral);
            // The normalisation exists precisely so that a neutral maps to itself
            // across the hop; anything else would mean the matrices disagree on
            // the white point.
            assert_rgb_close(prophoto, neutral, ROUND_TRIP_TOLERANCE, "forward neutral");

            let back = mul3(inverse, prophoto);
            assert_rgb_close(back, neutral, ROUND_TRIP_TOLERANCE, "round trip neutral");
        }

        // Non-vacuity: a chromatic colour must actually move through the hop,
        // otherwise the matrices could be the identity and the checks above would
        // pass trivially.
        let red = [1.0f32, 0.0, 0.0];
        let moved = mul3(forward, red);
        assert!(
            (moved[0] - red[0]).abs() > 0.1 || (moved[1] - red[1]).abs() > 0.1,
            "pure red must not be a fixed point of the hop, got {moved:?}"
        );
    }

    // -------------------------------------------------- identity / preserved

    #[test]
    fn identity_look_table_is_a_near_no_op() {
        let table = identity_table(36, 16, 16);
        let context = LookTableApplyContext::new(&table).expect("identity context");

        let probes = [
            [0.0f32, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.18, 0.18, 0.18],
            [0.18, 0.5, 0.9],
            [0.9, 0.4, 0.1],
            [0.05, 0.10, 0.15],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
        ];

        for probe in probes {
            let output = context.apply(probe).expect("identity apply");
            assert_rgb_close(output, probe, IDENTITY_NO_OP_TOLERANCE, &format!("identity {probe:?}"));
        }
    }

    #[test]
    fn black_white_and_neutral_are_preserved_by_an_identity_table() {
        let table = identity_table(9, 4, 4);
        let context = LookTableApplyContext::new(&table).expect("identity context");

        let probes = [
            ([0.0f32, 0.0, 0.0], "black"),
            ([1.0, 1.0, 1.0], "white"),
            ([0.25, 0.25, 0.25], "neutral"),
        ];

        for (probe, label) in probes {
            let output = context.apply(probe).expect("identity apply");
            assert_rgb_close(output, probe, IDENTITY_NO_OP_TOLERANCE, label);
        }

        // Non-vacuity: a non-identity table must actually change a neutral, so the
        // preservation assertions above are not passing by construction.
        let mut changed = identity_table(9, 4, 4);
        for entry in &mut changed.entries {
            entry[2] = 2.0; // valScale = 2
        }
        let changed_context = LookTableApplyContext::new(&changed).expect("changed context");
        let output = changed_context.apply([0.25, 0.25, 0.25]).expect("apply");
        assert!(
            (output[0] - 0.25).abs() > 0.1,
            "valScale = 2 must visibly change a neutral, got {output:?}"
        );
    }

    #[test]
    fn output_is_finite_for_finite_input() {
        let table = identity_table(36, 16, 16);
        let context = LookTableApplyContext::new(&table).expect("context");

        for index in 0..128 {
            let x = index as f32 / 127.0;
            let output = context.apply([x, 1.0 - x, 0.5 * x]).expect("apply");
            for channel in output {
                assert!(channel.is_finite(), "non-finite output {output:?}");
            }
        }
    }

    #[test]
    fn rejects_non_finite_input() {
        let table = identity_table(1, 2, 1);
        let context = LookTableApplyContext::new(&table).expect("context");

        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let error = context
                .apply([bad, 0.5, 0.5])
                .expect_err("non-finite input must be rejected");
            assert_eq!(error, "LookTable input contains non-finite value");
        }
    }

    // ---------------------------------------------------------- interpolation

    #[test]
    fn hue_interpolation_wraps_at_the_top_cell() {
        // hueDiv = 6 => hScale = 1, so h == h_index for h in [0,6). Grid value =
        // hue index (fHueShift = hue), sat/val constant.
        let mut entries = Vec::new();
        for _v in 0..2 {
            for hue in 0..6u32 {
                for _s in 0..2 {
                    entries.push([hue as f32, 1.0, 1.0]);
                }
            }
        }
        let table = make_table(6, 2, 2, entries, ENCODING_LINEAR);

        // Interior: h = 4.5 interpolates cells 4 and 5 -> 4.5 (no wrap involved).
        let interior = lookup(&table, 4.5, 0.0, 0.0);
        assert_close(interior[0], 4.5, LOOKUP_TOLERANCE, "interior hue");

        // Boundary: h = 5.5 has hIndex0 == maxHueIndex0 == 5, so hIndex1 wraps to 0
        // and the result is 0.5*cell5 + 0.5*cell0 = 2.5. A non-wrapping
        // implementation would run off the end (or clamp) and give 5.0.
        let wrapped = lookup(&table, 5.5, 0.0, 0.0);
        assert_close(wrapped[0], 2.5, LOOKUP_TOLERANCE, "wrapped hue");

        assert!(
            (wrapped[0] - interior[0]).abs() > 1.0,
            "the wrap case must differ from the interior case"
        );
    }

    #[test]
    fn saturation_index_clamps_to_the_last_cell() {
        // satDiv = 3 => sScale = 2. satScale varies by sat cell: 10, 20, 30.
        let mut entries = Vec::new();
        for _v in 0..2 {
            for _h in 0..2 {
                for sat in 0..3u32 {
                    entries.push([0.0, 10.0 * (sat + 1) as f32, 1.0]);
                }
            }
        }
        let table = make_table(2, 3, 2, entries, ENCODING_LINEAR);

        // s = 0 -> base cell 0.
        let low = lookup(&table, 0.0, 0.0, 0.0);
        assert_close(low[1], 10.0, LOOKUP_TOLERANCE, "sat base");

        // s = 1 -> sScaled = 2, the base clamps to maxSatIndex0 = 1 with fract 1.0,
        // so the last saturation sample (cell 2, value 30) is used. Saturation
        // CLAMPS: unlike hue it never wraps back to cell 0 (10).
        let high = lookup(&table, 0.0, 1.0, 0.0);
        assert_close(high[1], 30.0, LOOKUP_TOLERANCE, "sat clamped to last sample");
        assert!(
            (high[1] - 10.0).abs() > 1.0,
            "saturation must clamp to the last sample, not wrap to cell 0"
        );
        assert!(
            (high[1] - low[1]).abs() > 1.0,
            "the clamped case must differ from the base case"
        );
    }

    #[test]
    fn value_index_clamps_to_the_last_cell() {
        // valDiv = 3 => vScale = 2. valScale varies by val cell: 10, 20, 30.
        let mut entries = Vec::new();
        for val in 0..3u32 {
            for _h in 0..2 {
                for _s in 0..2 {
                    entries.push([0.0, 1.0, 10.0 * (val + 1) as f32]);
                }
            }
        }
        let table = make_table(2, 2, 3, entries, ENCODING_LINEAR);

        let low = lookup(&table, 0.0, 0.0, 0.0);
        assert_close(low[2], 10.0, LOOKUP_TOLERANCE, "val base");

        // v = 1 -> vScaled = 2 clamps the base to maxValIndex0 = 1 with fract 1.0,
        // so the last value sample (value 30) is used. Value CLAMPS.
        let high = lookup(&table, 0.0, 0.0, 1.0);
        assert_close(high[2], 30.0, LOOKUP_TOLERANCE, "val clamped to last sample");
        assert!(
            (high[2] - 10.0).abs() > 1.0,
            "value must clamp to the last sample, not wrap to cell 0"
        );
        assert!(
            (high[2] - low[2]).abs() > 1.0,
            "the clamped case must differ from the base case"
        );
    }

    #[test]
    fn single_value_division_uses_the_2_5d_table() {
        // Adobe's "2.5-D" table has valDivisions == 1, i.e. no value axis. The
        // lookup must read the single value plane instead of computing a negative
        // `maxValIndex0` (which would panic).
        let entries = vec![[0.0, 1.0, 1.0], [30.0, 1.0, 2.0]]; // h0s0, h0s1
        let table = make_table(1, 2, 1, entries, ENCODING_LINEAR);

        let result = lookup(&table, 0.0, 0.0, 0.5);
        assert_close(result[0], 0.0, LOOKUP_TOLERANCE, "2.5-D hue shift");
        assert_close(result[2], 1.0, LOOKUP_TOLERANCE, "2.5-D val scale");

        // Non-vacuity: the value coordinate is genuinely ignored, so a different v
        // yields the same modification.
        let other = lookup(&table, 0.0, 0.0, 0.9);
        assert_eq!(result, other);

        // And the full pipeline renders (the previous panic would surface here).
        let context = LookTableApplyContext::new(&table).expect("2.5-D context");
        let output = context.apply([0.5, 0.5, 0.5]).expect("2.5-D apply");
        assert_rgb_close(output, [0.5, 0.5, 0.5], IDENTITY_NO_OP_TOLERANCE, "2.5-D no-op");
    }

    // -------------------------------------------------------------- encoding

    #[test]
    fn srgb_2_5d_table_skips_the_encode_but_still_decodes() {
        // encoding == sRGB combined with valDivisions < 2 is the one place where
        // Adobe's two branches disagree about V. The "2.5-D" branch leaves
        // `vEncoded = v` (dng_reference.cpp:1584; the encode at :1654 lives in the
        // `else` branch only) yet still DECODES after the lookup (:1741-1743). The
        // stage must therefore compute decode(clamp(v * valScale)) and never
        // decode(clamp(encode(v) * valScale)).
        let table = make_table(1, 2, 1, vec![[0.0, 1.0, 1.0]; 2], ENCODING_SRGB);
        assert_eq!(table.val_divisions, 1, "this test must exercise the 2.5-D branch");

        let output = LookTableApplyContext::new(&table)
            .expect("2.5-D sRGB context")
            .apply([0.5, 0.5, 0.5])
            .expect("apply");

        // Hand-computed: V stays 0.5 and valScale is 1, so the value leaves the
        // stage as decode(0.5) = ((0.5 + 0.055) / 1.055)^2.4 = 0.2140411.
        let expected = srgb_decode(0.5);
        assert_close(expected, 0.2140411, 1e-5, "hand-computed decode(0.5)");
        assert_rgb_close(output, [expected; 3], IDENTITY_NO_OP_TOLERANCE, "unencoded 2.5-D grey");

        // Non-vacuity: encoding 0 skips the decode entirely, so the two encodings
        // must not agree. The old behaviour (encode *then* decode) collapsed back to
        // ~0.5 and matched the linear table -- exactly the bug this guards.
        let linear = make_table(1, 2, 1, vec![[0.0, 1.0, 1.0]; 2], ENCODING_LINEAR);
        let linear_out = LookTableApplyContext::new(&linear)
            .expect("2.5-D linear context")
            .apply([0.5, 0.5, 0.5])
            .expect("apply");
        assert_rgb_close(linear_out, [0.5; 3], IDENTITY_NO_OP_TOLERANCE, "linear 2.5-D grey");
        assert!(
            (output[0] - linear_out[0]).abs() > 0.2,
            "the 2.5-D sRGB branch must decode V ({} vs linear {}); an encode round trip \
             would collapse both back to 0.5",
            output[0],
            linear_out[0]
        );
    }

    #[test]
    fn srgb_encoding_uses_the_transfer_curve_on_v_only() {
        // The transfer curve must round-trip on the V coordinate...
        for value in [0.0f32, 0.0031308, 0.04045, 0.18, 0.5, 1.0] {
            let round_trip = srgb_decode(srgb_encode(value));
            assert_close(round_trip, value, 1e-6, &format!("sRGB round trip of {value}"));
        }

        // ...and the stage must feed the ENCODED V into a value-varying table.
        // valDivisions == 2 gives a real value axis; valScale ramps 1 -> 2 with V.
        let entries = vec![
            [0.0, 1.0, 1.0], // (v0, h0, s0)
            [0.0, 1.0, 1.0], // (v0, h0, s1)
            [0.0, 1.0, 2.0], // (v1, h0, s0)
            [0.0, 1.0, 2.0], // (v1, h0, s1)
        ];

        let encoded = make_table(1, 2, 2, entries.clone(), ENCODING_SRGB);
        assert_eq!(encoded.val_divisions, 2, "the value axis must be interpolated");
        let output = LookTableApplyContext::new(&encoded)
            .expect("sRGB context")
            .apply([0.25, 0.25, 0.25])
            .expect("apply");

        // Hand-computed: V == 0.25, encode(0.25) = 0.5370987, so valScale =
        // 1 + 0.5370987 = 1.5370987, the encoded value becomes 0.8255738 and
        // decode(0.8255738) = 0.6480849.
        assert_rgb_close(output, [0.6480849; 3], 1e-4, "encoded V through a value-varying table");

        // encoding == 0 indexes the SAME table with V unencoded: valScale = 1.25
        // and no decode, so the stage must land on 0.25 * 1.25 = 0.3125.
        let linear = make_table(1, 2, 2, entries, ENCODING_LINEAR);
        let linear_out = LookTableApplyContext::new(&linear)
            .expect("linear context")
            .apply([0.25, 0.25, 0.25])
            .expect("apply");
        assert_rgb_close(linear_out, [0.3125; 3], 1e-4, "unencoded V through the same table");

        // Non-vacuity: the two encodings disagree materially and both moved the
        // grey, so neither assertion above can hold for a stage that ignored
        // `encoding` or a table that ignored V.
        assert!(
            (output[0] - linear_out[0]).abs() > 0.3,
            "encoding 1 (V sRGB-encoded) must differ materially from encoding 0, got {} vs {}",
            output[0],
            linear_out[0]
        );
        assert!(
            linear_out[0] > 0.3 && output[0] > 0.6,
            "the value-varying table must raise the 0.25 grey (linear {}, encoded {})",
            linear_out[0],
            output[0]
        );
    }

    // ------------------------------------------------------------------- HSV

    #[test]
    fn non_overrange_rgb_to_hsv_preserves_negative_components() {
        // The non-overrange reference path calls the raw DNG_RGBtoHSV
        // (dng_reference.cpp:1582). The `Max_real32 (…, 0.0f)` pins at
        // dng_reference.cpp:1567-1569 are inside `if (supportOverrange)`, so a
        // below-zero ProPhoto component is carried through unpinned.
        // r == V, so H = (g - b) / gap = -0.3 / 0.7 -> 5.5714286 and S = gap / V = 1.4.
        let [h, s, v] = linear_rgb_to_hsv([0.5, -0.2, 0.1]);
        assert_close(v, 0.5, 1e-6, "V is the maximum component");
        assert_close(s, 1.4, 1e-5, "S = gap / V exceeds 1 for a below-zero component");
        assert_close(h, 5.5714286, 1e-5, "H is built from the signed gap");

        // Non-vacuity: pinning the negative to zero first (i.e.
        // DNG_PinnedNonnegativeRGBtoHSV, dng_utils.h:915) would cap S at 1.0 and
        // move H to 5.8, so this distinguishes the pinned and unpinned paths.
        assert!(s > 1.0, "a pinned negative would cap S at 1.0, got {s}");
        assert_close(linear_rgb_to_hsv([0.5, 0.0, 0.1])[0], 5.8, 1e-5, "pinned H would differ");
    }

    #[test]
    fn rgb_to_hsv_covers_all_negative_and_equal_negative_vectors() {
        // All-negative vector: V is itself negative, so `S = gap / V` is NEGATIVE.
        // r == V, so H = (g - b) / gap = (-0.3 + 0.2) / 0.2 = -0.5, folded to +6.
        let [h, s, v] = linear_rgb_to_hsv([-0.1, -0.3, -0.2]);
        assert_close(v, -0.1, 1e-6, "V is the maximum (least-negative) component");
        assert_close(s, -2.0, 1e-6, "S = gap / V is negative when V < 0");
        assert_close(h, 5.5, 1e-6, "H folds the negative (g-b)/gap into [0,6)");
        assert!(s < 0.0, "an all-negative vector must yield a negative S, got {s}");

        // Equal-negative vector: gap == 0 takes the `[0, 0, V]` branch.
        let [h, s, v] = linear_rgb_to_hsv([-0.1, -0.1, -0.1]);
        assert_close(v, -0.1, 1e-6, "V is the common component");
        assert_eq!(s, 0.0, "a zero gap yields S = 0");
        assert_eq!(h, 0.0, "a zero gap yields H = 0");

        // Non-vacuity: the three vectors must be distinguishable, so the
        // assertions above are not all testing the same branch.
        assert_ne!(
            linear_rgb_to_hsv([-0.1, -0.3, -0.2]),
            linear_rgb_to_hsv([-0.1, -0.1, -0.1])
        );
    }

    /// The whole LookTable pipeline must stay finite for out-of-gamut, all-negative
    /// input: the hop, the unpinned RGB->HSV (negative V and S), the lookup and the
    /// HSV->RGB round trip must never produce NaN/Inf, for a Linear and an
    /// sRGB-encoded table.
    #[test]
    fn look_table_apply_returns_finite_pixels_for_negative_input() {
        let tables = [
            identity_table(36, 16, 16),
            make_table(2, 2, 2, vec![[15.0, 1.5, 0.5]; 8], ENCODING_SRGB),
        ];

        for table in tables {
            let context = LookTableApplyContext::new(&table).expect("valid context");

            for input in [
                [-0.1f32, -0.3, -0.2],
                [-0.5, -0.5, -0.5],
                [-1.0e6, -2.0e6, -3.0e6],
            ] {
                let output = context
                    .apply(input)
                    .unwrap_or_else(|error| panic!("negative input {input:?}: {error}"));

                for channel in output {
                    assert!(
                        channel.is_finite(),
                        "negative input {input:?} produced non-finite {output:?}"
                    );
                }
            }
        }
    }

    // ---------------------------------------------------------------- errors

    #[test]
    fn rejects_non_finite_input_on_every_channel() {
        let table = identity_table(36, 16, 16);

        for channel in 0..3 {
            for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut input = [0.5f32, 0.5, 0.5];
                input[channel] = bad;

                let error = LookTableApplyContext::new(&table)
                    .expect("context")
                    .apply(input)
                    .expect_err("non-finite input on any channel must be rejected");
                assert_eq!(
                    error, "LookTable input contains non-finite value",
                    "channel {channel} with {bad}"
                );
            }
        }

        // Non-vacuity: the same probe renders when finite.
        assert!(
            LookTableApplyContext::new(&table)
                .expect("context")
                .apply([0.5, 0.5, 0.5])
                .is_ok()
        );
    }

    #[test]
    fn rejects_unsupported_encoding() {
        let table = make_table(1, 2, 1, vec![[0.0, 1.0, 1.0]; 2], 2);
        let error = LookTableApplyContext::new(&table).err().expect("encoding 2 must be rejected");
        assert_eq!(error, "unsupported LookTable encoding: 2");
    }

    #[test]
    fn rejects_out_of_bounds_divisions() {
        let base = vec![[0.0f32, 1.0, 1.0]; 4];

        let mut hue_zero = make_table(1, 2, 2, base.clone(), ENCODING_LINEAR);
        hue_zero.hue_divisions = 0;
        assert_eq!(
            LookTableApplyContext::new(&hue_zero).err().expect("hue 0"),
            "unsupported LookTable hue divisions: 0"
        );

        let mut sat_one = make_table(1, 2, 2, base.clone(), ENCODING_LINEAR);
        sat_one.sat_divisions = 1;
        assert_eq!(
            LookTableApplyContext::new(&sat_one).err().expect("sat 1"),
            "unsupported LookTable sat divisions: 1"
        );

        let mut val_zero = make_table(1, 2, 2, base, ENCODING_LINEAR);
        val_zero.val_divisions = 0;
        assert_eq!(
            LookTableApplyContext::new(&val_zero).err().expect("val 0"),
            "unsupported LookTable val divisions: 0"
        );
    }

    #[test]
    fn rejects_malformed_entry_count() {
        let mut table = identity_table(2, 2, 2);
        table.entries.pop();
        let error = LookTableApplyContext::new(&table).err().expect("short entry list");
        assert!(
            error.contains("malformed LookTable: expected 8 entries, got 7"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_non_finite_entry_without_touching_pixels() {
        let mut table = identity_table(2, 2, 2);
        table.entries[5] = [f32::NAN, 1.0, 1.0];

        let error =
            LookTableApplyContext::new(&table).err().expect("non-finite entry must be rejected");
        assert_eq!(error, "LookTable contains non-finite entry at index 5");
    }

    #[test]
    fn context_validates_table_only_once_for_multiple_pixels() {
        reset_full_table_validations();

        let table = identity_table(36, 16, 16);
        let context = LookTableApplyContext::new(&table).expect("valid context");

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
    fn context_apply_matches_apply_look_table() {
        let inputs = [
            [0.0f32, 0.0, 0.0],
            [0.18, 0.5, 0.9],
            [1.0, 1.0, 1.0],
            [0.05, 0.10, 0.15],
            [0.9, 0.4, 0.1],
        ];

        let mut gradient = Vec::new();
        for _v in 0..2 {
            for hue in 0..3u32 {
                for sat in 0..2u32 {
                    gradient.push([hue as f32 * 5.0, 1.0 + sat as f32, 1.5]);
                }
            }
        }

        let tables = [
            identity_table(36, 16, 16),
            constant_table(2, 2, 2, [15.0, 0.5, 1.25]),
            make_table(3, 2, 2, gradient, ENCODING_LINEAR),
            make_table(3, 2, 2, vec![[0.0, 1.0, 1.0]; 12], ENCODING_SRGB),
        ];

        for table in tables {
            let context = LookTableApplyContext::new(&table).expect("valid context");

            for input in inputs {
                let from_context = context.apply(input).expect("context apply");
                let from_wrapper = apply_look_table(input, &table).expect("wrapper apply");

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

        let table = make_table(1, 2, 1, vec![[0.0, 1.0, 1.0]; 2], 7);

        let result = LookTableApplyContext::new(&table);
        assert_eq!(
            result.err().expect("unsupported encoding"),
            "unsupported LookTable encoding: 7"
        );

        // A rejected table must not even reach the full-table scan.
        assert_eq!(
            full_table_validations(),
            0,
            "invalid metadata should be rejected before the full table scan"
        );
    }
}
