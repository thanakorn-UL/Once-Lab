//! Shared linear-RGB linear-algebra toolbox for the XMP profile renderers.
//!
//! Both render stages bracket their per-pixel work with a 3x3 hop between
//! Once-Lab's **linear sRGB / D65** working space and the primaries the table is
//! authored in:
//!
//! * the LookTable stage ([`super::look_table_apply`]) always hops
//!   `linear sRGB (D65) <-> linear ProPhoto (D50)`, the DNG "RIMM" connection
//!   space the LookTable is defined in; and
//! * the RGBTable stage ([`super::color_space`]) hops
//!   `linear sRGB (D65) <-> table primaries` for whichever `primaries_enum` the
//!   table declares.
//!
//! Those two hops are the *same construction*, so this module owns it exactly
//! once: the Adobe D50 PCS white, the raw device->PCS constructor constants, the
//! [`set_matrix_to_pcs`] row normalisation, the 3x3 multiply and inverse, and the
//! f64 -> f32 narrowing used on the per-pixel fast path. Both stages derive their
//! matrices from here rather than each carrying a private copy, so a future edit
//! (e.g. a white-point fix) can only ever change one place.
//!
//! Everything is copied verbatim from the previously-verified 5A/5B code
//! (`color_space.rs` was the owner of the pinned VALUES), so the refactor is
//! numerically inert: the same f64 expressions in the same order produce
//! bit-identical results. `linear_rgb_hop_matches_both_stages` pins that the
//! composed ProPhoto hop is the *same matrix* the RGBTable stage builds for
//! `primaries_ProPhoto`.

/// A row-major 3x3 matrix used on the per-pixel fast path.
pub(super) type Matrix3 = [[f32; 3]; 3];

/// A row-major 3x3 matrix used while deriving the PCS-normalised transforms.
pub(super) type Matrix3F64 = [[f64; 3]; 3];

/// Adobe PCS white chromaticity: `D50_xy_coord()` (`dng_xy_coord.h:145`).
pub(super) const D50_XY: [f64; 2] = [0.3457, 0.3585];

/// `dng_space_sRGB` constructor constants (`dng_color_space.cpp:257`); already
/// Bradford-adapted to the D50 PCS. Also the working-space white anchor.
pub(super) const SRGB_RAW: Matrix3F64 = [
    [0.4361, 0.3851, 0.1431],
    [0.2225, 0.7169, 0.0606],
    [0.0139, 0.0971, 0.7141],
];

/// `dng_space_AdobeRGB` constructor constants (`dng_color_space.cpp:565-572`).
pub(super) const ADOBE_RGB_RAW: Matrix3F64 = [
    [0.6097, 0.2053, 0.1492],
    [0.3111, 0.6257, 0.0632],
    [0.0195, 0.0609, 0.7446],
];

/// `dng_space_ProPhoto` constructor constants (`dng_color_space.cpp:993-1000`).
pub(super) const PROPHOTO_RAW: Matrix3F64 = [
    [0.7977, 0.1352, 0.0313],
    [0.2880, 0.7119, 0.0001],
    [0.0000, 0.0000, 0.8249],
];

/// `dng_space_DisplayP3` constructor constants (`dng_color_space.cpp:781-783`,
/// the live `#else` branch; the `#if 0` chromaticity-derived block is dead).
pub(super) const DISPLAY_P3_RAW: Matrix3F64 = [
    [0.5151, 0.2920, 0.1571],
    [0.2412, 0.6922, 0.0666],
    [-0.0010, 0.0419, 0.7843],
];

/// `dng_space_Rec2020` constructor constants (`dng_color_space.cpp:896-898`,
/// the live `#else` branch).
pub(super) const REC2020_RAW: Matrix3F64 = [
    [0.6735, 0.1657, 0.1251],
    [0.2791, 0.6753, 0.0456],
    [-0.0019, 0.0300, 0.7971],
];

/// The Adobe PCS white `PCStoXYZ()` = `XYtoXYZ(D50_xy_coord())`.
pub(super) fn pcs_white() -> [f64; 3] {
    let [x, y] = D50_XY;
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// Replicates `dng_color_space::SetMatrixToPCS` (`dng_color_space.cpp:205-228`).
///
/// The published matrices are rounded, so Adobe rescales each row
/// (`scale_i = W2_i / (M * (1,1,1))_i`) to make device white `(1,1,1)` land
/// EXACTLY on the PCS white. Because every primaries set is normalised to the
/// *same* D50 white, any neutral `(t,t,t)` maps to `t * D50` in both spaces and
/// therefore survives the device-to-device hop unchanged.
pub(super) fn set_matrix_to_pcs(raw: Matrix3F64) -> Matrix3F64 {
    let white = pcs_white();
    let mut out = [[0.0f64; 3]; 3];

    for row in 0..3 {
        let sum = raw[row][0] + raw[row][1] + raw[row][2];
        let scale = white[row] / sum;
        for col in 0..3 {
            out[row][col] = raw[row][col] * scale;
        }
    }

    out
}

pub(super) fn invert3(m: Matrix3F64) -> Matrix3F64 {
    let [[a, b, c], [d, e, f], [g, h, i]] = m;

    let cof_a = e * i - f * h;
    let cof_b = -(d * i - f * g);
    let cof_c = d * h - e * g;

    let det = a * cof_a + b * cof_b + c * cof_c;

    [
        [cof_a / det, -(b * i - c * h) / det, (b * f - c * e) / det],
        [cof_b / det, (a * i - c * g) / det, -(a * f - c * d) / det],
        [cof_c / det, -(a * h - b * g) / det, (a * e - b * d) / det],
    ]
}

pub(super) fn mul3_f64(a: Matrix3F64, b: Matrix3F64) -> Matrix3F64 {
    let mut out = [[0.0f64; 3]; 3];
    for row in 0..3 {
        for col in 0..3 {
            out[row][col] = (0..3).map(|k| a[row][k] * b[k][col]).sum();
        }
    }
    out
}

pub(super) fn to_f32(m: Matrix3F64) -> Matrix3 {
    let mut out = [[0.0f32; 3]; 3];
    for row in 0..3 {
        for col in 0..3 {
            out[row][col] = m[row][col] as f32;
        }
    }
    out
}

/// Applies a row-major 3x3 matrix to a column vector.
pub(super) fn mul3(m: Matrix3, v: [f32; 3]) -> [f32; 3] {
    let mut out = [0.0f32; 3];
    for row in 0..3 {
        out[row] = m[row][0] * v[0] + m[row][1] * v[1] + m[row][2] * v[2];
    }
    out
}

/// `M_working -> table primaries` in f64 =
/// `Invert(SetMatrixToPCS(table_raw)) * SetMatrixToPCS(working_raw)`.
///
/// This is Once-Lab's generalisation of `dng_big_table.cpp:5244-5251`
/// (`fEncodeMatrix = table.MatrixFromPCS() * ProPhoto.MatrixToPCS()`) with
/// Once-Lab's working space (linear sRGB / D65) as the source instead of Adobe's
/// built-in linear ProPhoto. Both render stages call it: the RGBTable stage with
/// `table_raw = raw_primaries_matrix(primaries)`, and the LookTable stage with
/// `table_raw = PROPHOTO_RAW`.
pub(super) fn compose_working_to_primaries(
    table_raw: Matrix3F64,
    working_raw: Matrix3F64,
) -> Matrix3F64 {
    let table = set_matrix_to_pcs(table_raw);
    let working = set_matrix_to_pcs(working_raw);
    mul3_f64(invert3(table), working)
}

#[cfg(test)]
mod tests {
    use super::super::color_space::{Primaries, PrimariesTransform};
    use super::super::look_table_apply::{
        linear_prophoto_to_linear_srgb, linear_srgb_to_linear_prophoto,
    };
    use super::*;

    /// The shared composition and the RGBTable stage's `primaries_ProPhoto`
    /// matrix must be the *same construction*: both call
    /// [`compose_working_to_primaries`] with `PROPHOTO_RAW` as the table
    /// primaries and `SRGB_RAW` as the working space. If they ever disagree it
    /// means one of the two hops was built differently, which is exactly the
    /// divergence the 5A/5B code must not contain.
    #[test]
    fn linear_rgb_hop_matches_both_stages() {
        let shared_forward = to_f32(compose_working_to_primaries(PROPHOTO_RAW, SRGB_RAW));
        let shared_inverse = to_f32(invert3(compose_working_to_primaries(PROPHOTO_RAW, SRGB_RAW)));

        // (1) The LookTable stage's hop is the shared one, exactly.
        assert_eq!(
            linear_srgb_to_linear_prophoto(),
            shared_forward,
            "look_table_apply forward hop must be the shared composition"
        );
        assert_eq!(
            linear_prophoto_to_linear_srgb(),
            shared_inverse,
            "look_table_apply inverse hop must be the shared composition"
        );

        // (2) The RGBTable stage builds the IDENTICAL matrix for
        // `primaries_ProPhoto`. Bit-for-bit: same f64 expression, same order.
        let transform = PrimariesTransform::new(Primaries::ProPhoto);
        let cs_encode = transform
            .encode_matrix()
            .expect("primaries_ProPhoto must not use the identity shortcut");
        let cs_decode = transform
            .decode_matrix()
            .expect("primaries_ProPhoto must not use the identity shortcut");

        assert_eq!(
            cs_encode, shared_forward,
            "color_space primaries_ProPhoto encode must equal the shared ProPhoto hop"
        );
        assert_eq!(
            cs_decode, shared_inverse,
            "color_space primaries_ProPhoto decode must equal the shared ProPhoto hop inverse"
        );

        // Stated tight tolerance, so the intent is legible even though the
        // construction is bit-identical: agree to within 0 ULP.
        const HOP_MATRIX_TOLERANCE: f32 = 0.0;
        let mut max_delta = 0.0f32;
        for row in 0..3 {
            for col in 0..3 {
                max_delta = max_delta.max((cs_encode[row][col] - shared_forward[row][col]).abs());
                assert!(
                    (cs_encode[row][col] - shared_forward[row][col]).abs() <= HOP_MATRIX_TOLERANCE,
                    "ProPhoto hop[{row}][{col}] differs: shared {} vs color_space {}",
                    shared_forward[row][col],
                    cs_encode[row][col]
                );
            }
        }
        assert_eq!(max_delta, 0.0, "the two ProPhoto hops must be bit-identical");

        // Non-vacuity: the composed hop is a real matrix, not an identity (so the
        // equality above is not trivially satisfied by two identity matrices).
        let moved = mul3(shared_forward, [1.0, 0.0, 0.0]);
        assert!(
            (moved[0] - 1.0).abs() > 0.1 || (moved[1] - 0.0).abs() > 0.1,
            "the ProPhoto hop must actually move pure red, got {moved:?}"
        );
    }
}
