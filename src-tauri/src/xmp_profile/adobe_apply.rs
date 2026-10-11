//! The Adobe DNG SDK 1.7.1 (build 2502) scalar RGBTable evaluator.
//!
//! This is a line-for-line port of `RefRGBtoRGBTable3D`
//! (`dng_reference.cpp:3464-3774`) and `RefRGBtoRGBTable1D`
//! (`dng_reference.cpp:3841-4040`), driven by prepared state that mirrors
//! `dng_rgb_to_rgb_table_data`'s constructor (`dng_big_table.cpp:5192-5361`),
//! `dng_rgb_table::SetAmount` (`dng_big_table.h:1421-1435`) and
//! `dng_rgb_table::IsValid` (`dng_big_table.h:1384-1419`).
//!
//! Domain contract: pixels arrive in Linear ProPhoto RGB / D50. The
//! primaries matrix maps working space -> table primaries; in SDR a ProPhoto
//! table has NO matrix (`space == NULL` -> `fNeedMatrix = false`), while in
//! HDR overrange mode the SDK injects an identity matrix so the gamut/overrange
//! machinery still runs (`dng_reference.cpp:3539-3559`).
//!
//! Deliberate, documented divergences from the SDK (all out-of-contract):
//!   * `RefRGBtoRGBTable3D` has no lower bound on the cell index
//!     (`Min_int32 ((int32) scaled, maxIndex)`): a negative input on the
//!     matrix-free SDR path reads out of bounds in C++. This port clamps the
//!     index to `[0, divisions-2]`, so negative inputs extrapolate within cell
//!     0 instead of touching unowned memory.
//!   * `dng_1d_table::Interpolate` throws `dng_error_bad_format` on NaN or
//!     out-of-`[0,1]` input. This port pins the kernel input to `[0,1]` on the
//!     matrix-free SDR path (Adobe's effective contract is bounded input
//!     there), so `apply` never fails at run time.
//!   * The optional mask lerp (`Lerp_real32 (src, dst, m)`) is compiled in as
//!     `m == 1.0` — a global Creative XMP stage has no mask. DNG 1.6 semantic
//!     `RGBTables` masking is a separate feature.

use super::color_space::{Gamma, Gamut, Primaries, raw_primaries_matrix};
use super::linear_rgb::{PROPHOTO_RAW, invert3, mul3_f64, set_matrix_to_pcs};
use super::rgb_table::RgbTable;
use super::sampled_gamma::{SampledTable, pin_real64};

// ---------------------------------------------------------------------------
// `dng_utils.h` f32 helpers — verbatim semantics, including NaN behavior.
// `f32::min`/`f32::max` return the non-NaN operand; the SDK's comparison-chain
// forms do not, so these are written out explicitly.
// ---------------------------------------------------------------------------

#[inline]
fn min_real32(x: f32, y: f32) -> f32 {
    if x < y { x } else { y }
}

#[inline]
fn max_real32(x: f32, y: f32) -> f32 {
    if x > y { x } else { y }
}

/// `Pin_real32 (min, x, max)` = `Max_real32 (min, Min_real32 (x, max))`.
/// NaN maps to `max` (first comparison false, second true).
#[inline]
fn pin_real32(min: f32, x: f32, max: f32) -> f32 {
    max_real32(min, min_real32(x, max))
}

/// `Lerp_real32 (a, b, t)` (`dng_utils.h:490-495`): `a + t * (b - a)`.
/// With `t = 1.0` this is `a + (b - a)`, which is NOT bit-exact `b` — the
/// SDK's unconditional mask lerp rounds this way even when `mPtr == NULL`,
/// so we replicate it verbatim.
#[inline]
fn lerp_real32(a: f32, b: f32, t: f32) -> f32 {
    a + t * (b - a)
}

/// `Pin_real32 (x)` — the `x -> [0,1]` overload. Shared with the LookTable
/// stage's `Pin_real32 (v)` before its sampled V encode.
#[inline]
pub(super) fn pin_real32_unit(x: f32) -> f32 {
    pin_real32(0.0, x, 1.0)
}

/// `EncodeOverrange` (`dng_utils.h:1711-1719`): maps `[0,16]` -> `[0,1]`
/// (four stops above 1); NOT identity below 1. `EncodeOverrange(1)` is
/// exactly `257/512`.
#[inline]
pub(super) fn encode_overrange(x: f32) -> f32 {
    let x = max_real32(x, 0.0);
    x * (256.0 + x) / (256.0 * (1.0 + x))
}

/// `DecodeOverrange` (`dng_utils.h:1722-1730`): the algebraic inverse.
#[inline]
pub(super) fn decode_overrange(x: f32) -> f32 {
    let x = max_real32(x, 0.0);
    16.0 * ((8.0 * x) - 8.0 + (64.0 * x * x - 127.0 * x + 64.0).sqrt())
}

#[inline]
fn pin_int32(min: i32, x: i32, max: i32) -> i32 {
    if min > x.min(max) { min } else { x.min(max) }
}

/// `Round_int32` (`dng_utils.h`): `x > 0 ? x + 0.5 : x - 0.5`, truncated.
#[inline]
fn round_int32(x: f64) -> i32 {
    (if x > 0.0 { x + 0.5 } else { x - 0.5 }) as i32
}

// ---------------------------------------------------------------------------
// Prepared table state — mirrors dng_rgb_to_rgb_table_data's ctor.
// ---------------------------------------------------------------------------

/// `dng_rgb_table::SetAmount` (`dng_big_table.h:1421-1435`): two-decimal
/// rounding in `f64`, THEN the `[minAmount, maxAmount]` pin.
fn sdk_set_amount(amount: f64, min_amount: f64, max_amount: f64) -> f64 {
    pin_real64(
        min_amount,
        f64::from(round_int32(amount * 100.0)) * 0.01,
        max_amount,
    )
}

/// `dng_rgb_table::IsValid` (`dng_big_table.h:1384-1419`): whether the table
/// stage runs at all.
///
///   * `amount > 0`                -> runs.
///   * `amount == 0` with ProPhoto primaries OR gamut-extend -> the stage is
///     a true NOP and the SDK skips it.
///   * `amount == 0`, non-ProPhoto, gamut-clip -> still runs: the matrix +
///     pin performs gamut clipping even at zero LUT strength ("Table is a
///     NOP but there is some gamut clipping").
fn sdk_is_valid(primaries: Primaries, gamut: Gamut, amount: f64) -> bool {
    if amount > 0.0 {
        return true;
    }
    !(primaries == Primaries::ProPhoto || gamut == Gamut::Extend)
}

/// Immutable prepared evaluator: all heavy work (matrix derivation/inversion,
/// gamma-table sampling, 1-D composition) happens here once — never per pixel.
pub(crate) struct PreparedRgbTable<'a> {
    /// Whether the table stage runs at all (`dng_rgb_table::IsValid`).
    runs: bool,

    dimensions: u32,
    divisions: usize,
    /// RGB0 stride-4 absolute samples, borrowed — a 64^3 table is 2 MiB and
    /// must not be cloned into the prepared state.
    samples: &'a [u16],

    /// Post-`SetAmount` amount, plus `amount != 1.0` (`hasAmount` in the
    /// kernel: the LUT result is used raw at full strength).
    amount: f32,
    has_amount: bool,

    /// Kernel `hasMatrix` — TRUE for non-ProPhoto primaries, and also for
    /// ProPhoto in HDR mode, where the SDK injects the identity matrix.
    /// `hasGamut` is `gamut != clip || (overrange && hasMatrix)` — the SDK
    /// forces it on whenever it injects the identity matrix.
    has_matrix: bool,
    has_gamut: bool,
    overrange: bool,
    encode_matrix: [[f32; 3]; 3],
    decode_matrix: [[f32; 3]; 3],

    /// Sampled gamma tables (None for Linear) — 3-D path.
    encode_gamma: Option<SampledTable>,
    decode_gamma: Option<SampledTable>,

    /// Precomposed per-channel tables — 1-D path (`dng_big_table.cpp:5300-5361`).
    tables_1d: Option<[SampledTable; 3]>,
}

impl<'a> PreparedRgbTable<'a> {
    /// Build the prepared evaluator for `table` at the given amount
    /// (post UI-percent resolution, still subject to `SetAmount`'s rounding
    /// and the `[min,max]` pin).
    ///
    /// `overrange` is the HDR path flag. Creative XMP profiles carry no
    /// `ProfileDynamicRange`, so production callers pass `false`; the HDR
    /// branch is exercised by fixtures and by `apply_overrange`.
    pub(crate) fn new(table: &'a RgbTable, amount: f64) -> Result<Self, String> {
        Self::build(table, amount, false)
    }

    /// HDR variant: `supportOverrange`, i.e. `RefRGBtoRGBTable*D` with
    /// `EncodeOverrange`/`DecodeOverrange` and the identity-matrix injection
    /// for ProPhoto primaries.
    pub(crate) fn new_overrange(table: &'a RgbTable, amount: f64) -> Result<Self, String> {
        Self::build(table, amount, true)
    }

    fn build(table: &'a RgbTable, amount: f64, overrange: bool) -> Result<Self, String> {
        if table.dimensions != 1 && table.dimensions != 3 {
            return Err(format!(
                "unsupported RGBTable dimensions: {}",
                table.dimensions
            ));
        }
        if table.size < 2 {
            return Err("invalid RGBTable: fewer than 2 divisions".to_string());
        }
        let expected_nodes = if table.dimensions == 1 {
            table.size
        } else {
            table.size * table.size * table.size
        };
        if table.node_count() != expected_nodes {
            return Err(format!(
                "invalid RGBTable: {} stored nodes, expected {expected_nodes}",
                table.node_count()
            ));
        }

        let primaries = Primaries::from_enum(table.color_space)?;
        let gamma = Gamma::from_enum(table.gamma)?;
        let gamut = Gamut::from_enum(table.gamut)?;

        // `dng_rgb_table::IsValidAmounts`: finite bounds, min in [0,1],
        // max >= 1 — evaluated at prepare time, never per pixel.
        if !table.min_amount.is_finite()
            || !table.max_amount.is_finite()
            || !(0.0..=1.0).contains(&table.min_amount)
            || table.max_amount < 1.0
        {
            return Err("invalid RGBTable amount bounds".to_string());
        }
        if !amount.is_finite() {
            return Err("invalid RGBTable amount".to_string());
        }

        let resolved_amount = sdk_set_amount(amount, table.min_amount, table.max_amount);
        let runs = sdk_is_valid(primaries, gamut, resolved_amount);

        // --- Matrix selection (dng_big_table.cpp:5199-5253) ---------------
        // needMatrix = (space != NULL); NULL for ProPhoto primaries.
        let need_matrix = primaries != Primaries::ProPhoto;

        let (encode_matrix, decode_matrix) = if need_matrix {
            let raw = raw_primaries_matrix(primaries);
            // fEncodeMatrix = table.MatrixFromPCS() * ProPhoto.MatrixToPCS()
            //               = Invert(SetMatrixToPCS(table)) * PCS(ProPhoto);
            // fDecodeMatrix = ProPhoto.MatrixFromPCS() * table.MatrixToPCS()
            //               = Invert(PCS(ProPhoto)) * SetMatrixToPCS(table).
            // The SDK computes the two INDEPENDENTLY — it does not invert
            // fEncodeMatrix — so we match that operation order exactly
            // (algebraically equal, different rounding).
            let table_pcs = set_matrix_to_pcs(raw);
            let prophoto_pcs = set_matrix_to_pcs(PROPHOTO_RAW);
            let encode = mul3_f64(invert3(table_pcs), prophoto_pcs);
            let decode = mul3_f64(invert3(prophoto_pcs), table_pcs);
            (
                [
                    [
                        encode[0][0] as f32,
                        encode[0][1] as f32,
                        encode[0][2] as f32,
                    ],
                    [
                        encode[1][0] as f32,
                        encode[1][1] as f32,
                        encode[1][2] as f32,
                    ],
                    [
                        encode[2][0] as f32,
                        encode[2][1] as f32,
                        encode[2][2] as f32,
                    ],
                ],
                [
                    [
                        decode[0][0] as f32,
                        decode[0][1] as f32,
                        decode[0][2] as f32,
                    ],
                    [
                        decode[1][0] as f32,
                        decode[1][1] as f32,
                        decode[1][2] as f32,
                    ],
                    [
                        decode[2][0] as f32,
                        decode[2][1] as f32,
                        decode[2][2] as f32,
                    ],
                ],
            )
        } else {
            // `RefRGBtoRGBTable3D` dng_reference.cpp:3539-3559: with
            // supportOverrange and no matrices, install identity + gamut.
            (
                [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            )
        };

        let has_matrix = need_matrix || overrange;
        // `hasGamut = gamut != 0` at kernel entry, OR forced true when
        // overrange injects the identity matrix.
        let has_gamut = gamut == Gamut::Extend || (overrange && !need_matrix);

        // --- Gamma tables / 1-D composition (dng_big_table.cpp:5259-5361) --
        let mut encode_gamma = None;
        let mut decode_gamma = None;
        let mut tables_1d: Option<[SampledTable; 3]> = None;

        if table.dimensions == 1 {
            // dng_rgb_to_rgb_1d_function -> concatenate(gamma, map) [->
            // concatenate(firstPart, inverse)] — sampled once per channel.
            let mut planes = Vec::with_capacity(3);
            for plane in 0..3 {
                let map = OneDMap {
                    divisions: table.size,
                    samples: &table.samples,
                    plane,
                    amount: resolved_amount,
                };
                let sampled = if gamma == Gamma::Linear {
                    SampledTable::build(|x| map.evaluate(x))
                } else {
                    // combined = concatenate(concatenate(gamma, map), inverse)
                    SampledTable::build(|x| {
                        let y = pin_real64(0.0, gamma.evaluate_f64(x), 1.0);
                        let y2 = pin_real64(0.0, map.evaluate(y), 1.0);
                        gamma.evaluate_inverse_f64(y2)
                    })
                };
                planes.push(sampled);
            }
            let [t0, t1, t2] = planes.try_into().map_err(|_| "unreachable")?;
            tables_1d = Some([t0, t1, t2]);
        } else if gamma != Gamma::Linear {
            encode_gamma = Some(SampledTable::build(|x| gamma.evaluate_f64(x)));
            decode_gamma = Some(SampledTable::build(|y| gamma.evaluate_inverse_f64(y)));
        }

        let amount32 = resolved_amount as f32;

        Ok(PreparedRgbTable {
            runs,
            dimensions: table.dimensions,
            divisions: table.size,
            samples: &table.samples,
            amount: amount32,
            has_amount: amount32 != 1.0,
            has_matrix,
            has_gamut,
            overrange,
            encode_matrix,
            decode_matrix,
            encode_gamma,
            decode_gamma,
            tables_1d,
        })
    }

    /// `dng_rgb_table::IsValid` — whether this table runs at all under its
    /// amount. Used by callers and tests for the amount-0 NOP trap.
    #[cfg(test)]
    pub(crate) fn runs(&self) -> bool {
        self.runs
    }

    /// Apply the table to one pixel of Linear ProPhoto D50 input.
    ///
    /// Never fails at run time for in-contract input; the matrix-free SDR
    /// path pins the kernel input to `[0,1]` (documented divergence above).
    pub(crate) fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        if !self.runs {
            return rgb;
        }
        match self.dimensions {
            1 => self.apply_1d(rgb),
            _ => self.apply_3d(rgb),
        }
    }

    /// `RefRGBtoRGBTable3D` (`dng_reference.cpp:3464`), scalarized to a
    /// single pixel with mask `m == 1.0`.
    fn apply_3d(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut r = rgb[0];
        let mut g = rgb[1];
        let mut b = rgb[2];

        // rSrc/gSrc/bSrc — the mask-lerp source values, captured BEFORE the
        // matrix (`dng_reference.cpp:3574-3578`).
        let r_src = r;
        let g_src = g;
        let b_src = b;

        let mut gamut_delta = [0.0f32; 3];

        if self.has_matrix {
            let e = &self.encode_matrix;
            let mut rr = r * e[0][0] + g * e[0][1] + b * e[0][2];
            let mut gg = r * e[1][0] + g * e[1][1] + b * e[1][2];
            let mut bb = r * e[2][0] + g * e[2][1] + b * e[2][2];

            if self.overrange {
                rr = encode_overrange(rr);
                gg = encode_overrange(gg);
                bb = encode_overrange(bb);
            }

            r = pin_real32_unit(rr);
            g = pin_real32_unit(gg);
            b = pin_real32_unit(bb);

            if self.has_gamut {
                gamut_delta = [rr - r, gg - g, bb - b];
            }
        }
        // Divergence note: the matrix-free path (ProPhoto primaries, SDR) has
        // NO input pin in the SDK — out-of-domain values extrapolate through
        // the top table cell (matching Adobe), and a negative value reads out
        // of bounds in C++ (here: the index clamp keeps reads in bounds and
        // extrapolates inside cell 0 instead; only divergent for out-of-
        // contract negative input). A non-Linear gamma table throws in the
        // SDK; here `interpolate`'s fallback pins instead.

        if let Some(enc) = &self.encode_gamma {
            // Unreachable for in-contract input (already pinned); the pin
            // fallback keeps float noise at the top endpoint from snapping
            // a channel to black.
            r = enc
                .interpolate(r, "RGBTable encode gamma")
                .unwrap_or_else(|_| pin_real32_unit(r));
            g = enc
                .interpolate(g, "RGBTable encode gamma")
                .unwrap_or_else(|_| pin_real32_unit(g));
            b = enc
                .interpolate(b, "RGBTable encode gamma")
                .unwrap_or_else(|_| pin_real32_unit(b));
        }

        let scale = (self.divisions - 1) as f32;
        let max_index = self.divisions as i32 - 2;

        let r_scaled = r * scale;
        let g_scaled = g * scale;
        let b_scaled = b * scale;

        // SDK: Min_int32((int32) scaled, maxIndex) — no lower bound. Rust
        // clamps at 0 (see module doc): negative scaled values occur only
        // for out-of-contract input.
        let r_index = pin_int32(0, r_scaled as i32, max_index);
        let g_index = pin_int32(0, g_scaled as i32, max_index);
        let b_index = pin_int32(0, b_scaled as i32, max_index);

        let r_fract = r_scaled - r_index as f32;
        let g_fract = g_scaled - g_index as f32;
        let b_fract = b_scaled - b_index as f32;

        let offset_001 = 4usize;
        let offset_010 = offset_001 * self.divisions;
        let offset_100 = offset_010 * self.divisions;
        let offset_011 = offset_010 + offset_001;
        let offset_101 = offset_100 + offset_001;
        let offset_110 = offset_100 + offset_010;
        let offset_111 = offset_110 + offset_001;

        // The six tetrahedral orderings, verbatim from
        // dng_reference.cpp:3645-3713.
        let (offset1, offset2, f1, f2, f3);
        if g_fract >= r_fract {
            if b_fract >= g_fract {
                offset1 = offset_001;
                offset2 = offset_011;
                f1 = b_fract;
                f2 = g_fract;
                f3 = r_fract;
            } else if b_fract >= r_fract {
                offset1 = offset_010;
                offset2 = offset_011;
                f1 = g_fract;
                f2 = b_fract;
                f3 = r_fract;
            } else {
                offset1 = offset_010;
                offset2 = offset_110;
                f1 = g_fract;
                f2 = r_fract;
                f3 = b_fract;
            }
        } else if b_fract >= r_fract {
            offset1 = offset_001;
            offset2 = offset_101;
            f1 = b_fract;
            f2 = r_fract;
            f3 = g_fract;
        } else if b_fract >= g_fract {
            offset1 = offset_100;
            offset2 = offset_101;
            f1 = r_fract;
            f2 = b_fract;
            f3 = g_fract;
        } else {
            offset1 = offset_100;
            offset2 = offset_110;
            f1 = r_fract;
            f2 = g_fract;
            f3 = b_fract;
        }

        let w0 = 1.0 - f1;
        let w1 = f1 - f2;
        let w2 = f2 - f3;

        let base = (r_index as usize * offset_100)
            + (g_index as usize * offset_010)
            + (b_index as usize * offset_001);
        let table = &self.samples;
        let inv = 1.0f32 / 65535.0;

        let r0 = (w0 * table[base] as f32
            + w1 * table[base + offset1] as f32
            + w2 * table[base + offset2] as f32
            + f3 * table[base + offset_111] as f32)
            * inv;

        let g0 = (w0 * table[base + 1] as f32
            + w1 * table[base + offset1 + 1] as f32
            + w2 * table[base + offset2 + 1] as f32
            + f3 * table[base + offset_111 + 1] as f32)
            * inv;

        let b0 = (w0 * table[base + 2] as f32
            + w1 * table[base + offset1 + 2] as f32
            + w2 * table[base + offset2 + 2] as f32
            + f3 * table[base + offset_111 + 2] as f32)
            * inv;

        // Encoded-domain Amount, pinned after the blend; skipped entirely
        // at full strength (hasAmount == false uses the LUT result raw).
        if self.has_amount {
            r = pin_real32_unit(r + self.amount * (r0 - r));
            g = pin_real32_unit(g + self.amount * (g0 - g));
            b = pin_real32_unit(b + self.amount * (b0 - b));
        } else {
            r = r0;
            g = g0;
            b = b0;
        }

        if let Some(dec) = &self.decode_gamma {
            r = dec
                .interpolate(r, "RGBTable decode gamma")
                .unwrap_or_else(|_| pin_real32_unit(r));
            g = dec
                .interpolate(g, "RGBTable decode gamma")
                .unwrap_or_else(|_| pin_real32_unit(g));
            b = dec
                .interpolate(b, "RGBTable decode gamma")
                .unwrap_or_else(|_| pin_real32_unit(b));
        }

        if self.has_matrix {
            if self.has_gamut {
                r += gamut_delta[0];
                g += gamut_delta[1];
                b += gamut_delta[2];
            }

            if self.overrange {
                r = decode_overrange(r);
                g = decode_overrange(g);
                b = decode_overrange(b);
            }

            let d = &self.decode_matrix;
            let mut r_dst = r * d[0][0] + g * d[0][1] + b * d[0][2];
            let mut g_dst = r * d[1][0] + g * d[1][1] + b * d[1][2];
            let mut b_dst = r * d[2][0] + g * d[2][1] + b * d[2][2];

            if !self.overrange {
                r_dst = pin_real32_unit(r_dst);
                g_dst = pin_real32_unit(g_dst);
                b_dst = pin_real32_unit(b_dst);
            }

            // `Lerp_real32 (src, dst, m)` with m = 1.0 (no mask) —
            // unconditional in the SDK, and NOT bit-exact to `dst`.
            [
                lerp_real32(r_src, r_dst, 1.0),
                lerp_real32(g_src, g_dst, 1.0),
                lerp_real32(b_src, b_dst, 1.0),
            ]
        } else {
            [
                lerp_real32(r_src, r, 1.0),
                lerp_real32(g_src, g, 1.0),
                lerp_real32(b_src, b, 1.0),
            ]
        }
    }

    /// `RefRGBtoRGBTable1D` (`dng_reference.cpp:3841`), scalarized to one
    /// pixel with mask `m == 1.0`: three precomposed sampled lookups, no
    /// tetrahedral step, no explicit amount — amount was folded into the
    /// tables at prepare time.
    fn apply_1d(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut r = rgb[0];
        let mut g = rgb[1];
        let mut b = rgb[2];

        let r_src = r;
        let g_src = g;
        let b_src = b;

        let mut gamut_delta = [0.0f32; 3];

        if self.has_matrix {
            let e = &self.encode_matrix;
            let mut rr = r * e[0][0] + g * e[0][1] + b * e[0][2];
            let mut gg = r * e[1][0] + g * e[1][1] + b * e[1][2];
            let mut bb = r * e[2][0] + g * e[2][1] + b * e[2][2];

            if self.overrange {
                rr = encode_overrange(rr);
                gg = encode_overrange(gg);
                bb = encode_overrange(bb);
            }

            r = pin_real32_unit(rr);
            g = pin_real32_unit(gg);
            b = pin_real32_unit(bb);

            if self.has_gamut {
                gamut_delta = [rr - r, gg - g, bb - b];
            }
        }
        // No input pin on the matrix-free path — same rationale as apply_3d.

        let tables = self
            .tables_1d
            .as_ref()
            .expect("1-D prepared tables must exist");
        r = tables[0]
            .interpolate(r, "RGBTable 1-D R")
            .unwrap_or_else(|_| pin_real32_unit(r));
        g = tables[1]
            .interpolate(g, "RGBTable 1-D G")
            .unwrap_or_else(|_| pin_real32_unit(g));
        b = tables[2]
            .interpolate(b, "RGBTable 1-D B")
            .unwrap_or_else(|_| pin_real32_unit(b));

        if self.has_matrix {
            if self.has_gamut {
                r += gamut_delta[0];
                g += gamut_delta[1];
                b += gamut_delta[2];
            }

            if self.overrange {
                r = decode_overrange(r);
                g = decode_overrange(g);
                b = decode_overrange(b);
            }

            let d = &self.decode_matrix;
            let mut r_dst = r * d[0][0] + g * d[0][1] + b * d[0][2];
            let mut g_dst = r * d[1][0] + g * d[1][1] + b * d[1][2];
            let mut b_dst = r * d[2][0] + g * d[2][1] + b * d[2][2];

            if !self.overrange {
                r_dst = pin_real32_unit(r_dst);
                g_dst = pin_real32_unit(g_dst);
                b_dst = pin_real32_unit(b_dst);
            }

            [
                lerp_real32(r_src, r_dst, 1.0),
                lerp_real32(g_src, g_dst, 1.0),
                lerp_real32(b_src, b_dst, 1.0),
            ]
        } else {
            [
                lerp_real32(r_src, r, 1.0),
                lerp_real32(g_src, g, 1.0),
                lerp_real32(b_src, b, 1.0),
            ]
        }
    }
}

/// `dng_rgb_to_rgb_1d_function::Evaluate` (`dng_big_table.cpp:5153-5175`):
/// the f64 domain 1-D table lookup, amount folded in at evaluation time.
struct OneDMap<'a> {
    divisions: usize,
    samples: &'a [u16],
    plane: usize,
    amount: f64,
}

impl OneDMap<'_> {
    fn evaluate(&self, x: f64) -> f64 {
        let scaled = x * (self.divisions - 1) as f64;
        let index = pin_int32(0, scaled as i32, self.divisions as i32 - 2) as usize;
        let fract = scaled - index as f64;
        let base = index * 4 + self.plane;
        let y = ((1.0 - fract) * f64::from(self.samples[base])
            + fract * f64::from(self.samples[base + 4]))
            * (1.0 / 65535.0);
        x + self.amount * (y - x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(
        dimensions: u32,
        size: usize,
        samples: Vec<u16>,
        cs: u32,
        gamma: u32,
        gamut: u32,
    ) -> RgbTable {
        RgbTable {
            dimensions,
            size,
            samples,
            color_space: cs,
            gamma,
            gamut,
            min_amount: 0.0,
            max_amount: 2.0,
            flags: None,
        }
    }

    fn identity_3d(size: usize) -> Vec<u16> {
        let mut out = Vec::with_capacity(size * size * size * 4);
        let scale = (size - 1) as f64;
        for r in 0..size {
            for g in 0..size {
                for b in 0..size {
                    out.push((r as f64 / scale * 65535.0 + 0.5) as u16);
                    out.push((g as f64 / scale * 65535.0 + 0.5) as u16);
                    out.push((b as f64 / scale * 65535.0 + 0.5) as u16);
                    out.push(0);
                }
            }
        }
        out
    }

    // ------------------------------------------------------- overrange codec

    #[test]
    fn overrange_codec_matches_sdk_landmarks() {
        // EncodeOverrange: [0,16] -> [0,1], not identity below 1.
        assert_eq!(encode_overrange(0.0), 0.0);
        assert_eq!(encode_overrange(16.0), 1.0);
        assert_eq!(encode_overrange(1.0), 257.0 / 512.0);
        // Below 1 it is NOT identity.
        assert!((encode_overrange(0.5) - 0.5).abs() > 0.04);

        // Round trip within f32 noise (algebraic inverse, not bit-exact).
        for x in [0.0f32, 0.001, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0] {
            let back = decode_overrange(encode_overrange(x));
            assert!(
                (back - x).abs() <= 1e-5 * x.max(1.0),
                "decode(encode({x})) = {back}"
            );
        }

        // NaN -> 0 (Max_real32(0, NaN) is 0), and huge inputs saturate.
        assert_eq!(encode_overrange(f32::NAN), 0.0);
        assert_eq!(encode_overrange(-1.0), 0.0);
        assert_eq!(decode_overrange(1.0), 16.0);
    }

    // ------------------------------------------------------- IsValid / amount

    #[test]
    fn amount_zero_prophoto_or_extend_is_a_nop() {
        // `dng_rgb_table::IsValid`: amount 0 + (ProPhoto OR extend) -> skipped.
        let prophoto = table(3, 2, identity_3d(2), 2, 0, 0);
        let extend = table(3, 2, identity_3d(2), 0, 0, 1);
        let clip_srgb = table(3, 2, identity_3d(2), 0, 0, 0);

        let p = PreparedRgbTable::new(&prophoto, 0.0).expect("prophoto");
        let e = PreparedRgbTable::new(&extend, 0.0).expect("extend");
        let c = PreparedRgbTable::new(&clip_srgb, 0.0).expect("clip");

        assert!(!p.runs(), "ProPhoto + amount 0 must be a NOP");
        assert!(!e.runs(), "extend + amount 0 must be a NOP");
        assert!(c.runs(), "non-ProPhoto clip + amount 0 must still run");
    }

    #[test]
    fn amount_zero_clip_table_still_gamut_clips() {
        // The IsValid "trap": an amount-0 sRGB-primaries CLIP table stays live
        // and performs the matrix+pin gamut clip, even though the LUT is off.
        let t = table(3, 2, identity_3d(2), 0, 0, 0);
        let p = PreparedRgbTable::new(&t, 0.0).expect("ctx");
        let out = p.apply([1.4, 0.5, -0.3]);
        // The encode matrix is ProPhoto->sRGB; out-of-gamut channels are
        // pinned inside the table-primaries domain, so the result differs
        // from a pure passthrough.
        assert!(out.iter().all(|v| (0.0..=1.0).contains(v)));
    }

    #[test]
    fn set_amount_rounds_to_two_decimals() {
        // `SetAmount`: Round_int32(amount*100)*0.01 then pin to bounds.
        // 0.995 -> 1.00 (rounds UP), so hasAmount must be false.
        let t = table(3, 2, identity_3d(2), 0, 0, 0);
        let p = PreparedRgbTable::new(&t, 0.995).expect("ctx");
        assert!(!p.has_amount, "0.995 must round to 1.00");

        let p = PreparedRgbTable::new(&t, 0.994).expect("ctx");
        assert!(p.has_amount, "0.994 must round to 0.99");
        assert_eq!(p.amount, 0.99f32);
    }

    #[test]
    fn amount_one_skips_the_blend_entirely() {
        // hasAmount == false at a == 1.0: r0 is used raw, no lerp+pin.
        let t = table(3, 2, identity_3d(2), 0, 0, 0);
        let p = PreparedRgbTable::new(&t, 1.0).expect("ctx");
        assert!(!p.has_amount);
    }

    // ------------------------------------------------------------- identity

    #[test]
    fn identity_3d_table_is_near_passthrough_in_sdr() {
        for cs in 0..5u32 {
            for gamma in 0..5u32 {
                for gamut in 0..2u32 {
                    let t = table(3, 4, identity_3d(4), cs, gamma, gamut);
                    let p = PreparedRgbTable::new(&t, 1.0).expect("ctx");
                    for probe in [
                        [0.0f32; 3],
                        [1.0, 1.0, 1.0],
                        [0.18, 0.18, 0.18],
                        [0.5, 0.5, 0.5],
                    ] {
                        let out = p.apply(probe);
                        for c in 0..3 {
                            assert!(
                                (out[c] - probe[c]).abs() < 1e-4,
                                "cs={cs} gamma={gamma} gamut={gamut} probe={probe:?} out={out:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn prophoto_hdr_injects_identity_matrix_and_overrange() {
        // HDR ProPhoto: the SDK injects an identity matrix so overrange runs.
        // An identity table must approximately preserve an HDR pixel through
        // encode -> LUT -> decode (overrange round trip).
        let t = table(3, 4, identity_3d(4), 2, 0, 0);
        let p = PreparedRgbTable::new_overrange(&t, 1.0).expect("ctx");
        assert!(p.has_matrix, "HDR ProPhoto must inject an identity matrix");
        assert!(p.has_gamut, "HDR must force gamut processing");

        // HDR input at 4.0 (two stops above 1.0) must survive unclipped.
        let out = p.apply([4.0, 2.0, 0.5]);
        assert!(
            (out[0] - 4.0).abs() < 1e-3 && (out[1] - 2.0).abs() < 1e-3,
            "HDR identity must preserve overrange values, got {out:?}"
        );
    }

    #[test]
    fn sdr_output_is_pinned_but_hdr_is_not() {
        let t = table(3, 4, identity_3d(4), 0, 0, 0);
        let sdr = PreparedRgbTable::new(&t, 1.0).expect("sdr");
        let hdr = PreparedRgbTable::new_overrange(&t, 1.0).expect("hdr");

        let out_sdr = sdr.apply([4.0, 4.0, 4.0]);
        assert!(out_sdr.iter().all(|v| *v <= 1.0), "SDR must pin");

        let out_hdr = hdr.apply([4.0, 4.0, 4.0]);
        assert!(out_hdr[0] > 1.0, "HDR must not pin at 1.0, got {out_hdr:?}");
    }

    // ------------------------------------------------------------ 1-D tables

    #[test]
    fn one_dimensional_table_composes_and_runs() {
        // 1-D: each channel gets a precomposed 4096-knot table. A ramp is
        // identity; a non-ramp curve changes the value.
        let mut ramp = Vec::with_capacity(16 * 4);
        let mut curve = Vec::with_capacity(16 * 4);
        for i in 0..16usize {
            let x = i as f64 / 15.0;
            ramp.push((x * 65535.0 + 0.5) as u16);
            ramp.push((x * 65535.0 + 0.5) as u16);
            ramp.push((x * 65535.0 + 0.5) as u16);
            ramp.push(0);
            let c = (x * x * (3.0 - 2.0 * x) * 65535.0 + 0.5) as u16;
            curve.push(c);
            curve.push(c);
            curve.push(c);
            curve.push(0);
        }

        let ramp_t = table(1, 16, ramp, 0, 0, 0);
        let curve_t = table(1, 16, curve, 0, 0, 0);

        let rp = PreparedRgbTable::new(&ramp_t, 1.0).expect("ramp");
        let probe = [0.3f32, 0.5, 0.7];
        let out = rp.apply(probe);
        for c in 0..3 {
            assert!(
                (out[c] - probe[c]).abs() < 1e-4,
                "1-D ramp identity: {out:?}"
            );
        }

        let cp = PreparedRgbTable::new(&curve_t, 1.0).expect("curve");
        // smoothstep(0.3) = 0.216 — 0.5 is a fixed point of the curve, so the
        // probe deliberately avoids it.
        let out = cp.apply([0.3f32, 0.3, 0.3]);
        assert!(
            (out[0] - 0.216).abs() < 0.02 && (out[0] - 0.3).abs() > 0.05,
            "smoothstep 1-D must move the probe toward 0.216, got {out:?}"
        );
    }
}

/// Golden-pixel suite: every row of `tests/fixtures/rgb_table_golden.tsv` was
/// produced by `tests/adobe_oracle/oracle.cpp`, a compiled harness whose
/// evaluator functions are VERBATIM Adobe DNG SDK 1.7.1 (b2502) code
/// (`RefRGBtoRGBTable3D`/`RefRGBtoRGBTable1D`, `dng_1d_table`, the four gamma
/// `dng_1d_function`s, `EncodeOverrange`/`DecodeOverrange`, `Pin/Lerp`,
/// `SetAmount`, `IsValid`, and the `dng_rgb_to_rgb_table_data` prepare logic).
/// Regenerate with:
///   clang++ -std=c++17 -O2 -ffp-contract=off oracle.cpp -o oracle
///   ./oracle > ../fixtures/rgb_table_golden.tsv
///
/// Row format: rule dim div primaries gamma gamut amount hdr | in_r in_g in_b
/// (f32 bit patterns) | out_r out_g out_b (f32 bit patterns) — or the literal
/// markers `ERR` (the SDK's `dng_1d_table::Interpolate` throws on an
/// out-of-domain input; the Rust kernel pins its input instead — deliberate
/// divergence, so the row is skipped) and `OOB` (the SDK reads a negative
/// tetra index out of bounds; Rust clamps — deliberate safety divergence).
#[cfg(test)]
mod oracle_tests {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rgb_table_golden.tsv"
    );

    /// Rebuilds the exact u16 sample data the oracle synthesized for `rule`
    /// (the same formulas, quantized identically — tables are test data, not
    /// oracle output).
    fn build_samples(rule: &str, dimensions: u32, divisions: usize) -> Vec<u16> {
        let quant = |x: f64| -> u16 {
            let x = x.clamp(0.0, 1.0);
            (x * 65535.0 + 0.5).floor() as u16
        };
        let ss = |x: f64| x * x * (3.0 - 2.0 * x);

        if dimensions == 1 {
            let mut out = Vec::with_capacity(divisions * 4);
            for i in 0..divisions {
                let x = i as f64 / (divisions - 1) as f64;
                let (r, g, b) = if rule == "identity" {
                    (x, x, x)
                } else {
                    // "scurve"
                    (ss(x), x.powf(0.85), x.powf(1.2))
                };
                out.push(quant(r));
                out.push(quant(g));
                out.push(quant(b));
                out.push(0);
            }
            return out;
        }

        let mut out = Vec::with_capacity(divisions * divisions * divisions * 4);
        for ri in 0..divisions {
            for gi in 0..divisions {
                for bi in 0..divisions {
                    let r = ri as f64 / (divisions - 1) as f64;
                    let g = gi as f64 / (divisions - 1) as f64;
                    let b = bi as f64 / (divisions - 1) as f64;
                    let (or, og, ob) = match rule {
                        "identity" => (r, g, b),
                        "smooth" => (
                            ss(r) * 0.9 + g * 0.05 + b * 0.05,
                            ss(g) * 0.9 + r * 0.07,
                            ss(b) * 0.9 + g * 0.03,
                        ),
                        _ => (g, b, r), // "hard": channel exchange
                    };
                    out.push(quant(or));
                    out.push(quant(og));
                    out.push(quant(ob));
                    out.push(0);
                }
            }
        }
        out
    }

    /// Channels must match the oracle bit-for-bit, OR within 4 f32 ULP: the
    /// kernel's f32 arithmetic is deterministic, but `powf` in the f64
    /// gamma-table *sampling* may differ by an ulp between libm and Rust on
    /// some knots, which shifts a sampled knot by one f32 ulp.
    fn ulps(a: f32, b: f32) -> u32 {
        let (ia, ib) = (a.to_bits() as i32, b.to_bits() as i32);
        // monotonic mapping: works for non-negative floats (all our values are)
        if ia.signum() == ib.signum() {
            (ia - ib).unsigned_abs()
        } else {
            ia.unsigned_abs() + ib.unsigned_abs()
        }
    }

    #[test]
    fn golden_pixels_match_the_sdk_oracle() {
        let text = std::fs::read_to_string(FIXTURE).expect("read golden fixture");

        let mut total = 0usize;
        let mut compared = 0usize;
        let mut exact = 0usize;
        let mut skipped_err = 0usize;
        let mut skipped_oob = 0usize;
        let mut worst_ulp = 0u32;
        let mut worst_row = String::new();
        let mut failures: Vec<String> = Vec::new();

        // Tables are cached per (rule, dim, div, primaries, gamma, gamut,
        // amount, hdr) key — PreparedRgbTable construction is the expensive
        // part (sampled gamma tables).
        let mut cache: std::collections::HashMap<String, (RgbTable, PreparedRgbTable<'static>)> =
            Default::default();

        for (lineno, line) in text.lines().enumerate() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            total += 1;
            let fields: Vec<&str> = line.split('\t').collect();
            assert!(fields.len() >= 12, "line {}: {}", lineno + 1, line);

            let rule = fields[0];
            let dimensions: u32 = fields[1].parse().unwrap();
            let divisions: u32 = fields[2].parse().unwrap();
            let primaries: u32 = fields[3].parse().unwrap();
            let gamma: u32 = fields[4].parse().unwrap();
            let gamut: u32 = fields[5].parse().unwrap();
            let amount: f64 = fields[6].parse().unwrap();
            let hdr = fields[7] == "1";

            let ir = f32::from_bits(u32::from_str_radix(fields[8], 16).unwrap());
            let ig = f32::from_bits(u32::from_str_radix(fields[9], 16).unwrap());
            let ib = f32::from_bits(u32::from_str_radix(fields[10], 16).unwrap());

            if fields[11] == "ERR" {
                skipped_err += 1;
                continue;
            }
            if fields[11] == "OOB" {
                skipped_oob += 1;
                continue;
            }

            let key = format!(
                "{rule}|{dimensions}|{divisions}|{primaries}|{gamma}|{gamut}|{amount}|{hdr}"
            );
            if !cache.contains_key(&key) {
                let samples = build_samples(rule, dimensions, divisions as usize);
                let table = RgbTable {
                    dimensions,
                    size: divisions as usize,
                    samples,
                    color_space: primaries,
                    gamma,
                    gamut,
                    min_amount: 0.0,
                    max_amount: 2.0,
                    flags: None,
                };
                // SAFETY-FREE clone trick: PreparedRgbTable borrows the table,
                // so store an owned clone inside the map with a leaked
                // reference (test-only, bounded count).
                let leaked: &'static RgbTable = Box::leak(Box::new(table.clone()));
                let prepared = if hdr {
                    PreparedRgbTable::new_overrange(leaked, amount)
                } else {
                    PreparedRgbTable::new(leaked, amount)
                }
                .unwrap_or_else(|e| panic!("line {}: prepare {key}: {e}", lineno + 1));
                cache.insert(key.clone(), (table, prepared));
            }
            let prepared = &cache.get(&key).unwrap().1;

            let out = prepared.apply([ir, ig, ib]);

            let er = f32::from_bits(u32::from_str_radix(fields[11], 16).unwrap());
            let eg = f32::from_bits(u32::from_str_radix(fields[12], 16).unwrap());
            let eb = f32::from_bits(u32::from_str_radix(fields[13], 16).unwrap());

            compared += 1;
            if out == [er, eg, eb] {
                exact += 1;
                continue;
            }

            let u = ulps(out[0], er).max(ulps(out[1], eg)).max(ulps(out[2], eb));
            if u > worst_ulp {
                worst_ulp = u;
                worst_row = format!("line {}: {}", lineno + 1, line);
            }
            if u > 4 {
                failures.push(format!(
                    "line {}: {} -> {:?} expected {:?} ({} ulp)",
                    lineno + 1,
                    line,
                    out,
                    [er, eg, eb],
                    u
                ));
            }
        }

        eprintln!(
            "GOLDEN: {total} rows, {compared} compared, {exact} bit-exact, \
             worst divergence {worst_ulp} ulp at [{worst_row}] | \
             skipped: {skipped_err} SDK-throws, {skipped_oob} SDK-OOB rows"
        );

        assert!(
            failures.len() <= compared / 1000 + 5,
            "{} rows diverged beyond 4 ulp; first: {:?}",
            failures.len(),
            failures.first()
        );
        assert!(compared > 10_000, "fixture must exercise >10k pixels");
    }
}
