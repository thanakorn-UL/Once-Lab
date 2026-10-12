//! The per-pixel camera-profile render stages, following the public DNG SDK's
//! `dng_render_task::Process` order for SDR output:
//!
//! ```text
//! normalized camera RGB
//!   -> clip at cameraWhite + CameraToPCS + XYZ->ProPhoto(D50), pin [0,1]
//!      (`RefBaselineABCtoRGB`, `dng_reference.cpp:1389`)
//!   -> ProfileHueSatMap (temp-interpolated; `DoBaselineHueSatMap`)
//!   -> exposure ramp (`dng_function_exposure_ramp`)
//!   -> ProfileLookTable (`DoBaselineHueSatMap` again)
//!   -> [ ProfileToneCurve — applied by the integration stage immediately
//!        before the Creative RGBTable, matching `dng_render.cpp` ]
//! ```
//!
//! Everything the SDK does per pixel is implemented; anything the profile
//! lacks is skipped, never approximated. Where the SDK references negative-
//! side state this app does not model (CameraCalibration, AnalogBalance,
//! user exposure, DNG BaselineExposure/ShadowScale), the identity value is
//! used and recorded in the diagnostic report.

use super::color_spec::{DcpColorSpec, Xy, illuminant_to_temperature, temperature_from_xy};
use super::dcp::{CameraMatch, DEFAULT_BLACK_RENDER_NONE, DcpProfile};
use super::spline::{BaselineToneCurve, DngSpline};
use crate::xmp_profile::LookTable;
use crate::xmp_profile::LookTableAdobeContext;

/// The interpolated hue/sat map for one scene white, per
/// `dng_camera_profile::HueSatMapForWhite` (single + dual illuminant;
/// triple is rejected earlier).
fn hue_sat_map_for_white(dcp: &DcpProfile, white: Xy) -> Option<LookTable> {
    let map1 = dcp.hue_sat_maps.first()?.as_ref()?;
    // `HueSatMapForWhite`: a single valid table is used for every white;
    // the dual path needs both tables *and* both illuminant temperatures.
    let (Some(map2), Some(illum2)) = (
        dcp.hue_sat_maps.get(1).and_then(|m| m.as_ref()),
        dcp.matrices.get(1).map(|m| m.illuminant),
    ) else {
        return Some(map1.clone());
    };

    // `HueSatMapForWhite_Dual` reads the authored CalibrationTemperatures in
    // authored order (not the color-spec's swapped order) and flips `g` when
    // out of order — reproduce that exactly.
    let mut t1 = illuminant_to_temperature(dcp.matrices[0].illuminant).unwrap_or(0.0);
    let mut t2 = illuminant_to_temperature(illum2).unwrap_or(0.0);
    if t1 <= 0.0 || t2 <= 0.0 || t1 == t2 {
        return Some(map1.clone());
    }
    let reverse_order = t1 > t2;
    if reverse_order {
        std::mem::swap(&mut t1, &mut t2);
    }

    let temperature = temperature_from_xy(white).unwrap_or(t1);
    let mut g = if temperature <= t1 {
        1.0
    } else if temperature >= t2 {
        0.0
    } else {
        (1.0 / temperature - 1.0 / t2) / (1.0 / t1 - 1.0 / t2)
    };
    if reverse_order {
        g = 1.0 - g;
    }

    // `dng_hue_sat_map::Interpolate`: per-entry linear blend, same dims.
    if map1.hue_divisions != map2.hue_divisions
        || map1.sat_divisions != map2.sat_divisions
        || map1.val_divisions != map2.val_divisions
        || map1.entries.len() != map2.entries.len()
    {
        return None;
    }
    if g >= 1.0 {
        return Some(map1.clone());
    }
    if g <= 0.0 {
        return Some(map2.clone());
    }

    let w1 = g as f32;
    let w2 = 1.0 - w1;
    let entries = map1
        .entries
        .iter()
        .zip(map2.entries.iter())
        .map(|(a, b)| {
            [
                w1 * a[0] + w2 * b[0],
                w1 * a[1] + w2 * b[1],
                w1 * a[2] + w2 * b[2],
            ]
        })
        .collect();
    Some(LookTable {
        hue_divisions: map1.hue_divisions,
        sat_divisions: map1.sat_divisions,
        val_divisions: map1.val_divisions,
        entries,
        encoding: map1.encoding,
        flags: None,
    })
}

/// `dng_function_exposure_ramp` (`dng_render.cpp`): the per-channel exposure /
/// black-mapping ramp. With `exposure_ev = 0` the only effect is the SDK's
/// default shadow rolloff (`fShadows = 5.0` unless the profile asks for none).
#[derive(Debug, Clone)]
pub(crate) struct ExposureRamp {
    slope: f64,
    black: f64,
    radius: f64,
    qscale: f64,
    /// `false` when the ramp is a true identity (EV 0, shadows 0).
    pub(crate) active: bool,
    /// Diagnostic label for reports/diagnostics.
    pub(crate) label: String,
}

impl ExposureRamp {
    /// `white = 1 / 2^max(0, exposure)`, `black = shadows * shadowScale *
    /// stage3Gain * 0.001` pinned below `0.99 * white`.
    ///
    /// `pub(crate)` so the Rawler-fallback baseline can explicitly opt into
    /// the SDK render-default ramp (`Shadows=5`, `ShadowScale=1`, EV 0).
    pub(crate) fn new(exposure_ev: f64, shadows: f64, shadow_scale: f64) -> Self {
        let white = 1.0 / (2.0f64).powf(exposure_ev.max(0.0));
        let black = (shadows * shadow_scale * 0.001).min(0.99 * white);

        let slope = 1.0 / (white - black);
        let radius = (0.5 * black).min((1.0 / 16.0) / slope);
        let qscale = if radius > 0.0 {
            slope / (4.0 * radius)
        } else {
            0.0
        };

        let active = black > 0.0 || exposure_ev != 0.0;
        let label = if active {
            format!(
                "exposure-ramp(exposure={exposure_ev:.3}EV, shadows={shadows}, black={black:.6})"
            )
        } else {
            "identity (exposure=0, shadows=0)".to_string()
        };

        Self {
            slope,
            black,
            radius,
            qscale,
            active,
            label,
        }
    }

    pub(crate) fn evaluate(&self, x: f32) -> f32 {
        let x = f64::from(x);
        if x <= self.black - self.radius {
            return 0.0;
        }
        if x >= self.black + self.radius {
            return ((x - self.black) * self.slope).min(1.0) as f32;
        }
        let y = x - (self.black - self.radius);
        (self.qscale * y * y) as f32
    }
}

/// Everything needed to run a loaded DCP against one RAW file's white balance.
pub(crate) struct DcpRender {
    /// `fCameraToRGB` = `ProPhoto.MatrixFromPCS * CameraToPCS` (camera→linear
    /// ProPhoto D50), f64-composed then narrowed once.
    camera_to_rgb: [[f32; 3]; 3],
    /// `fCameraWhite` clip points (max-normalized, pinned [0.001, 1]).
    camera_white: [f32; 3],
    hue_sat_map: Option<LookTableAdobeContext<'static>>,
    look_table: Option<LookTableAdobeContext<'static>>,
    exposure_ramp: ExposureRamp,
    tone_curve: Option<DngSpline>,
    /// Diagnostic fields for [`super::AdobePipelineReport`].
    pub report: RenderReport,
}

/// Serializable summary of the resolved camera-profile render, suitable for
/// the diagnostic panel.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderReport {
    pub profile_name: String,
    pub scene_white_xy: [f64; 2],
    pub scene_temperature: Option<f64>,
    pub interpolation_weight: f64,
    pub matrix_path: String,
    pub illuminants: Vec<String>,
    pub camera_white: [f32; 3],
    pub hue_sat_map: String,
    pub look_table: String,
    pub exposure_stage: String,
    /// `BaselineExposureOffset` (EV) authored by the profile, when present.
    pub baseline_exposure_offset_ev: Option<f64>,
    pub tone_curve: String,
    pub match_kind: String,
}

impl DcpRender {
    /// Resolve the DCP against the RAW's decoded white balance.
    ///
    /// `wb_coeffs` are Rawler's as-shot multipliers; the as-shot neutral
    /// (camera-space neutral encoding) is their reciprocal.
    pub(crate) fn new(
        dcp: DcpProfile,
        wb_coeffs: [f32; 4],
        camera_make: &str,
        camera_model: &str,
    ) -> Result<Self, String> {
        let spec = DcpColorSpec::new(dcp.matrices.clone())?;

        let neutral = if wb_coeffs[0].is_nan() {
            [1.0f64; 3]
        } else {
            [
                1.0 / f64::from(wb_coeffs[0]),
                1.0 / f64::from(wb_coeffs[1]),
                1.0 / f64::from(wb_coeffs[2]),
            ]
        };

        let white = spec.neutral_to_xy(neutral).unwrap_or(Xy::D50);
        let resolved = spec.set_white_xy(white)?;

        // fCameraToRGB = MatrixFromPCS(ProPhoto) * CameraToPCS.
        let prophoto_from_pcs = crate::xmp_profile::adobe_xyz_to_linear_prophoto_matrix();
        let camera_to_rgb_f64 =
            super::color_spec::mat_mul(&prophoto_from_pcs, &resolved.camera_to_pcs);
        let mut camera_to_rgb = [[0.0f32; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                if !camera_to_rgb_f64[i][j].is_finite() {
                    return Err("camera->ProPhoto transform is non-finite".to_string());
                }
                camera_to_rgb[i][j] = camera_to_rgb_f64[i][j] as f32;
            }
        }

        let camera_white = [
            resolved.camera_white[0] as f32,
            resolved.camera_white[1] as f32,
            resolved.camera_white[2] as f32,
        ];

        let hue_sat_map = hue_sat_map_for_white(&dcp, white)
            .map(|table| {
                LookTableAdobeContext::new_with_cap(
                    table,
                    super::dcp::MAX_DCP_TABLE_SAMPLES as usize,
                    false,
                )
                .map_err(|e| format!("ProfileHueSatMap unusable: {e}"))
            })
            .transpose()?;

        let look_table = dcp
            .look_table
            .clone()
            .map(|table| {
                LookTableAdobeContext::new_with_cap(
                    table,
                    super::dcp::MAX_DCP_TABLE_SAMPLES as usize,
                    false,
                )
                .map_err(|e| format!("ProfileLookTable unusable: {e}"))
            })
            .transpose()?;

        let (tone_curve, tone_label) = match dcp.tone_curve.as_deref() {
            Some(knots) => match DngSpline::solve(knots) {
                Ok(spline) => {
                    let label = if spline.is_identity() {
                        "identity (authored (0,0)->(1,1) ProfileToneCurve)".to_string()
                    } else {
                        format!("ProfileToneCurve ({} knots)", knots.len())
                    };
                    (Some(spline), label)
                }
                Err(e) => (None, format!("identity (invalid ProfileToneCurve: {e})")),
            },
            None => (None, "identity (no ProfileToneCurve in DCP)".to_string()),
        };

        let shadows = if dcp.default_black_render == Some(DEFAULT_BLACK_RENDER_NONE) {
            0.0
        } else {
            // The SDK sample renderer's default (`dng_render` ctor).
            5.0
        };
        let exposure_ev = dcp.baseline_exposure_offset.unwrap_or(0.0);
        let exposure_ramp = ExposureRamp::new(exposure_ev, shadows, 1.0);
        let exposure_label = exposure_ramp.label.clone();

        let match_kind = match dcp.camera_match(camera_make, camera_model) {
            CameraMatch::Exact => "exact".to_string(),
            CameraMatch::Probable => "probable".to_string(),
            CameraMatch::Mismatch => "mismatch".to_string(),
            CameraMatch::Unknown => "unknown".to_string(),
        };

        Ok(Self {
            camera_to_rgb,
            camera_white,
            hue_sat_map,
            look_table,
            exposure_ramp,
            tone_curve,
            report: RenderReport {
                profile_name: dcp.display_name(),
                scene_white_xy: [white.x, white.y],
                scene_temperature: resolved.white_temperature,
                interpolation_weight: resolved.interpolation_weight,
                matrix_path: format!("{:?}", resolved.path),
                illuminants: resolved
                    .illuminants
                    .iter()
                    .map(|i| format!("{}({})", super::color_spec::illuminant_name(*i), i))
                    .collect(),
                camera_white,
                hue_sat_map: dcp_label(&dcp.hue_sat_maps),
                look_table: dcp
                    .look_table
                    .as_ref()
                    .map(|t| {
                        format!(
                            "ProfileLookTable {}x{}x{} enc={}",
                            t.hue_divisions, t.sat_divisions, t.val_divisions, t.encoding
                        )
                    })
                    .unwrap_or_else(|| "none".to_string()),
                exposure_stage: exposure_label,
                baseline_exposure_offset_ev: dcp.baseline_exposure_offset,
                tone_curve: tone_label,
                match_kind,
            },
        })
    }

    /// `RefBaselineABCtoRGB` for one normalized camera pixel: clip at the
    /// camera-white point, apply `CameraToRGB`, pin to [0,1].
    #[inline]
    pub(crate) fn apply_camera(&self, pixel: &mut [f32; 3]) {
        let a = pixel[0].min(self.camera_white[0]);
        let b = pixel[1].min(self.camera_white[1]);
        let c = pixel[2].min(self.camera_white[2]);
        let m = &self.camera_to_rgb;
        *pixel = [
            (m[0][0] * a + m[0][1] * b + m[0][2] * c).clamp(0.0, 1.0),
            (m[1][0] * a + m[1][1] * b + m[1][2] * c).clamp(0.0, 1.0),
            (m[2][0] * a + m[2][1] * b + m[2][2] * c).clamp(0.0, 1.0),
        ];
    }

    /// The stages between the camera transform and the tone curve:
    /// HueSatMap -> exposure ramp -> LookTable (SDK order).
    #[inline]
    pub(crate) fn apply_scene(&self, pixel: &mut [f32; 3]) {
        if let Some(hsm) = &self.hue_sat_map {
            *pixel = hsm.apply(*pixel);
        }
        if self.exposure_ramp.active {
            for channel in pixel.iter_mut() {
                *channel = self.exposure_ramp.evaluate(*channel);
            }
        }
        if let Some(look) = &self.look_table {
            *pixel = look.apply(*pixel);
        }
    }

    /// The baseline tone stage's tone curve, when the DCP authored one.
    /// Returns `None` for the explicit identity fallback.
    pub(crate) fn tone_curve(&self) -> Option<&dyn BaselineToneCurve> {
        self.tone_curve
            .as_ref()
            .map(|s| s as &dyn BaselineToneCurve)
    }
}

fn dcp_label(maps: &[Option<LookTable>]) -> String {
    let mut parts = Vec::new();
    for (i, map) in maps.iter().enumerate() {
        if let Some(t) = map {
            parts.push(format!(
                "ProfileHueSatMapData{} {}x{}x{}",
                i + 1,
                t.hue_divisions,
                t.sat_divisions,
                t.val_divisions
            ));
        }
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(" + ")
    }
}

#[cfg(test)]
mod tests {
    use super::super::color_spec::IlluminantMatrices;
    use super::super::dcp::DcpProfile;
    use super::*;

    fn hsm(hue_shift: f32) -> LookTable {
        // 2 hue x 2 sat x 1 val, every entry the same rotation.
        LookTable {
            hue_divisions: 2,
            sat_divisions: 2,
            val_divisions: 1,
            entries: vec![
                [hue_shift, 1.0, 1.0],
                [hue_shift, 1.0, 1.0],
                [hue_shift, 1.0, 1.0],
                [hue_shift, 1.0, 1.0],
            ],
            encoding: 0,
            flags: None,
        }
    }

    fn dcp_with(m1: [[f64; 3]; 3]) -> DcpProfile {
        DcpProfile {
            profile_name: Some("Test".to_string()),
            unique_camera_model: Some("TestCam".to_string()),
            copyright: None,
            embed_policy: None,
            default_black_render: None,
            baseline_exposure_offset: None,
            matrices: vec![IlluminantMatrices {
                illuminant: 21,
                color_matrix: m1,
                forward_matrix: None,
            }],
            hue_sat_maps: vec![None, None],
            look_table: None,
            tone_curve: None,
            warnings: vec![],
            unsupported: vec![],
        }
    }

    #[test]
    fn exposure_ramp_identity_when_shadows_zero() {
        let ramp = ExposureRamp::new(0.0, 0.0, 1.0);
        assert!(!ramp.active);
        for x in [0.0, 0.25, 0.5, 1.0] {
            assert_eq!(ramp.evaluate(x), (x as f64).min(1.0) as f32);
        }
    }

    #[test]
    fn exposure_ramp_subtracts_the_black_pedestal() {
        // SDK default (shadows=5): black = 0.005, slope = 1/(1-0.005).
        let ramp = ExposureRamp::new(0.0, 5.0, 1.0);
        assert!(ramp.active);
        // Deep blacks crush to 0; the pedestal is subtracted elsewhere.
        assert_eq!(ramp.evaluate(0.0), 0.0);
        assert!((ramp.evaluate(0.01) as f64 - 0.005 * (1.0 / 0.995)).abs() < 1e-4);
        assert!((ramp.evaluate(0.5) as f64 - (0.5 - 0.005) / 0.995).abs() < 1e-4);
        assert!((ramp.evaluate(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn apply_camera_clips_at_camera_white() {
        let dcp = dcp_with([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
        // wb neutral => scene white comes from the spec itself.
        let render = DcpRender::new(dcp, [1.0, 1.0, 1.0, 1.0], "Test", "TestCam").unwrap();
        let mut p = [5.0, 0.2, 0.1];
        render.apply_camera(&mut p);
        // Values above camera white are clipped at the clip point, then matrixed
        // and pinned; output must be inside [0,1] by contract.
        for c in p {
            assert!((0.0..=1.0).contains(&c));
        }
    }

    #[test]
    fn hue_sat_map_single_table_used_for_any_white() {
        let mut dcp = dcp_with([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
        dcp.hue_sat_maps = vec![Some(hsm(30.0)), None];
        let map = super::hue_sat_map_for_white(&dcp, Xy::D50).unwrap();
        assert_eq!(map.entries[0][0], 30.0);
    }

    #[test]
    fn hue_sat_map_dual_interpolates_by_temperature() {
        // Authored order: StdA(2850) table then D65(6500) table.
        let mut dcp = dcp_with([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
        dcp.matrices
            .push(super::super::color_spec::IlluminantMatrices {
                illuminant: 21,
                color_matrix: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                forward_matrix: None,
            });
        // Reorder: authored matrix1 = StdA (17), matrix2 = D65 (21).
        dcp.matrices[0].illuminant = 17;
        dcp.hue_sat_maps = vec![Some(hsm(0.0)), Some(hsm(60.0))];

        // White near D65 -> weight ~0 for map1 -> ~60 deg shift.
        let d65 = Xy {
            x: 0.31271,
            y: 0.32902,
        };
        let map = super::hue_sat_map_for_white(&dcp, d65).unwrap();
        assert!(
            map.entries[0][0] > 55.0,
            "near-D65 should take map2, got {}",
            map.entries[0][0]
        );

        // White near tungsten -> map1's 0 shift.
        let tung = Xy {
            x: 0.44757,
            y: 0.40745,
        };
        let map = super::hue_sat_map_for_white(&dcp, tung).unwrap();
        assert!(
            map.entries[0][0] < 5.0,
            "near-StdA should take map1, got {}",
            map.entries[0][0]
        );

        // A mid-temperature white blends, not switches.
        let mid = Xy { x: 0.36, y: 0.36 };
        let map = super::hue_sat_map_for_white(&dcp, mid).unwrap();
        let v = map.entries[0][0];
        assert!(v > 1.0 && v < 59.0, "expected blended shift, got {v}");
    }

    #[test]
    fn scene_neutral_stays_neutral_through_identity_profile() {
        // Identity-ish matrix: camera white should map to PCS white.
        let dcp = dcp_with([[0.5, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 2.0]]);
        let render = DcpRender::new(dcp, [2.0, 1.0, 0.5, 1.0], "Test", "TestCam").unwrap();
        // Camera white pixel should render near ProPhoto white (1,1,1).
        let mut p = render.camera_white;
        render.apply_camera(&mut p);
        for c in p {
            assert!(c > 0.9, "white should map high, got {p:?}");
        }
    }
}
