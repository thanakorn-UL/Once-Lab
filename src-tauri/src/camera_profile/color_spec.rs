//! The camera→PCS color transform resolution, ported from the public Adobe
//! DNG SDK (`dng_color_spec.cpp`, `dng_temperature.cpp`, `dng_camera_profile.cpp`).
//!
//! Given a scene white point (derived from the RAW file's as-shot neutral via
//! [`DcpColorSpec::neutral_to_xy`]) this computes the same matrices the SDK
//! derives in `dng_color_spec::SetWhiteXY`: the dual-illuminant interpolated
//! XYZ→camera matrix, the white-balanced `CameraToPCS` transform (ForwardMatrix
//! when authored, otherwise the ColorMatrix + Bradford adapt path), and the
//! camera-white clip points used by `RefBaselineABCtoRGB`.
//!
//! All matrix work is done in f64 like the SDK; the caller narrows once.

/// A CIE xy chromaticity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Xy {
    pub x: f64,
    pub y: f64,
}

impl Xy {
    /// `D50_xy_coord()` (`dng_xy_coord.h`): the DNG PCS white.
    pub(crate) const D50: Xy = Xy {
        x: 0.3457,
        y: 0.3585,
    };

    /// `XYtoXYZ` (`dng_xy_coord.cpp:44-71`): pins x and y into
    /// `[1e-6, 0.999999]` and rescales when `x + y > 0.999999` "to prevent
    /// math from doing strange things when users specify extreme
    /// temperature/tint coordinates", then lifts to XYZ with Y = 1.
    fn to_xyz(self) -> [f64; 3] {
        let mut x = self.x.clamp(0.000001, 0.999999);
        let mut y = self.y.clamp(0.000001, 0.999999);
        if x + y > 0.999999 {
            let scale = 0.999999 / (x + y);
            x *= scale;
            y *= scale;
        }
        [x / y, 1.0, (1.0 - x - y) / y]
    }

    /// `XYZtoXY`: `x = X/(X+Y+Z)`, `y = Y/(X+Y+Z)`; on a non-positive sum the
    /// result falls back to the D50 PCS white rather than producing NaN.
    fn from_xyz(xyz: [f64; 3]) -> Self {
        let sum = xyz[0] + xyz[1] + xyz[2];
        if sum > 0.0 {
            Self {
                x: xyz[0] / sum,
                y: xyz[1] / sum,
            }
        } else {
            Xy::D50
        }
    }
}

/// `PCStoXYZ()` — the D50 PCS white as XYZ.
fn pcs_to_xyz() -> [f64; 3] {
    Xy::D50.to_xyz()
}

// ---------------------------------------------------------------- matrices

pub(crate) type Mat3 = [[f64; 3]; 3];

pub(crate) fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    out
}

pub(crate) fn mat_vec(m: &Mat3, v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

pub(crate) fn mat_scale(m: &Mat3, s: f64) -> Mat3 {
    let mut out = *m;
    for row in &mut out {
        for v in row {
            *v *= s;
        }
    }
    out
}

pub(crate) fn mat_add_scaled(a: &Mat3, wa: f64, b: &Mat3, wb: f64) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = wa * a[i][j] + wb * b[i][j];
        }
    }
    out
}

pub(crate) fn mat_invert(m: &Mat3) -> Option<Mat3> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if !det.is_finite() || det.abs() < 1e-30 {
        return None;
    }
    let inv = 1.0 / det;
    let mut out = [[0.0; 3]; 3];
    out[0][0] = (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv;
    out[0][1] = (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv;
    out[0][2] = (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv;
    out[1][0] = (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv;
    out[1][1] = (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv;
    out[1][2] = (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv;
    out[2][0] = (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv;
    out[2][1] = (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv;
    out[2][2] = (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv;
    Some(out)
}

// ------------------------------------------------------- chromatic adaption

/// `MapWhiteMatrix` (`dng_color_spec.cpp`): the linearized Bradford adaptation
/// from `white1` to `white2`, with Adobe's negative-coordinate floor and the
/// [0.1, 10] scaling pin.
pub(crate) fn map_white_matrix(white1: Xy, white2: Xy) -> Mat3 {
    const MB: Mat3 = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];

    let w1 = mat_vec(&MB, white1.to_xyz());
    let w2 = mat_vec(&MB, white2.to_xyz());

    let w1 = w1.map(|v| v.max(0.0));
    let w2 = w2.map(|v| v.max(0.0));

    let mut a = [[0.0; 3]; 3];
    for i in 0..3 {
        let ratio = if w1[i] > 0.0 { w2[i] / w1[i] } else { 10.0 };
        a[i][i] = ratio.clamp(0.1, 10.0);
    }

    mat_mul(
        &mat_invert(&MB).expect("Bradford matrix is invertible"),
        &mat_mul(&a, &MB),
    )
}

// --------------------------------------------------------- temperature (CCT)

/// Wyszecki & Stiles table from `dng_temperature.cpp`: inverse temperature in
/// megaKelvin-ish reciprocal units (`r` = 1e6/T), `u`,`v` chromaticities and
/// the isotemperature-line slope `t`.
#[rustfmt::skip]
const TEMP_TABLE: &[(f64, f64, f64, f64)] = &[
    (0.0,   0.18006, 0.26352, -0.24341),
    (10.0,  0.18066, 0.26589, -0.25479),
    (20.0,  0.18133, 0.26846, -0.26876),
    (30.0,  0.18208, 0.27119, -0.28539),
    (40.0,  0.18293, 0.27407, -0.30470),
    (50.0,  0.18388, 0.27709, -0.32675),
    (60.0,  0.18494, 0.28021, -0.35156),
    (70.0,  0.18611, 0.28342, -0.37915),
    (80.0,  0.18740, 0.28668, -0.40955),
    (90.0,  0.18880, 0.28997, -0.44278),
    (100.0, 0.19032, 0.29326, -0.47888),
    (125.0, 0.19462, 0.30141, -0.58204),
    (150.0, 0.19962, 0.30921, -0.70471),
    (175.0, 0.20525, 0.31647, -0.84901),
    (200.0, 0.21142, 0.32312, -1.0182),
    (225.0, 0.21807, 0.32909, -1.2168),
    (250.0, 0.22511, 0.33439, -1.4512),
    (275.0, 0.23247, 0.33904, -1.7298),
    (300.0, 0.24010, 0.34308, -2.0637),
    (325.0, 0.24702, 0.34655, -2.4681),
    (350.0, 0.25591, 0.34951, -2.9641),
    (375.0, 0.26400, 0.35200, -3.5814),
    (400.0, 0.27218, 0.35407, -4.3633),
    (425.0, 0.28039, 0.35577, -5.3762),
    (450.0, 0.28863, 0.35714, -6.7262),
    (475.0, 0.29685, 0.35823, -8.5955),
    (500.0, 0.30505, 0.35907, -11.324),
    (525.0, 0.31320, 0.35968, -15.628),
    (550.0, 0.32129, 0.36011, -23.325),
    (575.0, 0.32931, 0.36038, -40.770),
    (600.0, 0.33724, 0.36051, -116.45),
];

/// `dng_temperature::Set_xy_coord`: correlated color temperature (Kelvin) of
/// a chromaticity, along the Robertson isotemperature lines. `None` when the
/// input is not finite.
pub(crate) fn temperature_from_xy(xy: Xy) -> Option<f64> {
    if !xy.x.is_finite() || !xy.y.is_finite() {
        return None;
    }

    let denom = 1.5 - xy.x + 6.0 * xy.y;
    if denom == 0.0 {
        return None;
    }
    let u = 2.0 * xy.x / denom;
    let v = 3.0 * xy.y / denom;

    let mut last_dt = 0.0;

    for index in 1..TEMP_TABLE.len() {
        let dv_raw = TEMP_TABLE[index].3;
        let len = (1.0 + dv_raw * dv_raw).sqrt();
        let du = 1.0 / len;
        let dv = dv_raw / len;

        let uu = u - TEMP_TABLE[index].1;
        let vv = v - TEMP_TABLE[index].2;
        let mut dt = -uu * dv + vv * du;

        if dt <= 0.0 || index == TEMP_TABLE.len() - 1 {
            if dt > 0.0 {
                dt = 0.0;
            }
            let dt = -dt;

            let f = if index == 1 { 0.0 } else { dt / (last_dt + dt) };

            return Some(1.0e6 / (TEMP_TABLE[index - 1].0 * f + TEMP_TABLE[index].0 * (1.0 - f)));
        }

        last_dt = dt;
    }
    None
}

/// `dng_camera_profile::IlluminantToTemperature`: EXIF light-source code to a
/// calibration temperature in Kelvin. `None` means "not usable for matrix
/// interpolation" (unknown / `lsOther` without explicit data).
pub(crate) fn illuminant_to_temperature(light: u16) -> Option<f64> {
    match light {
        // lsStandardLightA | lsTungsten
        17 | 3 => Some(2850.0),
        // lsISOStudioTungsten
        24 => Some(3200.0),
        // lsD50
        23 => Some(5000.0),
        // lsD55 | lsDaylight | lsFineWeather | lsFlash | lsStandardLightB
        20 | 1 | 9 | 4 | 18 => Some(5500.0),
        // lsD65 | lsStandardLightC | lsCloudyWeather
        21 | 19 | 10 => Some(6500.0),
        // lsD75 | lsShade
        22 | 11 => Some(7500.0),
        // lsDaylightFluorescent
        12 => Some((5700.0 + 7100.0) * 0.5),
        // lsDayWhiteFluorescent
        13 => Some((4600.0 + 5500.0) * 0.5),
        // lsCoolWhiteFluorescent | lsFluorescent
        14 | 2 => Some((3800.0 + 4500.0) * 0.5),
        // lsWhiteFluorescent
        15 => Some((3250.0 + 3800.0) * 0.5),
        // lsWarmWhiteFluorescent
        16 => Some((2600.0 + 3250.0) * 0.5),
        // lsOther without IlluminantData, and lsUnknown.
        _ => None,
    }
}

/// Human-readable illuminant name for the diagnostic report.
pub(crate) fn illuminant_name(light: u16) -> &'static str {
    match light {
        0 => "Unknown",
        1 => "Daylight",
        2 => "Fluorescent",
        3 => "Tungsten",
        4 => "Flash",
        9 => "FineWeather",
        10 => "CloudyWeather",
        11 => "Shade",
        12 => "DaylightFluorescent",
        13 => "DayWhiteFluorescent",
        14 => "CoolWhiteFluorescent",
        15 => "WhiteFluorescent",
        16 => "WarmWhiteFluorescent",
        17 => "StandardLightA",
        18 => "StandardLightB",
        19 => "StandardLightC",
        20 => "D55",
        21 => "D65",
        22 => "D75",
        23 => "D50",
        24 => "ISOStudioTungsten",
        255 => "Other",
        _ => "Unrecognized",
    }
}

// ------------------------------------------------------------- the spec

/// Per-illuminant input bundle: the authored XYZ→camera color matrix plus the
/// optional ForwardMatrix (camera→PCS/D50) sharing the same index.
#[derive(Debug, Clone)]
pub(crate) struct IlluminantMatrices {
    pub illuminant: u16,
    /// `ColorMatrixN` — 3x3 XYZ→camera.
    pub color_matrix: Mat3,
    /// `ForwardMatrixN` — 3x3 camera→XYZ(D50), normalized to map camera
    /// `[1,1,1]` to PCS white (the SDK applies `NormalizeForwardMatrix` at
    /// load; done in [`DcpColorSpec::new`]).
    pub forward_matrix: Option<Mat3>,
}

/// What matrix path the resolved transform ended up using (for reporting).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MatrixPath {
    /// A ForwardMatrix (optionally interpolated between illuminants).
    ForwardMatrix,
    /// The inverse of the interpolated ColorMatrix with Bradford adaptation
    /// from the scene white to D50 (SDK `adapt in XYZ` path).
    ColorMatrixInverseAdapted,
}

/// The white-balanced camera→PCS resolution for one scene white point.
pub(crate) struct ResolvedColorSpec {
    /// `fCameraToPCS`: white-balanced camera → XYZ(D50).
    pub camera_to_pcs: Mat3,
    /// `fCameraWhite`: per-channel camera-white clip points (`Pin(0.001,1)`),
    /// already normalized so the largest entry is 1.0.
    pub camera_white: [f64; 3],
    /// The interpolation weight `g` of the first calibration (1.0 → only
    /// matrix 1, 0.0 → only matrix 2).
    pub interpolation_weight: f64,
    /// The scene white's CCT estimate.
    pub white_temperature: Option<f64>,
    pub path: MatrixPath,
    /// The illuminant list the resolution chose between (post-reorder).
    pub illuminants: Vec<u16>,
}

/// The per-DCP equivalent of `dng_color_spec`: the authored matrices plus the
/// illuminant-temperature model (single or dual illuminant; triple-illuminant
/// profiles are rejected upstream in `dcp.rs`).
pub(crate) struct DcpColorSpec {
    temperature1: f64,
    temperature2: f64,
    color_matrix1: Mat3,
    color_matrix2: Mat3,
    forward_matrix1: Option<Mat3>,
    forward_matrix2: Option<Mat3>,
    illuminants: Vec<u16>,
}

impl DcpColorSpec {
    /// Mirrors the `dng_color_spec` constructor's single/dual selection,
    /// matrix ordering by temperature, and `NormalizeForwardMatrix`.
    ///
    /// `matrices` must carry 1 or 2 entries (triple-illuminant is an
    /// upstream `Unsupported` error).
    pub(crate) fn new(matrices: Vec<IlluminantMatrices>) -> Result<Self, String> {
        if matrices.is_empty() || matrices.len() > 2 {
            return Err("DcpColorSpec requires 1 or 2 illuminant matrix sets".to_string());
        }

        let first = &matrices[0];
        let mut temperature1 = illuminant_to_temperature(first.illuminant).unwrap_or(0.0);
        let mut temperature2 = matrices
            .get(1)
            .and_then(|m| illuminant_to_temperature(m.illuminant))
            .unwrap_or(0.0);

        let mut color_matrix1 = first.color_matrix;
        let mut color_matrix2 = matrices.get(1).map(|m| m.color_matrix);
        let mut forward_matrix1 = first.forward_matrix.map(normalize_forward_matrix);
        let mut forward_matrix2 = matrices
            .get(1)
            .map(|m| m.forward_matrix.map(normalize_forward_matrix))
            .unwrap_or(None);
        let mut illuminants: Vec<u16> = matrices.iter().map(|m| m.illuminant).collect();

        if color_matrix2.is_none()
            || temperature1 <= 0.0
            || temperature2 <= 0.0
            || temperature1 == temperature2
        {
            // Single-illuminant profile: the SDK copies the first set across.
            temperature1 = 5000.0;
            temperature2 = 5000.0;
            color_matrix2 = Some(color_matrix1);
            forward_matrix2 = forward_matrix1;
            illuminants = vec![matrices[0].illuminant];
        } else if temperature1 > temperature2 {
            std::mem::swap(&mut temperature1, &mut temperature2);
            if let Some(second) = color_matrix2.as_mut() {
                std::mem::swap(&mut color_matrix1, second);
            }
            std::mem::swap(&mut forward_matrix1, &mut forward_matrix2);
            illuminants.swap(0, 1);
        }

        Ok(Self {
            temperature1,
            temperature2,
            color_matrix1,
            color_matrix2: color_matrix2.expect("dual fallback guarantees matrix2"),
            forward_matrix1,
            forward_matrix2,
            illuminants,
        })
    }

    /// `dng_color_spec::FindXYZtoCamera` (single/dual): interpolate the
    /// color matrix at `white` by inverse-temperature weighting.
    fn find_xyz_to_camera(&self, white: Xy) -> (Mat3, Option<Mat3>, f64) {
        let temperature = temperature_from_xy(white).unwrap_or(self.temperature1);

        let g = if temperature <= self.temperature1 {
            1.0
        } else if temperature >= self.temperature2 {
            0.0
        } else {
            let inv_t = 1.0 / temperature;
            (inv_t - (1.0 / self.temperature2))
                / ((1.0 / self.temperature1) - (1.0 / self.temperature2))
        };

        let color_matrix = if g >= 1.0 {
            self.color_matrix1
        } else if g <= 0.0 {
            self.color_matrix2
        } else {
            mat_add_scaled(&self.color_matrix1, g, &self.color_matrix2, 1.0 - g)
        };

        let forward_matrix = match (self.forward_matrix1, self.forward_matrix2) {
            (Some(f1), Some(f2)) => Some(if g >= 1.0 {
                f1
            } else if g <= 0.0 {
                f2
            } else {
                mat_add_scaled(&f1, g, &f2, 1.0 - g)
            }),
            (Some(f1), None) => Some(f1),
            (None, Some(f2)) => Some(f2),
            (None, None) => None,
        };

        (color_matrix, forward_matrix, g)
    }

    /// `dng_color_spec::NeutralToXY`: iterative estimate of the scene
    /// illuminant's xy chromaticity from camera-space neutral values
    /// (`AsShotNeutral`, i.e. reciprocal white-balance multipliers).
    pub(crate) fn neutral_to_xy(&self, neutral: [f64; 3]) -> Option<Xy> {
        const MAX_PASSES: usize = 30;

        let mut last = Xy::D50;
        for pass in 0..MAX_PASSES {
            let (xyz_to_camera, _, _) = self.find_xyz_to_camera(last);
            let camera_to_xyz = mat_invert(&xyz_to_camera)?;
            let mut next = Xy::from_xyz(mat_vec(&camera_to_xyz, neutral));

            if (next.x - last.x).abs() + (next.y - last.y).abs() < 0.0000001 {
                return Some(next);
            }
            if pass == MAX_PASSES - 1 {
                next = Xy {
                    x: (last.x + next.x) * 0.5,
                    y: (last.y + next.y) * 0.5,
                };
            }
            last = next;
        }
        Some(last)
    }

    /// `dng_color_spec::SetWhiteXY`: resolve the white-balanced transform for
    /// a scene white chromaticity.
    pub(crate) fn set_white_xy(&self, white: Xy) -> Result<ResolvedColorSpec, String> {
        let (color_matrix, forward_matrix, g) = self.find_xyz_to_camera(white);

        // fCameraWhite: camera-space encoding of the scene white, normalized
        // by its max entry and pinned to (0.001, 1.0) exactly like the SDK.
        let mut camera_white = mat_vec(&color_matrix, white.to_xyz());
        let max_entry = camera_white.iter().copied().fold(f64::MIN, f64::max);
        if max_entry == 0.0 {
            return Err("camera white has zero max entry".to_string());
        }
        let white_scale = 1.0 / max_entry;
        for component in &mut camera_white {
            *component = (white_scale * *component).clamp(0.001, 1.0);
        }

        // fPCStoCamera: scale so PCS white maps just inside the first
        // saturating camera channel — `MaxEntry (fPCStoCamera * PCStoXYZ ())`
        // is the max entry of a matrix*vector product.
        let mut pcs_to_camera = mat_mul(&color_matrix, &map_white_matrix(Xy::D50, white));
        let pcs_scaled = mat_vec(&pcs_to_camera, pcs_to_xyz());
        let scale = pcs_scaled.iter().copied().fold(f64::MIN, f64::max);
        if scale == 0.0 {
            return Err("PCS-to-camera scale is zero".to_string());
        }
        pcs_to_camera = mat_scale(&pcs_to_camera, 1.0 / scale);

        let (camera_to_pcs, path) = if let Some(fm) = forward_matrix {
            // fCameraToPCS = FM * inv(diag(cameraWhite)); cameraCalibration and
            // analogBalance are the identity for a 3-channel DCP render.
            let diag_inv = {
                let mut d = [[0.0; 3]; 3];
                for (i, d_i) in d.iter_mut().enumerate() {
                    d_i[i] = 1.0 / camera_white[i];
                }
                d
            };
            (mat_mul(&fm, &diag_inv), MatrixPath::ForwardMatrix)
        } else {
            let inv = mat_invert(&pcs_to_camera)
                .ok_or_else(|| "PCS-to-camera matrix is singular".to_string())?;
            (inv, MatrixPath::ColorMatrixInverseAdapted)
        };

        Ok(ResolvedColorSpec {
            camera_to_pcs,
            camera_white,
            interpolation_weight: g,
            white_temperature: temperature_from_xy(white),
            path,
            illuminants: self.illuminants.clone(),
        })
    }
}

/// `dng_camera_profile::NormalizeForwardMatrix`: rescale so the matrix maps
/// camera `[1,1,1]` to PCS white.
/// `m = diag(PCStoXYZ) * inv(diag(m * ones)) * m`.
fn normalize_forward_matrix(m: Mat3) -> Mat3 {
    let xyz = mat_vec(&m, [1.0, 1.0, 1.0]);
    let mut m = m;
    let pcs = pcs_to_xyz();
    for i in 0..3 {
        if xyz[i] != 0.0 {
            let factor = pcs[i] / xyz[i];
            for v in &mut m[i] {
                *v *= factor;
            }
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    fn spec(ill1: u16, ill2: u16, cm1: Mat3, cm2: Mat3) -> DcpColorSpec {
        DcpColorSpec::new(vec![
            IlluminantMatrices {
                illuminant: ill1,
                color_matrix: cm1,
                forward_matrix: None,
            },
            IlluminantMatrices {
                illuminant: ill2,
                color_matrix: cm2,
                forward_matrix: None,
            },
        ])
        .unwrap()
    }

    /// A plausible Canon-like XYZ→camera matrix pair (values shaped like
    /// Adobe's published D65/A matrices, simplified).
    fn cm_a() -> Mat3 {
        [
            [1.8465, -0.9822, 0.0849],
            [-0.4101, 1.5430, -0.0444],
            [0.0409, -0.3684, 1.3202],
        ]
    }

    fn cm_d() -> Mat3 {
        [
            [0.9763, -0.2919, 0.1033],
            [-0.4195, 1.6489, -0.0789],
            [0.0751, -0.4380, 1.0845],
        ]
    }

    #[test]
    fn illuminant_temperatures_match_sdk_table() {
        assert_eq!(illuminant_to_temperature(17), Some(2850.0));
        assert_eq!(illuminant_to_temperature(21), Some(6500.0));
        assert_eq!(illuminant_to_temperature(23), Some(5000.0));
        assert_eq!(illuminant_to_temperature(0), None);
        assert_eq!(illuminant_to_temperature(255), None);
    }

    #[test]
    fn temperature_roundtrip_near_d65() {
        // D65 xy ≈ (0.31271, 0.32902) → ≈6504K.
        let t = temperature_from_xy(Xy {
            x: 0.31271,
            y: 0.32902,
        })
        .unwrap();
        assert!(approx(t, 6504.0, 30.0), "D65 CCT {t} far from 6504");
    }

    #[test]
    fn temperature_of_d50_is_near_5000() {
        let t = temperature_from_xy(Xy::D50).unwrap();
        assert!(approx(t, 5003.0, 40.0), "D50 CCT {t} far from ~5000");
    }

    #[test]
    fn neutral_to_xy_recovers_d65_white() {
        // Build the spec from our two matrices, compute the camera-space
        // neutral for a known scene white, then ask NeutralToXY to recover it.
        let spec = spec(21, 17, cm_d(), cm_a()); // D65 + StdA
        let white = Xy {
            x: 0.31271,
            y: 0.32902,
        };
        let (xyz_to_cam, _, _) = spec.find_xyz_to_camera(white);
        let neutral = mat_vec(&xyz_to_cam, white.to_xyz());
        let recovered = spec.neutral_to_xy(neutral).unwrap();
        assert!(approx(recovered.x, white.x, 1e-3));
        assert!(approx(recovered.y, white.y, 1e-3));
    }

    #[test]
    fn single_illuminant_spec_always_uses_matrix1() {
        let spec = DcpColorSpec::new(vec![IlluminantMatrices {
            illuminant: 21,
            color_matrix: cm_d(),
            forward_matrix: None,
        }])
        .unwrap();
        // The SDK duplicates matrix 1 across both slots for single-illuminant
        // profiles, so ANY interpolation weight still yields matrix 1.
        for xy in [Xy { x: 0.3, y: 0.3 }, Xy::D50, Xy { x: 0.45, y: 0.41 }] {
            let (cm, _, _) = spec.find_xyz_to_camera(xy);
            assert_eq!(cm, cm_d(), "white {xy:?} must select matrix 1");
        }
    }

    #[test]
    fn dual_illuminant_weighting_prefers_nearest_temperature() {
        // StdA=2850 as "1", D65=6500 as "2"; after ordering t1<t2.
        let spec = spec(21, 17, cm_d(), cm_a());
        // A scene white near StdA should weight ~CM_A.
        let (_, _, g_low) = spec.find_xyz_to_camera(Xy {
            x: 0.44757,
            y: 0.40745, // approx illuminant A
        });
        // With swapped order, matrix1 becomes StdA's; weight near 1.
        assert!(
            g_low > 0.95,
            "expected near-1 weight at tungsten white, got {g_low}"
        );

        let (_, _, g_high) = spec.find_xyz_to_camera(Xy {
            x: 0.31271,
            y: 0.32902, // D65
        });
        assert!(
            g_high < 0.05,
            "expected near-0 weight at D65 white, got {g_high}"
        );
    }

    #[test]
    fn set_white_xy_camera_white_maps_white_to_pcs_white() {
        let spec = spec(21, 17, cm_d(), cm_a());
        let white = Xy {
            x: 0.31271,
            y: 0.32902,
        };
        let resolved = spec.set_white_xy(white).unwrap();

        // camera_white encodes the scene white in camera space; scaling it
        // through camera_to_pcs must land on the D50 PCS white.
        let pcs = mat_vec(&resolved.camera_to_pcs, resolved.camera_white);
        let expected = Xy::D50.to_xyz();
        for i in 0..3 {
            assert!(
                approx(pcs[i], expected[i], 0.02),
                "camera white -> PCS[{i}] = {} expected {}",
                pcs[i],
                expected[i]
            );
        }
    }

    #[test]
    fn forward_matrix_path_reports_choice() {
        let mut with_fm = vec![IlluminantMatrices {
            illuminant: 21,
            color_matrix: cm_d(),
            forward_matrix: Some([[0.5, 0.3, 0.2], [0.2, 0.6, 0.1], [0.0, 0.1, 0.9]]),
        }];
        let spec = DcpColorSpec::new(std::mem::take(&mut with_fm)).unwrap();
        let resolved = spec.set_white_xy(Xy::D50).unwrap();
        assert_eq!(resolved.path, MatrixPath::ForwardMatrix);
    }

    #[test]
    fn bradford_adaptation_moves_white_exactly() {
        let m = map_white_matrix(
            Xy::D50,
            Xy {
                x: 0.31271,
                y: 0.32902,
            },
        );
        // Adapting the D50 PCS white should land near the target white XYZ.
        let adapted = mat_vec(&m, Xy::D50.to_xyz());
        let target = Xy {
            x: 0.31271,
            y: 0.32902,
        }
        .to_xyz();
        for i in 0..3 {
            assert!(approx(adapted[i], target[i], 0.03));
        }
    }
}
