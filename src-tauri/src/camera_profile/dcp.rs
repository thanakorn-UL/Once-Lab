//! Adobe DCP (DNG Camera Profile) tag-level parsing.
//!
//! A `.dcp` is a classic TIFF container whose IFD-0 carries the profile tags
//! defined by the DNG spec (and implemented by the public Adobe DNG SDK's
//! `dng_camera_profile`/`dng_info` parsing). This module reads the tags Once-Lab
//! can *faithfully* render, records anything it cannot, and never substitutes
//! invented data for authored data.
//!
//! Rendering semantics live in [`super::color_spec`] and [`super::render`];
//! this file only produces a validated [`DcpProfile`] description.

use super::color_spec::{IlluminantMatrices, illuminant_name};
use super::tiff::Tiff;
use crate::xmp_profile::{ENCODING_LINEAR, ENCODING_SRGB, LookTable, MIN_SAT_DIVISIONS};

/// Hard bound on `hue * sat * val` table samples, far above any table Adobe
/// ships (real LookTables top out near 90x30x16 ≈ 43k samples) yet small
/// enough to bound memory against a malformed dims product. Distinct from the
/// XMP `MAX_TOTAL_SAMPLES` guard, which fits the smaller creative-profile
/// tables.
pub(crate) const MAX_DCP_TABLE_SAMPLES: u64 = 1_000_000;

// Tag ids (`DngTag`, values confirmed against rawler's `tags.rs`).
const TAG_UNIQUE_CAMERA_MODEL: u16 = 50708;
const TAG_COLOR_MATRIX1: u16 = 50721;
const TAG_COLOR_MATRIX2: u16 = 50722;
const TAG_CAMERA_CALIBRATION1: u16 = 50723;
const TAG_CAMERA_CALIBRATION2: u16 = 50724;
const TAG_REDUCTION_MATRIX1: u16 = 50725;
const TAG_REDUCTION_MATRIX2: u16 = 50726;
const TAG_ANALOG_BALANCE: u16 = 50727;
const TAG_CALIBRATION_ILLUMINANT1: u16 = 50778;
const TAG_CALIBRATION_ILLUMINANT2: u16 = 50779;
const TAG_CAMERA_CALIBRATION_SIGNATURE: u16 = 50931;
const TAG_PROFILE_CALIBRATION_SIGNATURE: u16 = 50932;
const TAG_PROFILE_NAME: u16 = 50936;
const TAG_HUE_SAT_MAP_DIMS: u16 = 50937;
const TAG_HUE_SAT_MAP_DATA1: u16 = 50938;
const TAG_HUE_SAT_MAP_DATA2: u16 = 50939;
const TAG_PROFILE_TONE_CURVE: u16 = 50940;
const TAG_PROFILE_EMBED_POLICY: u16 = 50941;
const TAG_PROFILE_COPYRIGHT: u16 = 50942;
const TAG_FORWARD_MATRIX1: u16 = 50964;
const TAG_FORWARD_MATRIX2: u16 = 50965;
const TAG_LOOK_TABLE_DIMS: u16 = 50981;
const TAG_LOOK_TABLE_DATA: u16 = 50982;
const TAG_HUE_SAT_MAP_ENCODING: u16 = 51107;
const TAG_LOOK_TABLE_ENCODING: u16 = 51108;
const TAG_BASELINE_EXPOSURE_OFFSET: u16 = 51109;
const TAG_DEFAULT_BLACK_RENDER: u16 = 51110;
const TAG_CALIBRATION_ILLUMINANT3: u16 = 52529;
const TAG_COLOR_MATRIX3: u16 = 52531;
const TAG_FORWARD_MATRIX3: u16 = 52532;
const TAG_HUE_SAT_MAP_DATA3: u16 = 52537;

/// `defaultBlackRender` enumerant: the profile requests no black mapping.
pub(crate) const DEFAULT_BLACK_RENDER_NONE: u32 = 1;

/// How well a loaded DCP matches the camera that produced a RAW file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CameraMatch {
    /// `UniqueCameraModel` exactly equals the RAW's `"<make> <model>"`.
    Exact,
    /// Matches after case/whitespace normalization, or the model alone
    /// matches when the DCP model string already carries the maker.
    Probable,
    /// The strings plainly disagree.
    Mismatch,
    /// The DCP carries no `UniqueCameraModel` (or the RAW has no model),
    /// so no judgment is possible.
    Unknown,
}

/// A validated Adobe DCP camera profile.
#[derive(Debug)]
pub(crate) struct DcpProfile {
    pub profile_name: Option<String>,
    pub unique_camera_model: Option<String>,
    pub copyright: Option<String>,
    /// `ProfileEmbedPolicy` (informational only; we never embed).
    pub embed_policy: Option<u32>,
    /// `DefaultBlackRender`; `Some(1)` (`defaultBlackRender_None`) turns off
    /// the SDK's default shadow rolloff.
    pub default_black_render: Option<u32>,
    /// `BaselineExposureOffset` (EV) when authored.
    pub baseline_exposure_offset: Option<f64>,
    /// 1 or 2 illuminant matrix sets (3 is an unsupported hard error).
    pub matrices: Vec<IlluminantMatrices>,
    /// HueSatMap tables parallel to `matrices` (encoding stored on each
    /// table's `encoding` field); `None` per missing table.
    pub hue_sat_maps: Vec<Option<LookTable>>,
    /// `ProfileLookTable` when authored.
    pub look_table: Option<LookTable>,
    /// `ProfileToneCurve` knots `(x, y)` when authored.
    pub tone_curve: Option<Vec<(f64, f64)>>,
    /// Non-fatal observations worth surfacing to the user.
    pub warnings: Vec<String>,
    /// Features present in the file that Once-Lab does not implement; these
    /// are reported, never silently approximated.
    pub unsupported: Vec<String>,
}

impl DcpProfile {
    /// Parse and validate a DCP container.
    pub(crate) fn parse(data: &[u8]) -> Result<Self, String> {
        let tiff = Tiff::parse(data)?;
        let ifd = tiff
            .ifds
            .first()
            .ok_or_else(|| "DCP has no IFD 0".to_string())?;

        let mut warnings = Vec::new();
        let mut unsupported = Vec::new();

        let profile_name = ifd.ascii(TAG_PROFILE_NAME)?;
        let unique_camera_model = ifd.ascii(TAG_UNIQUE_CAMERA_MODEL)?;
        let copyright = ifd.ascii(TAG_PROFILE_COPYRIGHT)?;
        let embed_policy = ifd.u32_scalar(TAG_PROFILE_EMBED_POLICY)?;
        let default_black_render = ifd.u32_scalar(TAG_DEFAULT_BLACK_RENDER)?;
        let baseline_exposure_offset = ifd
            .f64_list(TAG_BASELINE_EXPOSURE_OFFSET)?
            .and_then(|v| v.first().copied());

        // ---------------------------------------------------- color matrices
        let matrix1 = required_matrix(ifd, TAG_COLOR_MATRIX1, "ColorMatrix1")?;
        let matrix2 = optional_matrix(ifd, TAG_COLOR_MATRIX2, "ColorMatrix2")?;
        let matrix3 = optional_matrix(ifd, TAG_COLOR_MATRIX3, "ColorMatrix3")?;

        let illuminant1 = ifd.u32_scalar(TAG_CALIBRATION_ILLUMINANT1)?;
        let illuminant2 = ifd.u32_scalar(TAG_CALIBRATION_ILLUMINANT2)?;
        let illuminant3 = ifd.u32_scalar(TAG_CALIBRATION_ILLUMINANT3)?;

        if matrix3.is_some() || illuminant3.is_some() {
            // The SDK supports triple-illuminant via barycentric weights; that
            // is a different interpolation model than the dual-illuminant
            // inverse-temperature rule and is reported, not approximated.
            return Err("unsupported DCP feature: triple-illuminant camera profile".to_string());
        }

        let forward1 = optional_matrix(ifd, TAG_FORWARD_MATRIX1, "ForwardMatrix1")?;
        let forward2 = optional_matrix(ifd, TAG_FORWARD_MATRIX2, "ForwardMatrix2")?;
        if optional_matrix(ifd, TAG_FORWARD_MATRIX3, "ForwardMatrix3")?.is_some() {
            return Err("unsupported DCP feature: ForwardMatrix3".to_string());
        }

        if ifd.f64_list(TAG_REDUCTION_MATRIX1)?.is_some()
            || ifd.f64_list(TAG_REDUCTION_MATRIX2)?.is_some()
        {
            unsupported.push(
                "ReductionMatrix1/2 present but unused (only needed for >3-channel cameras)"
                    .to_string(),
            );
        }
        if ifd.f64_list(TAG_CAMERA_CALIBRATION1)?.is_some()
            || ifd.f64_list(TAG_CAMERA_CALIBRATION2)?.is_some()
            || ifd.f64_list(TAG_ANALOG_BALANCE)?.is_some()
        {
            // The SDK folds `AnalogBalance * CameraCalibration * ColorMatrix`
            // for per-camera-unit fine-tuning. Zero of the 4,418 DCPs
            // shipping with Lightroom Classic carry these tags; rather than
            // silently ignore a real per-unit calibration, report it.
            warnings.push(
                "CameraCalibration/AnalogBalance tags present but not applied \
                 (absent from all shipped Adobe DCPs)"
                    .to_string(),
            );
        }
        if ifd.contains(TAG_CAMERA_CALIBRATION_SIGNATURE)
            || ifd.contains(TAG_PROFILE_CALIBRATION_SIGNATURE)
        {
            // The SDK only applies CameraCalibration when the signature on the
            // *negative* matches; we do not read the DNG's signature, so any
            // embedded expectation is reported as unapplied.
            warnings.push(
                "camera calibration signature present; negative-side CameraCalibration not applied"
                    .to_string(),
            );
        }

        let mut matrices = vec![IlluminantMatrices {
            illuminant: illuminant1.unwrap_or(0) as u16,
            color_matrix: matrix1,
            forward_matrix: forward1,
        }];
        if let Some(cm2) = matrix2 {
            matrices.push(IlluminantMatrices {
                illuminant: illuminant2.unwrap_or(0) as u16,
                color_matrix: cm2,
                forward_matrix: forward2,
            });
        } else if illuminant2.is_some() {
            warnings.push(
                "CalibrationIlluminant2 present without ColorMatrix2; single-illuminant model used"
                    .to_string(),
            );
        }

        // --------------------------------------------------------- hue/sat
        let hsm_dims = ifd.u32_list(TAG_HUE_SAT_MAP_DIMS)?;
        let hsm_encoding = ifd
            .u32_scalar(TAG_HUE_SAT_MAP_ENCODING)?
            .unwrap_or(ENCODING_LINEAR);
        validate_encoding(hsm_encoding, "ProfileHueSatMapEncoding")?;

        let hsm_data1 = parse_hue_sat_map(
            ifd,
            TAG_HUE_SAT_MAP_DATA1,
            hsm_dims.as_deref(),
            hsm_encoding,
            "ProfileHueSatMapData1",
        )?;
        let hsm_data2 = parse_hue_sat_map(
            ifd,
            TAG_HUE_SAT_MAP_DATA2,
            hsm_dims.as_deref(),
            hsm_encoding,
            "ProfileHueSatMapData2",
        )?;
        if ifd.f64_list(TAG_HUE_SAT_MAP_DATA3)?.is_some() {
            warnings
                .push("ProfileHueSatMapData3 ignored (no triple-illuminant support)".to_string());
        }

        // ------------------------------------------------------------ look
        let look_table = if let Some(dims) = ifd.u32_list(TAG_LOOK_TABLE_DIMS)? {
            let encoding = ifd
                .u32_scalar(TAG_LOOK_TABLE_ENCODING)?
                .unwrap_or(ENCODING_LINEAR);
            validate_encoding(encoding, "ProfileLookTableEncoding")?;
            Some(
                parse_hue_sat_map(
                    ifd,
                    TAG_LOOK_TABLE_DATA,
                    Some(dims.as_slice()),
                    encoding,
                    "ProfileLookTableData",
                )?
                .ok_or_else(|| "ProfileLookTableDims present without data".to_string())?,
            )
        } else {
            if ifd.f64_list(TAG_LOOK_TABLE_DATA)?.is_some() {
                warnings.push(
                    "ProfileLookTableData present without ProfileLookTableDims; ignored"
                        .to_string(),
                );
            }
            None
        };

        // ------------------------------------------------------- tone curve
        let tone_curve = if let Some(values) = ifd.f64_list(TAG_PROFILE_TONE_CURVE)? {
            if values.len() % 2 != 0 || values.len() < 4 {
                return Err(format!(
                    "ProfileToneCurve must hold an even number >= 4 of knots, got {}",
                    values.len()
                ));
            }
            let knots: Vec<(f64, f64)> = values
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| (pair[0], pair[1]))
                .collect();
            // `dng_tone_curve::IsValid` (`dng_tone_curve.cpp:73-107`): >= 2
            // points, every h and v inside [0,1], h strictly increasing. The
            // SDK gates the curve on IsValid at render time — an invalid
            // curve is *absent*, not a parse failure, so warn and drop it.
            // (The spec's first==(0,0)/last==(1,1) endpoint rule is
            // deliberately not checked — the SDK doesn't, and real files
            // deviate.)
            let valid = knots.len() <= 8192
                && knots.iter().all(|(h, v)| {
                    h.is_finite()
                        && v.is_finite()
                        && (0.0..=1.0).contains(h)
                        && (0.0..=1.0).contains(v)
                })
                && knots.windows(2).all(|w| w[1].0 > w[0].0);
            if valid {
                Some(knots)
            } else {
                warnings.push(
                    "ProfileToneCurve fails SDK IsValid; treated as absent (identity)".to_string(),
                );
                None
            }
        } else {
            None
        };

        Ok(Self {
            profile_name,
            unique_camera_model,
            copyright,
            embed_policy,
            default_black_render,
            baseline_exposure_offset,
            matrices,
            hue_sat_maps: vec![hsm_data1, hsm_data2],
            look_table,
            tone_curve,
            warnings,
            unsupported,
        })
    }

    /// Compare `UniqueCameraModel` against the RAW's make/model strings.
    pub(crate) fn camera_match(&self, make: &str, model: &str) -> CameraMatch {
        let Some(dcp_model) = self.unique_camera_model.as_deref() else {
            return CameraMatch::Unknown;
        };
        if model.trim().is_empty() {
            return CameraMatch::Unknown;
        }

        let normalize = |s: &str| -> String {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        };
        let dcp_norm = normalize(dcp_model);
        let make_norm = normalize(make);
        let model_norm = normalize(model);
        let full_norm = if make_norm.is_empty() {
            model_norm.clone()
        } else {
            format!("{make_norm} {model_norm}")
        };

        if dcp_norm == full_norm {
            return CameraMatch::Exact;
        }
        // The model alone can match when one side omits the maker
        // (e.g. DCP "EOS R5" vs RAW make "Canon" + model "EOS R5").
        // A suffix like "EOS R5 Mark II" is a *different* camera and must
        // not pass.
        if dcp_norm == model_norm {
            return CameraMatch::Probable;
        }
        CameraMatch::Mismatch
    }

    /// One-line identity for diagnostics: `"Adobe Standard" @ path`.
    pub(crate) fn display_name(&self) -> String {
        self.profile_name
            .clone()
            .or_else(|| self.unique_camera_model.clone())
            .unwrap_or_else(|| "unnamed DCP".to_string())
    }

    /// Illuminant names for diagnostics.
    pub(crate) fn illuminant_labels(&self) -> Vec<String> {
        self.matrices
            .iter()
            .map(|m| format!("{}({})", illuminant_name(m.illuminant), m.illuminant))
            .collect()
    }
}

/// A ColorMatrix that must exist and be a 3x3.
fn required_matrix(ifd: &super::tiff::Ifd, tag: u16, name: &str) -> Result<[[f64; 3]; 3], String> {
    optional_matrix(ifd, tag, name)?
        .ok_or_else(|| format!("DCP is missing required {name} (tag {tag})"))
}

/// Reads a 3x3 matrix tag; `Ok(None)` when absent, `Err` when present but not
/// exactly 9 finite values (a >3-channel matrix is a different profile class
/// we do not pretend to handle).
fn optional_matrix(
    ifd: &super::tiff::Ifd,
    tag: u16,
    name: &str,
) -> Result<Option<[[f64; 3]; 3]>, String> {
    let Some(values) = ifd.f64_list(tag)? else {
        return Ok(None);
    };
    if values.len() == 9 {
        let mut m = [[0.0f64; 3]; 3];
        for (i, v) in values.iter().enumerate() {
            if !v.is_finite() {
                return Err(format!("{name} contains a non-finite value"));
            }
            m[i / 3][i % 3] = *v;
        }
        return Ok(Some(m));
    }
    if values.len() % 3 == 0 && values.len() > 9 {
        return Err(format!(
            "unsupported DCP feature: {name} has {} entries (>3 camera channels)",
            values.len()
        ));
    }
    Err(format!(
        "malformed {name}: expected 9 entries, got {}",
        values.len()
    ))
}

/// `ProfileHueSatMapDims`/`ProfileLookTableDims` + data → the shared
/// [`LookTable`] grid type (identical layout and encoding semantics as the
/// XMP `LookTable`: value-hue-sat entry order, `[hueShiftDeg, satScale,
/// valScale]` entries, `encoding` 0/1).
fn parse_hue_sat_map(
    ifd: &super::tiff::Ifd,
    data_tag: u16,
    dims: Option<&[u32]>,
    encoding: u32,
    name: &str,
) -> Result<Option<LookTable>, String> {
    let Some(values) = ifd.f64_list(data_tag)? else {
        return Ok(None);
    };
    let Some(dims) = dims else {
        return Err(format!("{name} present without a dims tag"));
    };
    // The SDK accepts `CheckTagCount` 2..=3 on the dims tags
    // (`dng_shared.cpp`); a 2-entry dims implies `valDivisions = 1`.
    if !(2..=3).contains(&dims.len()) {
        return Err(format!(
            "{name} dims must have 2..=3 entries, got {}",
            dims.len()
        ));
    }
    let hue = dims[0];
    let sat = dims[1];
    let val = if dims.len() == 2 {
        1
    } else {
        dims[2].max(1) // dng_hue_sat_map::SetDivisions folds 0 -> 1.
    };

    if hue < 1 {
        return Err(format!("{name} hue divisions {hue} out of range"));
    }
    if !(MIN_SAT_DIVISIONS..=256).contains(&sat) {
        return Err(format!("{name} sat divisions {sat} out of range"));
    }
    if !(1..=256).contains(&val) {
        return Err(format!("{name} val divisions {val} out of range"));
    }

    let (hue, sat, val) = (hue as usize, sat as usize, val as usize);
    let total = hue as u64 * sat as u64 * val as u64;
    if total > MAX_DCP_TABLE_SAMPLES {
        return Err(format!("{name} exceeds {MAX_DCP_TABLE_SAMPLES} samples"));
    }
    let full = total * 3;
    // `dng_camera_profile::ReadHueSatMap`: a compact wire form omits the
    // s=0 row of each (v, h) cell — `deltas count == h*(s-1)*v`.
    let skip_sat0 = hue as u64 * (sat as u64 - 1) * val as u64 * 3;

    let mut entries: Vec<[f32; 3]> = if values.len() as u64 == full {
        values
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| [t[0] as f32, t[1] as f32, t[2] as f32])
            .collect()
    } else if values.len() as u64 == skip_sat0 {
        // Expand s=0 from the file's s=1 row with `valScale = 1.0` — the
        // exact fill `dng_hue_sat_map::SetDelta` performs on a first-sat
        // write when the zero-sat entry still holds its zeroed default.
        let mut out = Vec::with_capacity(total as usize);
        for v in 0..val {
            for h in 0..hue {
                let cell = (v * hue + h) * (sat - 1) * 3;
                out.push([values[cell] as f32, values[cell + 1] as f32, 1.0]);
                for s in 1..sat {
                    let src = cell + (s - 1) * 3;
                    out.push([
                        values[src] as f32,
                        values[src + 1] as f32,
                        values[src + 2] as f32,
                    ]);
                }
            }
        }
        out
    } else {
        return Err(format!(
            "{name}: expected {full} floats (or compact {skip_sat0}), got {}",
            values.len()
        ));
    };

    for entry in &entries {
        for v in entry {
            if !v.is_finite() {
                return Err(format!("{name} contains a non-finite entry"));
            }
        }
    }
    // `SetDelta` forces `valScale = 1.0` on every s=0 entry — a value other
    // than 1.0 would monotone-shift a zero-saturation pixel.
    for v in 0..val {
        for h in 0..hue {
            entries[(v * hue + h) * sat][2] = 1.0;
        }
    }

    Ok(Some(LookTable {
        hue_divisions: hue as u32,
        sat_divisions: sat as u32,
        val_divisions: val as u32,
        entries,
        encoding,
        flags: None,
    }))
}

/// Only `encoding_Linear` (0) and `encoding_sRGB` (1) exist in the spec.
fn validate_encoding(encoding: u32, name: &str) -> Result<(), String> {
    if encoding != ENCODING_LINEAR && encoding != ENCODING_SRGB {
        return Err(format!("unsupported {name} value {encoding}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a little-endian DCP with the given entries.
    fn build_dcp(entries: &[(u16, u16, u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&0x2Au16.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        let count = entries.len() as u16;
        out.extend_from_slice(&count.to_le_bytes());
        let table_len = 2 + 12 * entries.len() + 4;
        let mut data_area = Vec::new();
        for (tag, ty, n, bytes) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&n.to_le_bytes());
            if bytes.len() <= 4 {
                out.extend_from_slice(bytes);
                out.resize(out.len() + (4 - bytes.len()), 0);
            } else {
                let offset = (8 + table_len + data_area.len()) as u32;
                out.extend_from_slice(&offset.to_le_bytes());
                data_area.extend_from_slice(bytes);
            }
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&data_area);
        out
    }

    fn srationals(values: &[i32]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in values {
            v.extend_from_slice(&n.to_le_bytes());
            v.extend_from_slice(&1i32.to_le_bytes());
        }
        v
    }

    fn floats(values: &[f32]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in values {
            v.extend_from_slice(&n.to_le_bytes());
        }
        v
    }

    fn shorts(values: &[u16]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in values {
            v.extend_from_slice(&n.to_le_bytes());
        }
        v
    }

    fn longs(values: &[u32]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in values {
            v.extend_from_slice(&n.to_le_bytes());
        }
        v
    }

    /// Minimal valid DCP: model string, one illuminant (D65), identity-ish
    /// ColorMatrix1.
    fn minimal_dcp() -> Vec<u8> {
        build_dcp(&[
            (TAG_UNIQUE_CAMERA_MODEL, 2, 13, b"Canon EOS R5\0".to_vec()),
            (TAG_PROFILE_NAME, 2, 15, b"Adobe Standard\0".to_vec()),
            (TAG_CALIBRATION_ILLUMINANT1, 3, 1, shorts(&[21])),
            (
                TAG_COLOR_MATRIX1,
                10,
                9,
                srationals(&[1, 0, 0, 0, 1, 0, 0, 0, 1]),
            ),
        ])
    }

    #[test]
    fn parses_minimal_dcp() {
        let dcp = DcpProfile::parse(&minimal_dcp()).expect("minimal DCP parses");
        assert_eq!(dcp.profile_name.as_deref(), Some("Adobe Standard"));
        assert_eq!(dcp.unique_camera_model.as_deref(), Some("Canon EOS R5"));
        assert_eq!(dcp.matrices.len(), 1);
        assert_eq!(dcp.matrices[0].illuminant, 21);
        assert!(dcp.hue_sat_maps.iter().all(|m| m.is_none()));
        assert!(dcp.look_table.is_none());
        assert!(dcp.tone_curve.is_none());
    }

    #[test]
    fn missing_color_matrix_is_an_error() {
        let bytes = build_dcp(&[(TAG_UNIQUE_CAMERA_MODEL, 2, 5, b"Test\0".to_vec())]);
        assert!(DcpProfile::parse(&bytes).is_err());
    }

    #[test]
    fn triple_illuminant_is_reported_unsupported() {
        // Rebuild with a third illuminant.
        let dcp = build_dcp(&[
            (TAG_CALIBRATION_ILLUMINANT1, 3, 1, shorts(&[21])),
            (
                TAG_COLOR_MATRIX1,
                10,
                9,
                srationals(&[1, 0, 0, 0, 1, 0, 0, 0, 1]),
            ),
            (TAG_CALIBRATION_ILLUMINANT3, 3, 1, shorts(&[20])),
        ]);
        assert!(
            DcpProfile::parse(&dcp)
                .unwrap_err()
                .contains("triple-illuminant")
        );
    }

    #[test]
    fn hue_sat_map_decodes_into_look_table_shape() {
        // dims: hue=2, sat=2, val=1 → 4 entries.
        let entries: Vec<f32> = vec![
            10.0, 1.0, 1.0, // h0 s0
            20.0, 1.0, 1.0, // h0 s1
            -10.0, 1.0, 1.0, // h1 s0
            -20.0, 1.0, 1.0, // h1 s1
        ];
        let dcp = build_dcp(&[
            (TAG_CALIBRATION_ILLUMINANT1, 3, 1, shorts(&[21])),
            (
                TAG_COLOR_MATRIX1,
                10,
                9,
                srationals(&[1, 0, 0, 0, 1, 0, 0, 0, 1]),
            ),
            (TAG_HUE_SAT_MAP_DIMS, 4, 3, longs(&[2, 2, 1])),
            (TAG_HUE_SAT_MAP_DATA1, 11, 12, floats(&entries)),
        ]);
        let profile = DcpProfile::parse(&dcp).unwrap();
        let hsm = profile.hue_sat_maps[0].as_ref().expect("HSM1 parsed");
        assert_eq!(hsm.hue_divisions, 2);
        assert_eq!(hsm.sat_divisions, 2);
        assert_eq!(hsm.val_divisions, 1);
        assert_eq!(hsm.entries.len(), 4);
        assert_eq!(hsm.entries[0], [10.0, 1.0, 1.0]);
        assert_eq!(hsm.entries[3], [-20.0, 1.0, 1.0]);
    }

    #[test]
    fn hue_sat_map_data_without_dims_is_malformed() {
        let dcp = build_dcp(&[
            (TAG_CALIBRATION_ILLUMINANT1, 3, 1, shorts(&[21])),
            (
                TAG_COLOR_MATRIX1,
                10,
                9,
                srationals(&[1, 0, 0, 0, 1, 0, 0, 0, 1]),
            ),
            (TAG_HUE_SAT_MAP_DATA1, 11, 6, floats(&[0.0; 6])),
        ]);
        assert!(DcpProfile::parse(&dcp).is_err());
    }

    #[test]
    fn tone_curve_pairs_parse() {
        let knots = [0.0f32, 0.0, 0.5, 0.6, 1.0, 1.0];
        let dcp = build_dcp(&[
            (TAG_CALIBRATION_ILLUMINANT1, 3, 1, shorts(&[21])),
            (
                TAG_COLOR_MATRIX1,
                10,
                9,
                srationals(&[1, 0, 0, 0, 1, 0, 0, 0, 1]),
            ),
            (TAG_PROFILE_TONE_CURVE, 11, 6, floats(&knots)),
        ]);
        let profile = DcpProfile::parse(&dcp).unwrap();
        let curve = profile.tone_curve.as_ref().unwrap();
        assert_eq!(curve.len(), 3);
        assert!((curve[1].0 - 0.5).abs() < 1e-6);
        assert!((curve[1].1 - 0.6).abs() < 1e-6);
    }

    #[test]
    fn camera_matching() {
        let dcp = DcpProfile::parse(&minimal_dcp()).unwrap();
        assert_eq!(dcp.camera_match("Canon", "EOS R5"), CameraMatch::Exact);
        // A RAW carrying the whole model string in `model` still exacts.
        assert_eq!(dcp.camera_match("", "Canon EOS R5"), CameraMatch::Exact);
        // A DCP naming only the model (no maker) is a probable match.
        let model_only = build_dcp(&[
            (TAG_UNIQUE_CAMERA_MODEL, 2, 7, b"EOS R5\0".to_vec()),
            (TAG_CALIBRATION_ILLUMINANT1, 3, 1, shorts(&[21])),
            (
                TAG_COLOR_MATRIX1,
                10,
                9,
                srationals(&[1, 0, 0, 0, 1, 0, 0, 0, 1]),
            ),
        ]);
        let dcp = DcpProfile::parse(&model_only).unwrap();
        assert_eq!(dcp.camera_match("Canon", "EOS R5"), CameraMatch::Probable);
        let dcp = DcpProfile::parse(&minimal_dcp()).unwrap();
        assert_eq!(
            dcp.camera_match("Canon", "EOS R5 Mark II"),
            CameraMatch::Mismatch
        );
        assert_eq!(dcp.camera_match("NIKON", "Z 9"), CameraMatch::Mismatch);
        assert_eq!(dcp.camera_match("", ""), CameraMatch::Unknown);
    }
}

/// Owner/local fixture gate: set `ONCELAB_DCP_FIXTURE=/path/to/real.dcp`
/// to parse a licensed DCP end-to-end. Nothing is committed; the test is
/// a no-op without the variable.
#[test]
fn real_dcp_fixture_if_provided() {
    let Ok(path) = std::env::var("ONCELAB_DCP_FIXTURE") else {
        eprintln!("SKIPPED: ONCELAB_DCP_FIXTURE not set");
        return;
    };
    let bytes = std::fs::read(&path).expect("read DCP fixture");
    let profile = DcpProfile::parse(&bytes).expect("real DCP must parse");
    eprintln!(
        "DCP: name={:?} model={:?} illuminants={:?} hsm={:?} look={} tone={:?} warn={:?} unsup={:?}",
        profile.profile_name,
        profile.unique_camera_model,
        profile.illuminant_labels(),
        profile
            .hue_sat_maps
            .iter()
            .map(|m| m
                .as_ref()
                .map(|t| (t.hue_divisions, t.sat_divisions, t.val_divisions)))
            .collect::<Vec<_>>(),
        profile.look_table.is_some(),
        profile.tone_curve.as_ref().map(Vec::len),
        profile.warnings,
        profile.unsupported,
    );
    assert!(profile.matrices.len() == 1 || profile.matrices.len() == 2);
}
