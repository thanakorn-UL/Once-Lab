use crate::camera_profile::render::ExposureRamp;
use crate::camera_profile::spline::{BaselineToneCurve, SampledToneCurve};
use crate::camera_profile::{
    AdobePipelineReport, CameraMatch, CameraProfileChoice, CreativeProfileReport, DcpRender,
    load_dcp_from_path, record_adobe_report,
};
use crate::image_processing::apply_orientation;
use crate::raw_diagnostics::RawPipelineDiagnostics;
use anyhow::{Result, anyhow};
use image::{DynamicImage, ImageBuffer, Rgba};
use rawler::{
    decoders::{Orientation, RawDecodeParams},
    imgop::{
        develop::{DemosaicAlgorithm, Intermediate, ProcessingStep, RawDevelop},
        matrix::{multiply, normalize, pseudo_inverse},
        xyz::{Illuminant, SRGB_TO_XYZ_D65},
    },
    rawimage::{RawImage, RawPhotometricInterpretation},
    rawsource::RawSource,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Optional Adobe-comparison context for [`develop_raw_image_with_profile`].
/// `Some` means the isolated Adobe pipeline runs (the previous `adobe_baseline`
/// flag); `None` is the normal editor develop.
pub(crate) struct AdobeRawContext<'a> {
    /// User-supplied `.dcp` camera profile. Absent or unrenderable keeps the
    /// Rawler color-matrix fallback (reported as such, never claimed as
    /// "Adobe Standard").
    pub camera_profile_path: Option<&'a std::path::Path>,
    /// Source path for the diagnostic report (pixel work never depends on it).
    pub source_path: Option<&'a str>,
    /// Developer-only experiment selector for the profile-less baseline's
    /// exposure/tone defaults (`dng_render` runs the exposure ramp between
    /// camera transform and LookTable, then LookTable -> ProfileToneCurve ->
    /// RGBTables). Deliberately NOT a permanent user-facing feature: modes
    /// select which documented SDK defaults run when the camera profile
    /// authored none. `Identity` preserves the established Once-Lab
    /// baseline. None of these reproduce Lightroom's private PV2012 stack.
    pub baseline_mode: DngBaselineMode,
}

/// The four experiment modes for the DNG baseline tone/exposure defaults.
/// Developer-only — selected via `ONCELAB_ADOBE_DNG_TONE_MODE` or the
/// `adobeDngBaselineMode` settings key, not a permanent UI feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DngBaselineMode {
    /// Established Once-Lab baseline: no baseline tone, no ramp toe.
    #[default]
    Identity,
    /// `dng_tone_curve_acr3_default` (1025-pt sampled table) at the tone
    /// stage when the profile has no ProfileToneCurve.
    Acr3Tone,
    /// SDK render-default exposure ramp (`Shadows=5`, `ShadowScale=1`,
    /// EV 0 → ~0.5% quadratic black toe) on the fallback path.
    ShadowToe,
    /// Both SDK defaults: exposure ramp toe AND the acr3 tone curve.
    Acr3ToneWithShadowToe,
}

impl DngBaselineMode {
    /// True when the acr3 default curve runs at the tone stage.
    pub fn acr3_tone(self) -> bool {
        matches!(self, Self::Acr3Tone | Self::Acr3ToneWithShadowToe)
    }

    /// True when the SDK Shadows=5 ramp runs on the fallback path.
    pub fn shadow_toe(self) -> bool {
        matches!(self, Self::ShadowToe | Self::Acr3ToneWithShadowToe)
    }

    /// Parse a settings/env string; unknown values fall back to Identity.
    pub fn from_label(s: &str) -> Self {
        match s {
            "acr3" | "acr3-tone" | "tone" => Self::Acr3Tone,
            "toe" | "shadow-toe" | "shadows" => Self::ShadowToe,
            "acr3+toe" | "acr3-tone-shadow-toe" | "full" | "both" => Self::Acr3ToneWithShadowToe,
            _ => Self::Identity,
        }
    }
}

/// Develop a RAW file and collect the full stage-boundary diagnostic
/// document (see [`crate::raw_diagnostics`]). This is the same executed
/// pipeline as [`develop_raw_image_with_profile`] — the diagnostics describe
/// real buffers, not re-computed approximations.
pub(crate) fn develop_raw_image_diagnostics(
    file_bytes: &[u8],
    fast_demosaic: bool,
    highlight_compression: f32,
    linear_mode: String,
    adobe: Option<&AdobeRawContext<'_>>,
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
) -> Result<(DynamicImage, RawPipelineDiagnostics)> {
    let mut diag = RawPipelineDiagnostics::new(
        adobe.and_then(|c| c.source_path).unwrap_or("<memory>"),
        adobe.is_some(),
    );
    let (image, orientation) = develop_internal(
        file_bytes,
        fast_demosaic,
        highlight_compression,
        linear_mode,
        None,
        None,
        adobe,
        cancel_token,
        Some(&mut diag),
    )?;
    diag.output_stage = if adobe.is_some() {
        "linear sRGB working buffer -> GPU (tonemapper mode 2 sRGB encode); \
         TIFF export reads back rgba16float (no RGBA8 quantize); sRGB ICC embedded"
            .to_string()
    } else {
        "legacy editor pipeline (Rawler calibrate + GPU adjustments)".to_string()
    };
    let _ = orientation;
    Ok((image, diag))
}

// Retain the public no-profile development entry point for callers outside
// this crate, even though the current internal path uses the profile-aware API.
#[allow(dead_code)]
pub fn develop_raw_image(
    file_bytes: &[u8],
    fast_demosaic: bool,
    highlight_compression: f32,
    linear_mode: String,
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
) -> Result<DynamicImage> {
    develop_raw_image_with_profile(
        file_bytes,
        fast_demosaic,
        highlight_compression,
        linear_mode,
        None,
        None,
        None,
        cancel_token,
    )
}

/// Develops a RAW image, applying `profile` (with the optional Amount control)
/// when one is supplied.
///
/// `adobe` selects the isolated Adobe comparison RAW stage: the unclipped
/// camera -> Linear ProPhoto calibration (or a user-supplied DCP render when
/// `adobe.camera_profile_path` is set), no highlight compression, and a
/// ProPhoto -> linear sRGB conversion at the working-space boundary. `None`
/// is the normal editor develop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn develop_raw_image_with_profile(
    file_bytes: &[u8],
    fast_demosaic: bool,
    highlight_compression: f32,
    linear_mode: String,
    profile: Option<&crate::xmp_profile::XmpRgbProfile>,
    profile_amount_percent: Option<f32>,
    adobe: Option<&AdobeRawContext<'_>>,
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
) -> Result<DynamicImage> {
    let (developed_image, orientation) = develop_internal(
        file_bytes,
        fast_demosaic,
        highlight_compression,
        linear_mode,
        profile,
        profile_amount_percent,
        adobe,
        cancel_token,
        None,
    )?;
    Ok(apply_orientation(developed_image, orientation))
}

/// Signed variant of Rawler's `correct_blacklevel_cfa`: the same
/// `(v - black_pos) / (white_pos - black_pos)` normalization, minus the
/// `clip()` that zeros sub-black values before demosaic. The DNG SDK's
/// stage3 linearization preserves signed values (it only force-clips at
/// black in dedicated dst-black paths, which don't apply to rendering).
/// `pos` is the CFA position index `(y%2)*2 + (x%2)` — the exact indexing
/// `correct_blacklevel_cfa` uses.
fn correct_blacklevel_cfa_signed(
    data: &mut [f32],
    width: usize,
    blacks: &[f32; 4],
    denom: &[f32; 4],
) {
    for (i, v) in data.iter_mut().enumerate() {
        let pos = ((i / width) % 2) * 2 + ((i % width) % 2);
        *v = (*v - blacks[pos]) / denom[pos];
    }
}

/// Signed-preserving variant of `RawDevelop::develop_intermediate` for the
/// single-sample 3-color CFA case (Adobe comparison path only). Same stage
/// semantics as Rawler's `Rescale -> Demosaic(active area) -> CropDefault`
/// pipeline, minus the pre-demosaic black clip — so the demosaic input is
/// the true signed-linearized mosaic, not a shifted approximation. Because
/// the input domain is unchanged (no pedestal/offset), adaptive demosaic
/// decisions — PPG's gradient branch selection and `hue_transit` ties —
/// see the same values the signed ideal would produce, bit for bit.
fn develop_intermediate_signed_cfa(
    raw: &RawImage,
    demosaic_algorithm: DemosaicAlgorithm,
) -> Result<Intermediate> {
    use rawler::imgop::sensor::bayer::{
        Demosaic, ppg::PPGDemosaic, superpixel::SuperpixelQuarterRes3Channel,
    };
    use rawler::imgop::sensor::xtrans::demosaic::{XTransDemosaic, XTransSuperpixelDemosaic};
    use rawler::pixarray::Pix2D;

    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        anyhow::bail!("signed CFA develop requires a CFA photometric interpretation");
    };
    if raw.cpp != 1 || !config.cfa.is_rgb() || config.cfa.unique_colors() == 4 {
        anyhow::bail!("signed CFA develop requires a single-sample 3-color CFA");
    }

    // ---- Rescale step ------------------------------------------------
    // `(v - black_pos) / (white_pos - black_pos)` — the caller has already
    // overridden `whitelevel` to u32::MAX (the same single-scaling contract
    // as the clipped path; the post-stage `rescale_factor` then restores
    // `white - black` normalization).
    let blacks = raw.blacklevel.as_bayer_array();
    let whites = raw.whitelevel.as_bayer_array();
    let denom = std::array::from_fn(|i| (whites[i] - blacks[i]).max(1.0));
    let width = raw.width;
    let mut data = raw.data.as_f32().into_owned();
    correct_blacklevel_cfa_signed(&mut data, width, &blacks, &denom);
    let mosaic = Pix2D::new_with(data, width, raw.height);

    // ---- Demosaic over the active area (same dispatch as Rawler) ------
    let roi = raw.active_area.unwrap_or_else(|| mosaic.rect());
    let mut intermediate = if config.cfa.width == 6 && config.cfa.height == 6 {
        match demosaic_algorithm {
            DemosaicAlgorithm::Quality => Intermediate::ThreeColor(XTransDemosaic::new().demosaic(
                &mosaic,
                &config.cfa,
                &config.colors,
                roi,
            )),
            DemosaicAlgorithm::Speed => Intermediate::ThreeColor(
                XTransSuperpixelDemosaic::new().demosaic(&mosaic, &config.cfa, &config.colors, roi),
            ),
        }
    } else {
        match demosaic_algorithm {
            DemosaicAlgorithm::Quality => Intermediate::ThreeColor(PPGDemosaic::new().demosaic(
                &mosaic,
                &config.cfa,
                &config.colors,
                roi,
            )),
            DemosaicAlgorithm::Speed => {
                Intermediate::ThreeColor(SuperpixelQuarterRes3Channel::new().demosaic(
                    &mosaic,
                    &config.cfa,
                    &config.colors,
                    roi,
                ))
            }
        }
    };

    // ---- CropDefault (same logic as develop_intermediate) -------------
    if let Some(mut crop) = raw.crop_area {
        if let Some(active_area) = raw.active_area {
            crop = crop.intersection(&active_area).adapt(&active_area);
        }
        let original_width = raw.active_area.map(|a| a.d.w).unwrap_or(raw.dim().w);
        if original_width > 0 {
            let scale_factor = intermediate.dim().w as f32 / original_width as f32;
            if (scale_factor - 1.0).abs() > 1e-6 {
                crop.scale(scale_factor);
            }
        }
        if !crop.is_empty()
            && crop.d != intermediate.dim()
            && let Intermediate::ThreeColor(pixels) = intermediate
        {
            intermediate = Intermediate::ThreeColor(pixels.crop(crop));
        }
    }

    Ok(intermediate)
}

fn is_linear_raw_format(raw_image: &RawImage) -> bool {
    matches!(
        raw_image.photometric,
        RawPhotometricInterpretation::LinearRaw
    )
}

#[inline]
/// Standard sRGB EOTF inverse (IEC 61966-2-1): linear segment below 0.04045,
/// `((v + 0.055) / 1.055)^2.4` above. This previously used exponent 3.0, which
/// is not the sRGB transfer curve; the Adobe DNG SDK decode
/// (`dng_function_GammaEncode_sRGB::EvaluateInverse`,
/// `dng_color_space.cpp:34-44`) confirms 2.4.
fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Rawler's camera-matrix calibration without its final negative-channel
/// floor. Use the same Rawler camera-to-sRGB inverse and white balance, but
/// precompose its sRGB->ProPhoto/D50 transform ONCE, before evaluating pixels.
/// Adobe mode therefore enters the XMP profile stages in Linear ProPhoto and
/// retains colors that fall outside the smaller sRGB gamut. Normal editing
/// and Legacy profiles continue to use Rawler's original calibration.
///
/// This is still a Rawler-based camera calibration, not Adobe's proprietary
/// camera-profile or full RAW color pipeline.
struct UnclippedProPhotoCalibration {
    camera_to_prophoto: [[f32; 4]; 3],
    white_balance: [f32; 4],
}

impl UnclippedProPhotoCalibration {
    fn for_raw(raw: &RawImage) -> Option<Self> {
        // The supported path is ordinary 3-color CFA data. Four-component
        // sensors still use Rawler's own Calibrate operation.
        if raw.cpp == 4
            || matches!(
                &raw.photometric,
                RawPhotometricInterpretation::Cfa(config) if config.cfa.unique_colors() == 4
            )
        {
            return None;
        }
        let (_, matrix) = raw
            .color_matrix
            .iter()
            .find(|(illuminant, _)| **illuminant == Illuminant::D65)
            .or_else(|| raw.color_matrix.iter().next())?;
        if matrix.len() != 9 {
            return None;
        }

        let mut xyz_to_camera = [[0.0f32; 3]; 4];
        for channel in 0..3 {
            for axis in 0..3 {
                xyz_to_camera[channel][axis] = matrix[channel * 3 + axis];
            }
        }

        let rgb_to_camera = normalize(multiply(&xyz_to_camera, &SRGB_TO_XYZ_D65));
        let camera_to_srgb_unclipped = pseudo_inverse(rgb_to_camera);
        let camera_to_prophoto = multiply(
            &crate::xmp_profile::adobe_linear_srgb_to_prophoto_matrix(),
            &camera_to_srgb_unclipped,
        );
        if !camera_to_prophoto.iter().flatten().all(|v| v.is_finite()) {
            return None;
        }

        let white_balance = if raw.wb_coeffs[0].is_nan() {
            [1.0; 4]
        } else {
            raw.wb_coeffs
        };
        Some(Self {
            camera_to_prophoto,
            white_balance,
        })
    }

    /// The composite camera→Linear ProPhoto D50 matrix actually applied
    /// (WB folds separately). For diagnostics.
    pub(crate) fn transform(&self) -> [[f32; 4]; 3] {
        self.camera_to_prophoto
    }

    fn apply(&self, pixel: &mut [f32; 3]) {
        let [r, g, b] = *pixel;
        let r = r * self.white_balance[0];
        let g = g * self.white_balance[1];
        let b = b * self.white_balance[2];
        let m = &self.camera_to_prophoto;
        *pixel = [
            m[0][0] * r + m[0][1] * g + m[0][2] * b,
            m[1][0] * r + m[1][1] * g + m[1][2] * b,
            m[2][0] * r + m[2][1] * g + m[2][2] * b,
        ];
    }
}

/// Prepare a calibrated linear RGB pixel for the profile stage.
///
/// Rawler's default `Calibrate` converts camera RGB to linear sRGB/D65
/// and floors negative channels. Where available, Adobe mode instead uses
/// the direct camera->ProPhoto calibration above; the fallback path retains
/// signed, high-range linear RGB until the profile's SDK-defined gamut handling.
/// The original Once-Lab highlight compressor remains unchanged for the
/// normal editor path and the explicit Legacy XMP renderer.
fn prepare_three_color_input(
    mut rgb: [f32; 3],
    rescale_factor: f32,
    safe_highlight_compression: f32,
    clamp_limit: f32,
    undo_srgb_encoding: bool,
    preserve_for_adobe_profile: bool,
) -> [f32; 3] {
    for value in &mut rgb {
        let scaled = *value * rescale_factor;
        *value = if preserve_for_adobe_profile {
            scaled
        } else {
            scaled.max(0.0)
        };
        if undo_srgb_encoding {
            let encoded = if preserve_for_adobe_profile {
                *value
            } else {
                value.clamp(0.0, 1.0)
            };
            *value = srgb_to_linear(encoded);
        }
    }

    if preserve_for_adobe_profile {
        return rgb;
    }

    let [r, g, b] = rgb;
    let max_c = r.max(g).max(b);
    let (final_r, final_g, final_b) = if max_c > 1.0 {
        let min_c = r.min(g).min(b);
        let compression_factor =
            (1.0 - (max_c - 1.0) / (safe_highlight_compression - 1.0)).clamp(0.0, 1.0);
        let compressed_r = min_c + (r - min_c) * compression_factor;
        let compressed_g = min_c + (g - min_c) * compression_factor;
        let compressed_b = min_c + (b - min_c) * compression_factor;
        let compressed_max = compressed_r.max(compressed_g).max(compressed_b);

        if compressed_max > 1e-6 {
            let rescale = max_c / compressed_max;
            (
                compressed_r * rescale,
                compressed_g * rescale,
                compressed_b * rescale,
            )
        } else {
            (max_c, max_c, max_c)
        }
    } else {
        (r, g, b)
    };

    [
        final_r.clamp(0.0, clamp_limit),
        final_g.clamp(0.0, clamp_limit),
        final_b.clamp(0.0, clamp_limit),
    ]
}

#[allow(clippy::too_many_arguments)]
fn develop_internal(
    file_bytes: &[u8],
    fast_demosaic: bool,
    highlight_compression: f32,
    linear_mode: String,
    profile: Option<&crate::xmp_profile::XmpRgbProfile>,
    profile_amount_percent: Option<f32>,
    adobe: Option<&AdobeRawContext<'_>>,
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
    mut diag: Option<&mut RawPipelineDiagnostics>,
) -> Result<(DynamicImage, Orientation)> {
    let check_cancel = || -> Result<()> {
        if let Some((tracker, generation)) = &cancel_token
            && tracker.load(Ordering::SeqCst) != *generation
        {
            return Err(anyhow!("Load cancelled"));
        }
        Ok(())
    };

    check_cancel()?;

    let source = RawSource::new_from_slice(file_bytes);
    let decoder = rawler::get_decoder(&source)?;

    check_cancel()?;
    let mut raw_image: RawImage = decoder.raw_image(&source, &RawDecodeParams::default(), false)?;

    let metadata = decoder.raw_metadata(&source, &RawDecodeParams::default())?;
    let orientation = metadata
        .exif
        .orientation
        .map(Orientation::from_u16)
        .unwrap_or(Orientation::Normal);

    let is_linear_format = is_linear_raw_format(&raw_image);

    let (apply_ungamma, apply_calibration) = match linear_mode.as_str() {
        "gamma" => (true, true),
        "skip_calib" => (false, false),
        "gamma_skip_calib" => (true, false),
        _ => (false, true),
    };

    // Resolve the render mode once. The highlight preprocessing and the
    // profile evaluator must use the same mode even if the environment is
    // changed while a RAW file is being developed.
    let profile_render_mode = profile.map(|_| crate::xmp_profile::profile_render_mode());
    // The Adobe RAW stage runs whenever the Adobe evaluator will run, and for
    // the profile-less baseline when the comparison mode is enabled.
    let preserve_for_adobe_profile = match profile {
        Some(_) => matches!(
            profile_render_mode,
            Some(crate::xmp_profile::ProfileRenderMode::AdobeSdk)
        ),
        None => adobe.is_some(),
    };

    let original_white_level = raw_image
        .whitelevel
        .0
        .first()
        .cloned()
        .unwrap_or(u16::MAX as u32) as f32;
    let original_black_level = raw_image
        .blacklevel
        .levels
        .first()
        .map(|r| r.as_f32())
        .unwrap_or(0.0);
    let denominator = (original_white_level - original_black_level).max(1.0);
    let rescale_factor = (u32::MAX as f32 - original_black_level) / denominator;

    // Resolve any RAW-specific white-balance override before preparing
    // either Rawler's or the Adobe-mode unclipped camera calibration.
    raw_image.wb_coeffs =
        crate::multi_exposure::neutralize_wb_if_multiexposure(raw_image.wb_coeffs, file_bytes);

    // ---- sub-black signal preservation (Adobe comparison path only) ----
    // Rawler's `correct_blacklevel_cfa` hard-clips `raw - black` at 0 before
    // demosaic; the DNG SDK keeps sub-black values signed through stage3
    // (no `forceClipBlackLevel`). For the 3-color CFA case we run a
    // signed-preserving variant of Rawler's own Rescale/Demosaic/Crop
    // steps — see `develop_intermediate_signed_cfa` — so the demosaic
    // input is byte-for-byte what the SDK path would produce (signed
    // values, identical magnitude domain; no shifted-domain artifacts).
    // Only the 3-color CFA path is eligible (same as the unclipped
    // calibration); 4-color sensors and non-CFA inputs keep Rawler's path.
    let signed_cfa_develop = preserve_for_adobe_profile
        && !is_linear_format
        && raw_image.cpp == 1
        && matches!(
            &raw_image.photometric,
            RawPhotometricInterpretation::Cfa(config)
                if config.cfa.unique_colors() != 4 && config.cfa.is_rgb()
        );

    // Phase-1 diagnostics: record the decoder's true levels, per-CFA mosaic
    // stats and the linearization proof BEFORE the whitelevel override
    // mutates them (all values recorded are the real executed parameters).
    if let Some(d) = diag.as_mut() {
        d.record_raw_image(&raw_image, rescale_factor as f64, signed_cfa_develop);
    }

    for level in raw_image.whitelevel.0.iter_mut() {
        *level = u32::MAX;
    }

    // ------------------------------------------------- DCP camera profile
    // In Adobe comparison mode a user-supplied DCP takes over the camera
    // transform, HueSatMap/LookTable and ProfileToneCurve stages. A missing,
    // unreadable, mismatched or unsupported profile falls back to Rawler's
    // color-matrix calibration with an explicit report entry — never a
    // silent "Adobe Standard" claim.
    let mut report_warnings: Vec<String> = Vec::new();
    let dcp_render = if preserve_for_adobe_profile && apply_calibration {
        adobe
            .and_then(|ctx| ctx.camera_profile_path)
            .and_then(|path| match load_dcp_from_path(path) {
                Err(e) => {
                    report_warnings.push(format!("camera profile unavailable: {e}"));
                    None
                }
                Ok(dcp) => {
                    report_warnings.extend(dcp.warnings.iter().cloned());
                    report_warnings
                        .extend(dcp.unsupported.iter().map(|f| format!("unsupported: {f}")));
                    let match_kind =
                        dcp.camera_match(&raw_image.clean_make, &raw_image.clean_model);
                    if match_kind == CameraMatch::Mismatch {
                        report_warnings.push(format!(
                            "camera profile '{}' targets '{}' but the file is '{} {}'; \
                             using the Rawler color-matrix fallback",
                            dcp.display_name(),
                            dcp.unique_camera_model.as_deref().unwrap_or("?"),
                            raw_image.clean_make,
                            raw_image.clean_model,
                        ));
                        return None;
                    }
                    match DcpRender::new(
                        dcp,
                        raw_image.wb_coeffs,
                        &raw_image.clean_make,
                        &raw_image.clean_model,
                    ) {
                        Ok(render) => Some(render),
                        Err(e) => {
                            report_warnings.push(format!(
                                "camera profile render setup failed, using Rawler fallback: {e}"
                            ));
                            None
                        }
                    }
                }
            })
    } else {
        None
    };

    // Only Adobe mode can bypass Rawler's sRGB negative-channel clip.
    // For cameras without a standard 3-component color matrix, or when
    // calibration is intentionally disabled, keep Rawler's existing path.
    // When a DCP render resolved, it replaces this calibration entirely.
    let unclipped_calibration = if preserve_for_adobe_profile
        && !is_linear_format
        && apply_calibration
        && dcp_render.is_none()
    {
        UnclippedProPhotoCalibration::for_raw(&raw_image)
    } else {
        None
    };

    let mut developer = RawDevelop::default();

    if is_linear_format {
        developer.steps.retain(|&step| {
            step != ProcessingStep::SRgb
                && step != ProcessingStep::Demosaic
                && (apply_calibration || step != ProcessingStep::Calibrate)
        });
    } else if fast_demosaic {
        developer.demosaic_algorithm = DemosaicAlgorithm::Speed;
        developer.steps.retain(|&step| step != ProcessingStep::SRgb);
    } else {
        developer.steps.retain(|&step| step != ProcessingStep::SRgb);
    }

    if unclipped_calibration.is_some() || dcp_render.is_some() {
        // Rawler's Calibrate step also applies its camera WB multipliers; the
        // DCP path white-balances inside CameraToPCS, the fallback inside its
        // own composite — either way Rawler's step must not run a second time.
        developer
            .steps
            .retain(|step| *step != ProcessingStep::Calibrate);
    }

    check_cancel()?;
    let mut developed_intermediate = if signed_cfa_develop {
        // Signed-preserving develop (see above): same Rescale/Demosaic/
        // Crop steps as `develop_intermediate`, without the pre-demosaic
        // black clip. Sub-black values remain signed through PPG.
        develop_intermediate_signed_cfa(&raw_image, developer.demosaic_algorithm)?
    } else {
        developer.develop_intermediate(&raw_image)?
    };

    let camera_make = raw_image.clean_make.clone();
    let camera_model = raw_image.clean_model.clone();
    let raw_wb_coeffs = raw_image.wb_coeffs;
    drop(raw_image);

    let safe_highlight_compression = highlight_compression.max(1.01);

    let clamp_limit = if fast_demosaic {
        1.0
    } else {
        safe_highlight_compression
    };

    check_cancel()?;

    match &mut developed_intermediate {
        Intermediate::Monochrome(pixels) => {
            pixels.data.iter_mut().for_each(|p| {
                let mut linear_val = *p * rescale_factor;
                if is_linear_format && apply_ungamma {
                    linear_val = srgb_to_linear(linear_val.clamp(0.0, 1.0));
                }
                *p = linear_val.clamp(0.0, clamp_limit);
            });
        }
        Intermediate::ThreeColor(pixels) => {
            pixels.data.iter_mut().for_each(|p| {
                let out = prepare_three_color_input(
                    *p,
                    rescale_factor,
                    safe_highlight_compression,
                    clamp_limit,
                    is_linear_format && apply_ungamma,
                    preserve_for_adobe_profile,
                );
                *p = out;
            });
        }
        Intermediate::FourColor(pixels) => {
            pixels.data.iter_mut().for_each(|p| {
                p.iter_mut().for_each(|c| {
                    let mut linear_val = *c * rescale_factor;
                    if is_linear_format && apply_ungamma {
                        linear_val = srgb_to_linear(linear_val.clamp(0.0, 1.0));
                    }
                    *c = linear_val.clamp(0.0, clamp_limit);
                });
            });
        }
    }

    // Stage boundary: post-Rescale/demosaic/crop + post-rescale_factor.
    // At this point the buffer is camera-space RGB, pre-WB (Rawler's
    // Calibrate step is removed in the Adobe path; WB folds into the
    // camera transform instead).
    if let Some(d) = diag.as_mut() {
        crate::raw_diagnostics::record_intermediate(
            d,
            "post-linearization-demosaic-rescale",
            "camera-space linear RGB (pre-WB)",
            &developed_intermediate,
        );
    }

    // Whether the user opted into the SDK's documented defaults for the
    // profile-less baseline (acr3 tone curve + Shadows=5 ramp toe).
    // Developer-only experiment selector: which SDK documented defaults
    // run when the camera profile authored none. `Identity` is the
    // established baseline. The ramp (`fShadows = 5`, `ShadowScale = 1`,
    // EV 0 → ~0.5% quadratic black toe) runs only on the Rawler-fallback
    // path in toe modes; a real DCP carries its own ramp data instead.
    let baseline_mode = adobe.map(|ctx| ctx.baseline_mode).unwrap_or_default();
    let sdk_default_ramp = ExposureRamp::new(0.0, 5.0, 1.0);
    let fallback_ramp_active =
        baseline_mode.shadow_toe() && dcp_render.is_none() && sdk_default_ramp.active;

    // ------------------------------------------------- camera transform
    // Runs AFTER normalization: the DCP stage clips at the camera-white point
    // and pins to [0,1] (`RefBaselineABCtoRGB`), which does not commute with
    // the rescale, so ordering matters. The Rawler fallback calibration is a
    // pure matrix + WB and commutes, but runs here for one stage boundary.
    let mut profile_input_is_prophoto = false;
    if let (Some(render), Intermediate::ThreeColor(pixels)) =
        (&dcp_render, &mut developed_intermediate)
    {
        // DCP camera stage followed by profile HueSatMap -> exposure ramp ->
        // ProfileLookTable, in SDK order on linear ProPhoto D50 values.
        if let Some(d) = diag.as_mut() {
            for pixel in &mut pixels.data {
                render.apply_camera(pixel);
            }
            let (w, h) = (pixels.dim().w as u32, pixels.dim().h as u32);
            d.record_three_color(
                "post-dcp-camera-transform",
                "linear ProPhoto D50 (SetWhiteXY / ForwardMatrix invariate)",
                &pixels.data,
                w,
                h,
            );
            for pixel in &mut pixels.data {
                render.apply_scene(pixel);
            }
            d.record_three_color(
                "post-dng-scene-stage",
                "linear ProPhoto D50 (HueSatMap -> exposure ramp -> LookTable)",
                &pixels.data,
                w,
                h,
            );
        } else {
            for pixel in &mut pixels.data {
                render.apply_camera(pixel);
                render.apply_scene(pixel);
            }
        }
        profile_input_is_prophoto = true;
    } else if let (Some(calibration), Intermediate::ThreeColor(pixels)) =
        (&unclipped_calibration, &mut developed_intermediate)
    {
        for pixel in &mut pixels.data {
            calibration.apply(pixel);
            if fallback_ramp_active {
                for channel in pixel.iter_mut() {
                    *channel = sdk_default_ramp.evaluate(*channel);
                }
            }
        }
        if let Some(d) = diag.as_mut() {
            d.fallback_transform = Some(serde_json::json!({
                "cameraToProPhotoD50": calibration.transform(),
                "whiteBalance": raw_wb_coeffs,
            }));
            let (w, h) = (pixels.dim().w as u32, pixels.dim().h as u32);
            d.record_three_color(
                "post-camera-transform-rawler-fallback",
                "linear ProPhoto D50 (WB multiply + inverted color matrix, unclipped)",
                &pixels.data,
                w,
                h,
            );
        }
        profile_input_is_prophoto = true;
    }

    // Baseline tone stage: `dng_render` runs LookTable -> ProfileToneCurve ->
    // RGBTables. The profile-authored curve takes precedence; when absent the
    // SDK substitutes `dng_tone_curve_acr3_default` — but only when the user
    // has explicitly opted in (it is NOT Lightroom PV2012, see the research
    // note in `raw_diagnostics`). Otherwise the established identity fallback.
    let acr3_default_curve = SampledToneCurve::acr3_default();
    let tone_curve: Option<&dyn BaselineToneCurve> = dcp_render
        .as_ref()
        .and_then(|render| render.tone_curve())
        .or_else(|| {
            baseline_mode
                .acr3_tone()
                .then_some(&acr3_default_curve as &dyn BaselineToneCurve)
        });
    let tone_curve_label = tone_curve.map(|c| c.label()).unwrap_or("identity");

    if let Some(d) = diag.as_mut() {
        let ramp_label = if let Some(r) = &dcp_render {
            r.report.exposure_stage.clone()
        } else if fallback_ramp_active {
            sdk_default_ramp.label.clone()
        } else {
            "identity (no DCP exposure ramp)".to_string()
        };
        let beo = dcp_render
            .as_ref()
            .and_then(|r| r.report.baseline_exposure_offset_ev);
        let mode_label = match baseline_mode {
            DngBaselineMode::Identity => "identity",
            DngBaselineMode::Acr3Tone => "acr3",
            DngBaselineMode::ShadowToe => "shadow-toe",
            DngBaselineMode::Acr3ToneWithShadowToe => "acr3+toe",
        };
        d.record_tone(&ramp_label, beo, mode_label, tone_curve_label, false);
    }

    if let Some(profile) = profile {
        match &mut developed_intermediate {
            Intermediate::ThreeColor(pixels) => {
                let mode = profile_render_mode.expect("profile implies resolved render mode");
                let applied =
                    crate::xmp_profile::apply_profile_to_three_color_pixels_with_mode_space_and_tone(
                        &mut pixels.data,
                        profile,
                        profile_amount_percent,
                        mode,
                        profile_input_is_prophoto,
                        tone_curve,
                    );

                applied.map_err(|error| anyhow!(error))?;
                if let Some(d) = diag.as_mut() {
                    let (w, h) = (pixels.dim().w as u32, pixels.dim().h as u32);
                    d.record_three_color(
                        "post-creative-profile",
                        "linear sRGB D65 (post tone curve -> Creative RGBTable)",
                        &pixels.data,
                        w,
                        h,
                    );
                }
            }
            _ => {
                return Err(anyhow!("XMP RGB profile requires ThreeColor intermediate"));
            }
        }
    } else if profile_input_is_prophoto {
        // Baseline tone stage for the profile-less DCP render: the SDK runs
        // LookTable -> ProfileToneCurve -> RGBTables, and with no creative
        // RGBTable the tone curve still applies before output conversion.
        if let Some(curve) = tone_curve
            && let Intermediate::ThreeColor(pixels) = &mut developed_intermediate
        {
            // `RefBaselineRGBTone` — ratio-preserving, as the SDK applies
            // `fToneCurve` to the linear ProPhoto pixels.
            for p in &mut pixels.data {
                curve.apply_rgb(p);
            }
            if let Some(d) = diag.as_mut() {
                let (w, h) = (pixels.dim().w as u32, pixels.dim().h as u32);
                d.record_three_color(
                    "post-baseline-tone",
                    &format!("linear ProPhoto D50 (tone: {tone_curve_label})"),
                    &pixels.data,
                    w,
                    h,
                );
            }
        }
        // Profile-less Adobe comparison baseline: the pixels were calibrated
        // straight into Linear ProPhoto/D50 (signed out-of-sRGB-gamut values
        // and highlight excursions retained). The working buffer is linear
        // sRGB/D65, so hop there now WITHOUT clamping - the output stage's
        // encoding owns the final pin, matching how the profiled render
        // keeps its headroom until the same boundary.
        if let Intermediate::ThreeColor(pixels) = &mut developed_intermediate {
            let prophoto_to_srgb = crate::xmp_profile::adobe_linear_prophoto_to_srgb_matrix();
            for p in &mut pixels.data {
                *p = [
                    prophoto_to_srgb[0][0] * p[0]
                        + prophoto_to_srgb[0][1] * p[1]
                        + prophoto_to_srgb[0][2] * p[2],
                    prophoto_to_srgb[1][0] * p[0]
                        + prophoto_to_srgb[1][1] * p[1]
                        + prophoto_to_srgb[1][2] * p[2],
                    prophoto_to_srgb[2][0] * p[0]
                        + prophoto_to_srgb[2][1] * p[1]
                        + prophoto_to_srgb[2][2] * p[2],
                ];
            }
        }
        if let Some(d) = diag.as_mut() {
            crate::raw_diagnostics::record_intermediate(
                d,
                "final-linear-srgb-working-buffer",
                "linear sRGB D65 (pre-GPU output stage)",
                &developed_intermediate,
            );
        }
    } else if let Some(d) = diag.as_mut() {
        crate::raw_diagnostics::record_intermediate(
            d,
            "final-working-buffer",
            "linear sRGB D65 (legacy path, post-calibration)",
            &developed_intermediate,
        );
    }

    // ------------------------------------------------- diagnostic report
    if let Some(ctx) = adobe {
        let camera_profile = match &dcp_render {
            Some(render) => CameraProfileChoice::Dcp {
                name: render.report.profile_name.clone(),
                path: ctx
                    .camera_profile_path
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                model_match: render.report.match_kind.clone(),
            },
            None => CameraProfileChoice::RawlerFallback {
                reason: if ctx.camera_profile_path.is_none() {
                    "no DCP camera profile selected; Rawler color-matrix calibration (not Adobe Standard parity)"
                        .to_string()
                } else {
                    "DCP unavailable or unrenderable; see warnings".to_string()
                },
            },
        };

        let creative_profile = profile.map(|p| CreativeProfileReport {
            name: p.name.clone(),
            uuid: p.uuid.clone(),
            rgb_table_id: p.rgb_table_id.clone(),
            effective_amount: crate::xmp_profile::effective_profile_amount(
                p,
                profile_amount_percent,
            )
            .unwrap_or(1.0),
            gamut_policy: p
                .table
                .as_ref()
                .map(|t| {
                    if t.gamut == 0 {
                        "clip".to_string()
                    } else {
                        format!("unknown({})", t.gamut)
                    }
                })
                .unwrap_or_else(|| "n/a".to_string()),
        });

        record_adobe_report(AdobePipelineReport {
            source_path: ctx.source_path.unwrap_or("<unknown>").to_string(),
            camera_make,
            camera_model,
            raw_wb_coeffs,
            camera_profile,
            render: dcp_render.map(|r| r.report.clone()),
            creative_profile,
            output_stage: format!(
                "linear->sRGB encode (tonemapper mode 2, Adobe comparison); baseline tone: {tone_curve_label}"
            ),
            lens_correction: None,
            warnings: report_warnings,
        });
    }

    let (width, height) = {
        let dim = developed_intermediate.dim();
        (dim.w as u32, dim.h as u32)
    };

    check_cancel()?;

    let dynamic_image = match developed_intermediate {
        Intermediate::ThreeColor(pixels) => {
            let buffer = ImageBuffer::<Rgba<f32>, _>::from_fn(width, height, |x, y| {
                let p = pixels.data[(y * width + x) as usize];
                Rgba([p[0], p[1], p[2], 1.0])
            });
            DynamicImage::ImageRgba32F(buffer)
        }
        Intermediate::Monochrome(pixels) => {
            let buffer = ImageBuffer::<Rgba<f32>, _>::from_fn(width, height, |x, y| {
                let p = pixels.data[(y * width + x) as usize];
                Rgba([p, p, p, 1.0])
            });
            DynamicImage::ImageRgba32F(buffer)
        }
        _ => {
            return Err(anyhow!("Unsupported intermediate format for conversion"));
        }
    };

    Ok((dynamic_image, orientation))
}

pub fn get_fast_demosaic_scale_factor(
    file_bytes: &[u8],
    decoded_width: u32,
    decoded_height: u32,
) -> f32 {
    let source = RawSource::new_from_slice(file_bytes);
    if let Ok(decoder) = rawler::get_decoder(&source)
        && let Ok(raw_img) = decoder.raw_image(&source, &RawDecodeParams::default(), true)
    {
        let max_orig = (raw_img.width as f32).max(raw_img.height as f32);
        let max_comp = (decoded_width as f32).max(decoded_height as f32);
        if max_orig > 0.0 {
            let ratio = max_comp / max_orig;
            if ratio > 0.1 && ratio < 0.35 {
                return 0.25;
            } else if (0.35..0.75).contains(&ratio) {
                return 0.5;
            }
        }
    }
    1.0
}

#[cfg(test)]
mod adobe_profile_upstream_tests {
    use super::*;

    #[test]
    fn adobe_profile_preserves_highlight_channel_ratios_before_lut() {
        let source = [4.0, 1.25, 0.5];
        let adobe = prepare_three_color_input(source, 1.0, 3.0, 3.0, false, true);
        let legacy = prepare_three_color_input(source, 1.0, 3.0, 3.0, false, false);

        assert_eq!(
            adobe, source,
            "Adobe profile must receive uncompressed linear highlights"
        );
        assert_ne!(
            adobe, legacy,
            "Legacy path must retain its existing compressor"
        );
        assert!(legacy.iter().all(|v| *v <= 3.0));
    }

    #[test]
    fn adobe_profile_ungamma_preserves_values_above_one() {
        let source = [2.0, 0.5, 0.0];
        let adobe = prepare_three_color_input(source, 1.0, 3.0, 3.0, true, true);
        let legacy = prepare_three_color_input(source, 1.0, 3.0, 3.0, true, false);

        assert!(
            adobe[0] > 1.0,
            "do not clip encoded highlights before Adobe stage"
        );
        assert!(legacy[0] <= 1.0, "Legacy encoded input is still pinned");
        assert!((adobe[1] - srgb_to_linear(0.5)).abs() < 1e-6);
        assert_eq!(adobe[2], 0.0);
    }

    #[test]
    fn adobe_profile_keeps_negative_srgb_chroma_for_prophoto_conversion() {
        let source = [-0.15, 0.4, 1.4];
        let adobe = prepare_three_color_input(source, 1.0, 3.0, 3.0, false, true);
        let legacy = prepare_three_color_input(source, 1.0, 3.0, 3.0, false, false);
        assert_eq!(adobe, source);
        assert_eq!(legacy[0], 0.0);
    }

    #[test]
    fn calibrated_prophoto_values_are_not_floored_in_adobe_mode() {
        let calibration = UnclippedProPhotoCalibration {
            camera_to_prophoto: [
                [1.0, -1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
            ],
            white_balance: [1.0; 4],
        };
        let mut pixel = [0.2, 0.9, 0.5];
        calibration.apply(&mut pixel);
        assert!(pixel[0] < 0.0);
        assert_eq!(pixel[1], 0.9);
    }

    #[test]
    fn editor_without_adobe_profile_uses_legacy_highlight_behavior() {
        let source = [0.18, 0.5, 0.95];
        let processed = prepare_three_color_input(source, 1.0, 3.0, 3.0, false, false);
        assert_eq!(processed, source);
    }
}

#[cfg(test)]
mod adobe_dcp_end_to_end {
    use super::*;
    use image::GenericImageView;
    use std::path::{Path, PathBuf};

    /// Owner/local fixture gate — set:
    ///   ONCELAB_RAW_FIXTURE=/path/to/file.cr3
    ///   ONCELAB_DCP_FIXTURE=/path/to/camera.dcp
    ///   ONCELAB_XMP_FIXTURE=/path/to/profile.xmp   (optional)
    /// Nothing is committed; the tests are no-ops without the variables.
    fn fixtures() -> Option<(PathBuf, PathBuf)> {
        let raw = std::env::var("ONCELAB_RAW_FIXTURE").ok()?;
        let dcp = std::env::var("ONCELAB_DCP_FIXTURE").ok()?;
        Some((PathBuf::from(raw), PathBuf::from(dcp)))
    }

    /// With the owner's Canon EOS R5 + its licensed Adobe Standard DCP:
    /// the DCP path must engage (not the Rawler fallback), produce a valid
    /// image, and differ materially from the matrix fallback.
    #[test]
    fn dcp_render_engages_and_differs_from_fallback() {
        let Some((raw_path, dcp_path)) = fixtures() else {
            eprintln!("SKIPPED: ONCELAB_RAW_FIXTURE/ONCELAB_DCP_FIXTURE not set");
            return;
        };
        let bytes = std::fs::read(&raw_path).expect("read RAW fixture");

        let with_dcp = develop_raw_image_with_profile(
            &bytes,
            false,
            2.5,
            "auto".to_string(),
            None,
            None,
            Some(&AdobeRawContext {
                camera_profile_path: Some(dcp_path.as_path()),
                source_path: Some(raw_path.to_str().unwrap()),
                baseline_mode: DngBaselineMode::Identity,
            }),
            None,
        )
        .expect("DCP develop");

        // The recorded report must name the DCP, not the fallback — checked
        // BEFORE the fallback develop overwrites the same source key.
        let report = crate::camera_profile::get_adobe_pipeline_report(Some(
            raw_path.to_str().unwrap().to_string(),
        ))
        .expect("report")
        .expect("a report must be recorded");
        eprintln!("report: {report:#?}");
        match &report.camera_profile {
            crate::camera_profile::CameraProfileChoice::Dcp { path, .. } => {
                assert!(path.ends_with(".dcp"), "report path {path}");
            }
            other => panic!("expected DCP profile choice, got {other:?}"),
        }

        let fallback = develop_raw_image_with_profile(
            &bytes,
            false,
            2.5,
            "auto".to_string(),
            None,
            None,
            Some(&AdobeRawContext {
                camera_profile_path: None,
                source_path: Some(raw_path.to_str().unwrap()),
                baseline_mode: DngBaselineMode::Identity,
            }),
            None,
        )
        .expect("fallback develop");

        assert_eq!(with_dcp.dimensions(), fallback.dimensions());

        // The camera profile stage must visibly change the render — the
        // Adobe Standard matrices + HueSatMap + LookTable vs Rawler's plain
        // color-matrix calibration.
        let a = with_dcp.to_rgb16();
        let b = fallback.to_rgb16();
        let diff: u64 = a
            .as_raw()
            .iter()
            .zip(b.as_raw().iter())
            .map(|(x, y)| (*x as i64 - *y as i64).unsigned_abs())
            .sum();
        let mae = diff as f64 / a.as_raw().len() as f64;
        eprintln!("DCP-vs-fallback MAE(u16): {mae:.1}");
        assert!(mae > 0.5, "DCP render indistinguishable from fallback");
    }

    /// Full fidelity path: DCP camera profile + creative XMP RGBTable.
    /// The report must record both stages, and the profile must be applied
    /// at its authored amount after the camera-profile stages.
    #[test]
    fn dcp_and_creative_profile_compose_end_to_end() {
        let (Ok(raw_path), Ok(dcp_path), Ok(xmp_path)) = (
            std::env::var("ONCELAB_RAW_FIXTURE"),
            std::env::var("ONCELAB_DCP_FIXTURE"),
            std::env::var("ONCELAB_XMP_FIXTURE"),
        ) else {
            eprintln!("SKIPPED: ONCELAB_RAW/DCP/XMP_FIXTURE not all set");
            return;
        };
        let bytes = std::fs::read(&raw_path).expect("read RAW fixture");
        let profile = crate::xmp_profile::load_xmp_rgb_profile_from_path(Path::new(&xmp_path))
            .expect("load XMP profile");

        let authored = profile.rgb_table_amount;
        eprintln!(
            "creative profile: name={:?} uuid={:?} rgbTableId={:?} amount={authored:?}",
            profile.name, profile.uuid, profile.rgb_table_id,
        );

        let developed = develop_raw_image_with_profile(
            &bytes,
            false,
            2.5,
            "auto".to_string(),
            Some(&profile),
            Some(100.0), // slider 100% -> authored amount (Au: 0.5)
            Some(&AdobeRawContext {
                camera_profile_path: Some(Path::new(&dcp_path)),
                source_path: Some(raw_path.as_str()),
                baseline_mode: DngBaselineMode::Identity,
            }),
            None,
        )
        .expect("DCP + creative profile develop");

        let report = crate::camera_profile::get_adobe_pipeline_report(Some(raw_path))
            .expect("report")
            .expect("a report must be recorded");
        eprintln!("report: {report:#?}");

        match &report.camera_profile {
            crate::camera_profile::CameraProfileChoice::Dcp { .. } => {}
            other => panic!("expected DCP profile choice, got {other:?}"),
        }
        let creative = report
            .creative_profile
            .as_ref()
            .expect("creative profile must be recorded");
        eprintln!("creative stage: {creative:?}");

        // Sanity: a real render, not a degenerate image.
        let rgb = developed.to_rgb16();
        let total = rgb.as_raw().len() as u64;
        let sum: u64 = rgb.as_raw().iter().map(|v| *v as u64).sum();
        let mean = sum as f64 / total as f64;
        eprintln!("mean channel u16: {mean:.0}");
        assert!(mean > 200.0, "image implausibly dark — pipeline error");
    }
}

#[cfg(test)]
mod round3_probe {
    use super::*;

    /// Phase-1 instrumentation end-to-end: runs the real Adobe comparison
    /// pipeline on the owner CR3 with the diagnostics sink attached, writes
    /// the JSON document (and optional stage TIFFs when
    /// `ONCELAB_RAW_DIAG_DIR` is set), and asserts the executed linearization
    /// is the documented single-subtraction path.
    #[test]
    fn cr3_baseline_diagnostics_capture_real_stages() {
        let Ok(raw_path) = std::env::var("ONCELAB_RAW_FIXTURE") else {
            eprintln!("SKIPPED: ONCELAB_RAW_FIXTURE not set");
            return;
        };
        let bytes = std::fs::read(&raw_path).expect("read RAW fixture");

        for baseline_mode in [
            DngBaselineMode::Identity,
            DngBaselineMode::Acr3Tone,
            DngBaselineMode::ShadowToe,
            DngBaselineMode::Acr3ToneWithShadowToe,
        ] {
            let ctx = AdobeRawContext {
                camera_profile_path: None,
                source_path: Some(raw_path.as_str()),
                baseline_mode,
            };
            let (_image, diag) = develop_raw_image_diagnostics(
                &bytes,
                false,
                2.5,
                "auto".to_string(),
                Some(&ctx),
                None,
            )
            .expect("diagnostic develop");

            let prov = diag.provenance.as_ref().expect("provenance recorded");
            assert_eq!(prov.camera_model, "EOS R5");
            assert_eq!(prov.black_levels.len(), 4);
            assert!(prov.wb_coeffs[0] > 1.0, "as-shot R gain present");

            let lin = diag.linearization.as_ref().expect("linearization proof");
            assert!(lin.single_black_subtraction);
            assert!(
                lin.max_scale_deviation < 1e-7,
                "linearization scale deviation {} exceeds float tolerance",
                lin.max_scale_deviation
            );

            assert!(
                diag.stages
                    .iter()
                    .any(|s| s.name == "post-linearization-demosaic-rescale"),
                "demosaic stage must be sampled"
            );
            let tone_label = diag
                .exposure_tone
                .as_ref()
                .expect("tone report")
                .tone_curve
                .clone();
            match baseline_mode {
                DngBaselineMode::Identity | DngBaselineMode::ShadowToe => {
                    assert_eq!(tone_label, "identity")
                }
                DngBaselineMode::Acr3Tone | DngBaselineMode::Acr3ToneWithShadowToe => {
                    assert!(tone_label.contains("acr3_default"))
                }
            }
            // The toe mode must run the SDK ramp on the fallback path.
            let ramp_label = &diag.exposure_tone.as_ref().unwrap().exposure_ramp;
            match baseline_mode {
                DngBaselineMode::Identity | DngBaselineMode::Acr3Tone => {
                    assert!(ramp_label.contains("identity"))
                }
                DngBaselineMode::ShadowToe | DngBaselineMode::Acr3ToneWithShadowToe => {
                    assert!(ramp_label.contains("shadows=5"))
                }
            }

            let json = serde_json::to_string_pretty(&diag).expect("serialize diagnostics");
            let out_dir = std::env::var("ONCELAB_RAW_DIAG_DIR")
                .unwrap_or_else(|_| std::env::temp_dir().display().to_string());
            std::fs::create_dir_all(&out_dir).unwrap();
            let name = format!(
                "raw-baseline-diagnostics-{}.json",
                match baseline_mode {
                    DngBaselineMode::Identity => "identity",
                    DngBaselineMode::Acr3Tone => "acr3tone",
                    DngBaselineMode::ShadowToe => "shadowtoe",
                    DngBaselineMode::Acr3ToneWithShadowToe => "acr3tone+shadowtoe",
                }
            );
            std::fs::write(format!("{out_dir}/{name}"), &json).unwrap();
            eprintln!("wrote {out_dir}/{name} ({} bytes)", json.len());
        }
    }

    /// The signed normalization keeps sub-black values: `(v - b)/d` for
    /// `v < b` is negative, where Rawler's `clip()` would emit 0. Per-plane
    /// denominators and CFA-position indexing are verified against the
    /// decoder's black levels.
    #[test]
    fn signed_normalize_preserves_sub_black_values() {
        let blacks = [511.0f32, 505.0, 514.0, 512.0];
        let whites = [u32::MAX as f32; 4];
        let denom: [f32; 4] = std::array::from_fn(|i| whites[i] - blacks[i]);
        let width = 4usize;
        // Positions: idx (y,x) -> (y%2)*2+(x%2).
        let mut data = vec![
            493.0f32, 1000.0, 480.0, 512.0, // y=0: pos 0,1,0,1
            14008.0, 0.0, 600.0, 511.0, // y=1: pos 2,3,2,3
        ];
        correct_blacklevel_cfa_signed(&mut data, width, &blacks, &denom);

        // Below-black samples stay negative (Rawler's clip would zero them).
        assert!((data[0] - (493.0 - 511.0) / denom[0]).abs() < 1e-9);
        assert!(data[0] < 0.0);
        assert!(data[2] < 0.0);
        // Above-black samples match the standard formula exactly.
        assert!((data[1] - (1000.0 - 505.0) / denom[1]).abs() < 1e-9);
        assert!((data[4] - (14008.0 - 514.0) / denom[2]).abs() < 1e-9);
        assert!((data[7] - (511.0 - 512.0) / denom[3]).abs() < 1e-9);
    }

    /// End-to-end: the signed linearization feeds real, un-clipped
    /// negatives into Rawler's actual PPG demosaic. Compared against the
    /// clipped variant of the same mosaic, differences are nonzero ONLY in
    /// the stencil neighborhood of below-black samples — everywhere else
    /// the outputs are identical (no perturbation of unaffected pixels).
    /// Reports the measured max absolute difference rather than claiming
    /// a bound.
    #[test]
    fn signed_develop_propagates_sub_black_through_ppg() {
        use rawler::cfa::{CFA, PlaneColor};
        use rawler::imgop::sensor::bayer::{Demosaic, ppg::PPGDemosaic};
        use rawler::imgop::{Dim2, Point, Rect};
        use rawler::pixarray::Pix2D;

        let (w, h) = (16usize, 16usize);
        // Unequal per-position blacks (R, G1, G2, B).
        let blacks = [511.0f32, 505.0, 514.0, 512.0];
        let whites = [u32::MAX as f32; 4];
        let denom: [f32; 4] = std::array::from_fn(|i| whites[i] - blacks[i]);

        // Scene with structure + sparse below-black samples + near-tie
        // gradients (alternating columns make opposing gradient
        // magnitudes compete).
        let raw_v = |y: usize, x: usize| -> f32 {
            let base = 600.0 + x as f32 * 30.0 + y as f32 * 20.0;
            let texture = (((x * 7 + y * 13) % 11) as f32 - 5.0) * 8.0;
            let mut v = base + texture;
            if y.is_multiple_of(2) && x.is_multiple_of(2) && (x + y).is_multiple_of(7) {
                v = 480.0 + ((x * 3 + y) % 4) as f32; // below R-plane black
            }
            if x % 4 == 2 {
                v += (y as f32 % 3.0) * 4.0; // near-tie structure
            }
            v.clamp(0.0, 5000.0)
        };

        let mut signed_data: Vec<f32> = (0..h)
            .flat_map(|y| (0..w).map(move |x| raw_v(y, x)))
            .collect();
        correct_blacklevel_cfa_signed(&mut signed_data, w, &blacks, &denom);
        assert!(
            signed_data.iter().any(|&v| v < 0.0),
            "fixture must contain sub-black samples"
        );
        // Rawler's clip is `clip(v - b)/d` == `max(0, (v-b)/d)` for d>0.
        let clipped_data: Vec<f32> = signed_data.iter().map(|v| v.max(0.0)).collect();

        let cfa = CFA::new("RGGB");
        let colors = PlaneColor::new("RGB");
        let roi = Rect::new(Point::new(0, 0), Dim2::new(w, h));
        let dem = PPGDemosaic::new();
        let signed = dem.demosaic(&Pix2D::new_with(signed_data, w, h), &cfa, &colors, roi);
        let clipped = dem.demosaic(&Pix2D::new_with(clipped_data, w, h), &cfa, &colors, roi);

        let mut max_diff = 0.0f32;
        let mut diff_px = 0usize;
        let mut min_signed = f32::MAX;
        for i in 0..signed.data.len() {
            for c in 0..3 {
                min_signed = min_signed.min(signed.data[i][c]);
                if (signed.data[i][c] - clipped.data[i][c]).abs() > 1e-9 {
                    diff_px += 1;
                }
                max_diff = max_diff.max((signed.data[i][c] - clipped.data[i][c]).abs());
            }
        }
        eprintln!(
            "signed-vs-clipped PPG: diff samples {diff_px}, max |diff| {max_diff:.3e}, min signed {min_signed:.4e}"
        );
        // The signed path must visibly differ from the clipped path at the
        // below-black stencil neighborhoods — else nothing was preserved.
        assert!(diff_px > 0, "signed path identical to clipped — no effect");
        // And stay bounded by the local contrast (interpolation weights).
        assert!(max_diff < 0.05, "unbounded divergence: {max_diff}");
    }

    #[test]
    fn dump_cr3_levels() {
        let Ok(p) = std::env::var("ONCELAB_RAW_FIXTURE") else {
            return;
        };
        let bytes = std::fs::read(&p).unwrap();
        let source = RawSource::new_from_slice(&bytes);
        let decoder = rawler::get_decoder(&source).unwrap();
        let raw = decoder
            .raw_image(&source, &RawDecodeParams::default(), false)
            .unwrap();
        eprintln!("dims: {}x{} cpp={}", raw.width, raw.height, raw.cpp);
        eprintln!("blacklevel: {:?}", raw.blacklevel);
        eprintln!("whitelevel: {:?}", raw.whitelevel);
        eprintln!("wb_coeffs: {:?}", raw.wb_coeffs);
        eprintln!("active_area: {:?}", raw.active_area);
        eprintln!("crop_area: {:?}", raw.crop_area);
        eprintln!("blackareas: {:?}", raw.blackareas);
        eprintln!("photometric: {:?}", raw.photometric);
        eprintln!("color_matrix keys: {:?}", raw.color_matrix.keys());
        for (k, v) in &raw.color_matrix {
            eprintln!("  {k:?}: {v:?}");
        }
        eprintln!("dng_tags: {} entries", raw.dng_tags.len());
        let stats = |d: &[u16]| {
            let mut s: Vec<u32> = d.iter().map(|v| *v as u32).collect();
            s.sort_unstable();
            (s[0], s[s.len() / 2], s[s.len() - 1])
        };
        if let rawler::rawimage::RawImageData::Integer(d) = &raw.data {
            let (mn, md, mx) = stats(d);
            eprintln!("mosaic min/median/max: {mn}/{md}/{mx}");
        }
    }
}
