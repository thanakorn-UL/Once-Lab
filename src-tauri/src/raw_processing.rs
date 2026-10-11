use crate::image_processing::apply_orientation;
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
        cancel_token,
    )
}

/// Develops a RAW image, applying `profile` (with the optional Amount control)
/// when one is supplied.
pub(crate) fn develop_raw_image_with_profile(
    file_bytes: &[u8],
    fast_demosaic: bool,
    highlight_compression: f32,
    linear_mode: String,
    profile: Option<&crate::xmp_profile::XmpRgbProfile>,
    profile_amount_percent: Option<f32>,
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
) -> Result<DynamicImage> {
    let (developed_image, orientation) = develop_internal(
        file_bytes,
        fast_demosaic,
        highlight_compression,
        linear_mode,
        profile,
        profile_amount_percent,
        cancel_token,
    )?;
    Ok(apply_orientation(developed_image, orientation))
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
/// floor. The operation order, normalization, inverse and WB coefficients
/// are the same as `rawler::imgop::raw::map_3ch_to_rgb`. Adobe mode needs
/// the signed linear-sRGB coordinates so a later sRGB->ProPhoto conversion can
/// retain colors outside the smaller sRGB gamut. No change to Legacy or to
/// rendering when an Adobe XMP profile isn't selected.
///
/// This remains a Rawler-based camera transform, NOT an Adobe camera profile
/// or native ProPhoto camera pipeline.
struct UnclippedSrgbCalibration {
    camera_to_srgb: [[f32; 4]; 3],
    white_balance: [f32; 4],
}

impl UnclippedSrgbCalibration {
    fn for_raw(raw: &RawImage) -> Option<Self> {
        // The supported path is ordinary 3-color CFA data. Four-component
        // sensors still use Rawler's own Calibrate operation.
        if raw.cpp == 4 {
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
        let camera_to_srgb = pseudo_inverse(rgb_to_camera);
        if !camera_to_srgb.iter().flatten().all(|v| v.is_finite()) {
            return None;
        }

        let white_balance = if raw.wb_coeffs[0].is_nan() {
            [1.0; 4]
        } else {
            raw.wb_coeffs
        };
        Some(Self {
            camera_to_srgb,
            white_balance,
        })
    }

    fn apply(&self, pixel: &mut [f32; 3]) {
        let [r, g, b] = *pixel;
        let r = r * self.white_balance[0];
        let g = g * self.white_balance[1];
        let b = b * self.white_balance[2];
        let m = &self.camera_to_srgb;
        *pixel = [
            m[0][0] * r + m[0][1] * g + m[0][2] * b,
            m[1][0] * r + m[1][1] * g + m[1][2] * b,
            m[2][0] * r + m[2][1] * g + m[2][2] * b,
        ];
    }
}

/// Prepare a calibrated linear-sRGB pixel for the profile stage.
///
/// Rawler's `Calibrate` step converts camera RGB into linear sRGB/D65. The
/// Adobe-SDK renderer subsequently converts this to ProPhoto/D50. In Adobe
/// mode, retain values above 1.0 and their channel ratios until the
/// RGBTable/LookTable stage performs its own SDK-defined gamut handling.
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

fn develop_internal(
    file_bytes: &[u8],
    fast_demosaic: bool,
    highlight_compression: f32,
    linear_mode: String,
    profile: Option<&crate::xmp_profile::XmpRgbProfile>,
    profile_amount_percent: Option<f32>,
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
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
    let preserve_for_adobe_profile = matches!(
        profile_render_mode,
        Some(crate::xmp_profile::ProfileRenderMode::AdobeSdk)
    );

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

    for level in raw_image.whitelevel.0.iter_mut() {
        *level = u32::MAX;
    }

    // Only Adobe mode can bypass Rawler's sRGB negative-channel clip.
    // For cameras without a standard 3-component color matrix, or when
    // calibration is intentionally disabled, keep Rawler's existing path.
    let unclipped_calibration = if preserve_for_adobe_profile
        && !is_linear_format
        && apply_calibration
    {
        UnclippedSrgbCalibration::for_raw(&raw_image)
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

    raw_image.wb_coeffs =
        crate::multi_exposure::neutralize_wb_if_multiexposure(raw_image.wb_coeffs, file_bytes);

    // The compensation above may change WB; derive the calibration only
    // after the effective white balance has been finalized.
    let unclipped_calibration = if unclipped_calibration.is_some() {
        UnclippedSrgbCalibration::for_raw(&raw_image)
    } else {
        None
    };
    if unclipped_calibration.is_some() {
        developer.steps.retain(|step| *step != ProcessingStep::Calibrate);
    }

    check_cancel()?;
    let mut developed_intermediate = developer.develop_intermediate(&raw_image)?;

    if let (Some(calibration), Intermediate::ThreeColor(pixels)) =
        (&unclipped_calibration, &mut developed_intermediate)
    {
        for pixel in &mut pixels.data {
            calibration.apply(pixel);
        }
    }

    drop(raw_image);

    let denominator = (original_white_level - original_black_level).max(1.0);
    let rescale_factor = (u32::MAX as f32 - original_black_level) / denominator;

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
                *p = prepare_three_color_input(
                    *p,
                    rescale_factor,
                    safe_highlight_compression,
                    clamp_limit,
                    is_linear_format && apply_ungamma,
                    preserve_for_adobe_profile,
                );
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

    if let Some(profile) = profile {
        match &mut developed_intermediate {
            Intermediate::ThreeColor(pixels) => {
                let mode = profile_render_mode.expect("profile implies resolved render mode");
                let applied = crate::xmp_profile::apply_profile_to_three_color_pixels_with_mode(
                    &mut pixels.data,
                    profile,
                    profile_amount_percent,
                    mode,
                );

                applied.map_err(|error| anyhow!(error))?;
            }
            _ => {
                return Err(anyhow!("XMP RGB profile requires ThreeColor intermediate"));
            }
        }
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

        assert_eq!(adobe, source, "Adobe profile must receive uncompressed linear highlights");
        assert_ne!(adobe, legacy, "Legacy path must retain its existing compressor");
        assert!(legacy.iter().all(|v| *v <= 3.0));
    }

    #[test]
    fn adobe_profile_ungamma_preserves_values_above_one() {
        let source = [2.0, 0.5, 0.0];
        let adobe = prepare_three_color_input(source, 1.0, 3.0, 3.0, true, true);
        let legacy = prepare_three_color_input(source, 1.0, 3.0, 3.0, true, false);

        assert!(adobe[0] > 1.0, "do not clip encoded highlights before Adobe stage");
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
    fn calibrated_rgb_values_are_not_floored_in_adobe_mode() {
        let calibration = UnclippedSrgbCalibration {
            camera_to_srgb: [
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
