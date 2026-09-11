use crate::Cursor;
use crate::app_settings::{AppSettings, load_settings};
use crate::app_state::{AppState, LoadedImage};
use crate::exif_processing;
use crate::file_management::{parse_virtual_path, read_file_mapped};
use crate::formats::is_raw_file;
use crate::image_processing::ImageMetadata;
use crate::image_processing::{
    apply_orientation, apply_srgb_to_linear, remove_raw_artifacts_and_enhance,
};
use crate::mask_generation::{MaskDefinition, SubMask, generate_mask_bitmap};
use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use exif::{Reader as ExifReader, Tag};
use image::{DynamicImage, GenericImageView, ImageReader, imageops};
use rawler::Orientation;
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;

#[derive(serde::Serialize)]
pub struct LoadImageResult {
    pub width: u32,
    pub height: u32,
    pub metadata: ImageMetadata,
    pub exif: HashMap<String, String>,
    pub is_raw: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchMaskInfo {
    id: String,
    name: String,
    #[serde(default)]
    invert: bool,
    #[serde(default)]
    sub_masks: Vec<SubMask>,
}

fn srgb_to_linear_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut lut = [0.0f32; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            let x = i as f32 / 255.0;
            *v = if x <= 0.04045 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            };
        }
        lut
    })
}

pub fn load_and_composite(
    base_image: &[u8],
    path: &str,
    adjustments: &Value,
    use_fast_raw_dev: bool,
    settings: &AppSettings,
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
) -> Result<DynamicImage> {
    let base_image =
        load_base_image_from_bytes(base_image, path, use_fast_raw_dev, settings, cancel_token)?;
    composite_patches_on_image(&base_image, adjustments)
}

pub fn load_base_image_from_bytes(
    bytes: &[u8],
    path_for_ext_check: &str,
    use_fast_raw_dev: bool,
    settings: &AppSettings,
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
) -> Result<DynamicImage> {
    load_base_image_from_bytes_with_xmp_profile(
        bytes,
        path_for_ext_check,
        use_fast_raw_dev,
        settings,
        None,
        cancel_token,
    )
}

pub(crate) fn load_base_image_from_bytes_with_xmp_profile(
    bytes: &[u8],
    path_for_ext_check: &str,
    use_fast_raw_dev: bool,
    settings: &AppSettings,
    xmp_profile_path: Option<&Path>,
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
) -> Result<DynamicImage> {
    let highlight_compression = settings.raw_highlight_compression.unwrap_or(2.5);
    let linear_mode = settings.linear_raw_mode.clone();
    let color_nr_setting = settings.raw_preprocessing_color_nr.unwrap_or(0.5);
    let color_nr_amount = if color_nr_setting <= 0.0 {
        0.0
    } else {
        let x = color_nr_setting.clamp(0.01, 1.0);
        (12.0 / x - 10.0).max(0.1)
    };
    let sharpening_amount = settings.raw_preprocessing_sharpening.unwrap_or(0.35);
    let apply_to_non_raws = settings.apply_preprocessing_to_non_raws.unwrap_or(false);

    let is_raw = is_raw_file(path_for_ext_check);

    // A supplied profile is only meaningful for RAW input. Reject it explicitly
    // rather than silently ignoring it; this happens before any file read.
    if !is_raw && xmp_profile_path.is_some() {
        return Err(anyhow!(
            "XMP RGB profiles are supported only for RAW images"
        ));
    }

    crate::exif_processing::persist_exif_if_missing(
        Path::new(path_for_ext_check),
        path_for_ext_check,
        bytes,
    );

    // Load and parse the profile before RAW development, outside the Rawler
    // panic boundary, so a bad profile surfaces as a profile error instead of
    // silently degrading into an unprofiled render.
    let profile = match xmp_profile_path {
        Some(profile_path) if is_raw => Some(
            crate::xmp_profile::load_xmp_rgb_profile_from_path(profile_path)
                .map_err(|error| anyhow!(error))?,
        ),
        _ => None,
    };

    if is_raw {
        let profile_ref = profile.as_ref();

        match panic::catch_unwind(move || {
            crate::raw_processing::develop_raw_image_with_profile(
                bytes,
                use_fast_raw_dev,
                highlight_compression,
                linear_mode,
                profile_ref,
                cancel_token,
            )
        }) {
            Ok(Ok(mut image)) => {
                if !use_fast_raw_dev && (color_nr_amount > 0.0 || sharpening_amount > 0.0) {
                    let start = Instant::now();
                    remove_raw_artifacts_and_enhance(
                        &mut image,
                        color_nr_amount,
                        sharpening_amount,
                    );
                    let duration = start.elapsed();
                    log::info!(
                        "Raw enhancing for '{}' took {:?}",
                        path_for_ext_check,
                        duration
                    );
                }
                Ok(image)
            }
            Ok(Err(e)) => {
                let classified = classify_raw_develop_error(path_for_ext_check, e);

                if classified.to_string().contains("Load cancelled") {
                    return Err(classified);
                }

                // An explicitly requested profile must never be substituted by an
                // embedded preview: the preview does not contain the requested look.
                if let Some(profile_path) = xmp_profile_path {
                    return Err(anyhow!(
                        "failed to develop RAW with XMP profile '{}': {}",
                        profile_path.display(),
                        classified
                    ));
                }

                log::warn!(
                    "Error developing RAW file '{}': {}",
                    path_for_ext_check,
                    classified
                );
                if let Some(preview) = safe_embedded_preview_fallback(bytes, path_for_ext_check) {
                    log::warn!(
                        "Using embedded preview fallback for '{}' ({}x{})",
                        path_for_ext_check,
                        preview.width(),
                        preview.height()
                    );

                    return Ok(linearize_embedded_preview(preview));
                }
                Err(classified)
            }
            Err(_) => {
                if let Some(profile_path) = xmp_profile_path {
                    return Err(anyhow!(
                        "RAW development panicked while applying XMP profile '{}'",
                        profile_path.display()
                    ));
                }

                log::error!("Panic while processing RAW file: {}", path_for_ext_check);
                if let Some(preview) = safe_embedded_preview_fallback(bytes, path_for_ext_check) {
                    log::warn!(
                        "Using embedded preview fallback for '{}' after RAW decoder panic ({}x{})",
                        path_for_ext_check,
                        preview.width(),
                        preview.height()
                    );

                    return Ok(linearize_embedded_preview(preview));
                }
                Err(anyhow!(
                    "Failed to process RAW file: {}",
                    path_for_ext_check
                ))
            }
        }
    } else {
        let mut image = load_image_with_orientation(bytes, cancel_token)?;

        if apply_to_non_raws
            && !use_fast_raw_dev
            && (color_nr_amount > 0.0 || sharpening_amount > 0.0)
        {
            let start = Instant::now();
            remove_raw_artifacts_and_enhance(&mut image, color_nr_amount, sharpening_amount);
            let duration = start.elapsed();
            log::info!(
                "Enhancing non-RAW '{}' took {:?}",
                path_for_ext_check,
                duration
            );
        }

        Ok(image)
    }
}

fn classify_raw_develop_error(path: &str, err: anyhow::Error) -> anyhow::Error {
    let error_text = err.to_string();
    let lowered = error_text.to_ascii_lowercase();
    let unsupported_compression =
        lowered.contains("nef compression") && lowered.contains("not supported");

    if unsupported_compression {
        return anyhow!(
            "Unsupported RAW compression format for '{}'. Original error: {}",
            path,
            error_text
        );
    }

    err
}

fn largest_tiff_jpeg_preview(buf: &[u8]) -> Option<DynamicImage> {
    let le = match buf.get(..4)? {
        [0x49, 0x49, 0x2A, 0x00] => true,
        [0x4D, 0x4D, 0x00, 0x2A] => false,
        _ => return None,
    };
    let rd16 = |o: usize| -> Option<u64> {
        let b: [u8; 2] = buf.get(o..o + 2)?.try_into().ok()?;
        Some(if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        } as u64)
    };
    let rd32 = |o: usize| -> Option<u64> {
        let b: [u8; 4] = buf.get(o..o + 4)?.try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        } as u64)
    };

    let mut candidates: Vec<(u64, u64)> = Vec::new();
    let mut queue: Vec<u64> = vec![rd32(4)?];
    let mut seen = HashMap::new();

    while let Some(ifd) = queue.pop() {
        if seen.insert(ifd, ()).is_some() || seen.len() > 64 {
            continue;
        }
        let Some(n) = rd16(ifd as usize) else {
            continue;
        };

        let mut compression: u64 = 0;
        let mut strip: Option<(u64, u64)> = None;
        let mut old_jpeg: Option<(u64, u64)> = None;

        for i in 0..n {
            let e = ifd as usize + 2 + (i as usize) * 12;
            let (Some(tag), Some(count), Some(val)) = (rd16(e), rd32(e + 4), rd32(e + 8)) else {
                continue;
            };
            match tag {
                259 => compression = val,
                273 if count == 1 => strip = Some((val, strip.map_or(0, |s| s.1))),
                279 if count == 1 => strip = strip.map(|s| (s.0, val)).or(Some((0, val))),
                513 => old_jpeg = Some((val, old_jpeg.map_or(0, |s| s.1))),
                514 => old_jpeg = old_jpeg.map(|s| (s.0, val)).or(Some((0, val))),
                330 => {
                    if count == 1 {
                        queue.push(val);
                    } else {
                        for j in 0..count.min(8) {
                            if let Some(p) = rd32(val as usize + (j as usize) * 4) {
                                queue.push(p);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if matches!(compression, 6 | 7)
            && let Some(s) = strip
        {
            candidates.push(s);
        }
        if let Some(oj) = old_jpeg {
            candidates.push(oj);
        }
        if let Some(next) = rd32(ifd as usize + 2 + (n as usize) * 12)
            && next != 0
        {
            queue.push(next);
        }
    }

    candidates.sort_by_key(|&(_, len)| std::cmp::Reverse(len));

    for (off, len) in candidates {
        if let Some(bytes) = buf.get(off as usize..(off + len) as usize)
            && let Ok(img) = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
        {
            return Some(img);
        }
    }

    None
}

fn embedded_preview_fallback(bytes: &[u8], path: &str) -> Option<DynamicImage> {
    let img = match largest_tiff_jpeg_preview(bytes) {
        Some(img) => img,
        None => rawler::analyze::extract_preview_pixels(
            path,
            &rawler::decoders::RawDecodeParams::default(),
        )
        .ok()?,
    };

    let orientation = ExifReader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
        .and_then(|exif| {
            exif.get_field(Tag::Orientation, exif::In::PRIMARY)?
                .value
                .get_uint(0)
        });

    Some(match orientation {
        Some(o) if o > 1 => apply_orientation(img, Orientation::from_u16(o as u16)),
        _ => img,
    })
}

fn safe_embedded_preview_fallback(bytes: &[u8], path: &str) -> Option<DynamicImage> {
    match panic::catch_unwind(panic::AssertUnwindSafe(|| {
        embedded_preview_fallback(bytes, path)
    })) {
        Ok(preview) => preview,
        Err(_) => {
            log::warn!("Embedded RAW preview extraction panicked for '{}'", path);
            None
        }
    }
}

fn linearize_embedded_preview(preview: DynamicImage) -> DynamicImage {
    let preview = DynamicImage::ImageRgb32F(preview.to_rgb32f());
    let mut linear_preview = apply_srgb_to_linear(preview).into_rgb32f();
    for pixel in linear_preview.pixels_mut() {
        pixel[0] *= 0.4;
        pixel[1] *= 0.4;
        pixel[2] *= 0.4;
    }
    DynamicImage::ImageRgb32F(linear_preview)
}

pub fn load_image_with_orientation(
    bytes: &[u8],
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
) -> Result<DynamicImage> {
    let check_cancel = || -> Result<()> {
        if let Some((tracker, generation)) = &cancel_token
            && tracker.load(Ordering::SeqCst) != *generation
        {
            return Err(anyhow!("Load cancelled"));
        }
        Ok(())
    };

    let cursor = Cursor::new(bytes);
    let mut reader = ImageReader::new(cursor.clone())
        .with_guessed_format()
        .context("Failed to guess image format")?;

    reader.no_limits();

    check_cancel()?;

    let image = reader.decode().context("Failed to decode image")?;
    check_cancel()?;

    let oriented_image = {
        let exif_reader = ExifReader::new();
        if let Ok(exif) = exif_reader.read_from_container(&mut cursor.clone()) {
            if let Some(orientation) = exif
                .get_field(Tag::Orientation, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
            {
                check_cancel()?;
                apply_orientation(image, Orientation::from_u16(orientation as u16))
            } else {
                image
            }
        } else {
            image
        }
    };

    Ok(DynamicImage::ImageRgb32F(oriented_image.to_rgb32f()))
}

pub fn composite_patches_on_image(
    base_image: &DynamicImage,
    current_adjustments: &Value,
) -> Result<DynamicImage> {
    let patches_val = match current_adjustments.get("aiPatches") {
        Some(val) => val,
        None => return Ok(base_image.clone()),
    };

    let patches_arr = match patches_val.as_array() {
        Some(arr) if !arr.is_empty() => arr,
        _ => return Ok(base_image.clone()),
    };

    let visible_patches: Vec<&Value> = patches_arr
        .par_iter()
        .filter(|patch_obj| {
            let is_visible = patch_obj
                .get("visible")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if !is_visible {
                return false;
            }
            patch_obj
                .get("patchData")
                .and_then(|data| data.get("color"))
                .and_then(|color| color.as_str())
                .is_some_and(|s| !s.is_empty())
        })
        .collect();

    if visible_patches.is_empty() {
        return Ok(base_image.clone());
    }

    let (base_w, base_h) = base_image.dimensions();

    struct DecodedPatch {
        offset_x: Option<u32>,
        offset_y: Option<u32>,
        mask: image::GrayImage,
        color: image::RgbImage,
        is_srgb_encoded: bool,
    }

    let decoded_patches: Result<Vec<DecodedPatch>> = visible_patches
        .par_iter()
        .map(|patch_obj| {
            let patch_data = patch_obj.get("patchData").context("Missing patchData")?;
            let offset_x = patch_data
                .get("offsetX")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let offset_y = patch_data
                .get("offsetY")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let is_cropped = offset_x.is_some() && offset_y.is_some();

            let is_srgb_encoded = patch_data
                .get("isSrgbEncoded")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let mask_bitmap = if let Some(mask_b64) = patch_data
                .get("mask")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                let mask_bytes = general_purpose::STANDARD.decode(mask_b64)?;
                let mask_img = image::load_from_memory(&mask_bytes)?.to_luma8();
                if !is_cropped && (mask_img.width() != base_w || mask_img.height() != base_h) {
                    imageops::resize(&mask_img, base_w, base_h, imageops::FilterType::Lanczos3)
                } else {
                    mask_img
                }
            } else {
                let patch_info: PatchMaskInfo = serde_json::from_value((*patch_obj).clone())
                    .context("Failed to deserialize patch info for mask generation")?;

                let mask_def = MaskDefinition {
                    id: patch_info.id,
                    name: patch_info.name,
                    visible: true,
                    invert: patch_info.invert,
                    opacity: 100.0,
                    adjustments: Value::Null,
                    sub_masks: patch_info.sub_masks,
                };

                let orientation_steps = current_adjustments
                    .get("orientationSteps")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u8;
                let (trans_w, trans_h) = if orientation_steps % 2 == 1 {
                    (base_h, base_w)
                } else {
                    (base_w, base_h)
                };

                let mut gen_mask =
                    generate_mask_bitmap(&mask_def, trans_w, trans_h, 1.0, (0.0, 0.0), None)
                        .context("Failed to generate mask from sub_masks for compositing")?;

                gen_mask =
                    crate::image_processing::inverse_transform_mask(gen_mask, current_adjustments);

                if let (Some(ox), Some(oy)) = (offset_x, offset_y) {
                    let w = patch_data
                        .get("width")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32)
                        .unwrap_or(base_w);
                    let h = patch_data
                        .get("height")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32)
                        .unwrap_or(base_h);
                    let crop_w = w.min(base_w.saturating_sub(ox));
                    let crop_h = h.min(base_h.saturating_sub(oy));
                    gen_mask = imageops::crop_imm(&gen_mask, ox, oy, crop_w, crop_h).to_image();
                }
                gen_mask
            };

            let color_b64 = patch_data
                .get("color")
                .and_then(|v| v.as_str())
                .context("Missing color data")?;
            let color_bytes = general_purpose::STANDARD.decode(color_b64)?;
            let color_image_u8 = image::load_from_memory(&color_bytes)?.to_rgb8();

            let (patch_w, patch_h) = color_image_u8.dimensions();
            let final_color = if !is_cropped && (base_w != patch_w || base_h != patch_h) {
                imageops::resize(
                    &color_image_u8,
                    base_w,
                    base_h,
                    imageops::FilterType::Lanczos3,
                )
            } else {
                color_image_u8
            };

            Ok(DecodedPatch {
                offset_x,
                offset_y,
                mask: mask_bitmap,
                color: final_color,
                is_srgb_encoded,
            })
        })
        .collect();

    let decoded_patches = decoded_patches?;

    let mut composited_image = base_image.clone();
    let lut = srgb_to_linear_lut();

    let get_color = |patch: &DecodedPatch, r: u8, g: u8, b: u8| -> (f32, f32, f32) {
        if patch.is_srgb_encoded {
            (lut[r as usize], lut[g as usize], lut[b as usize])
        } else {
            (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
        }
    };

    match &mut composited_image {
        DynamicImage::ImageRgb32F(img_buf) => {
            for patch in decoded_patches {
                let mask_raw = patch.mask.as_raw();
                let color_raw = patch.color.as_raw();
                let patch_w = patch.mask.width() as usize;

                if let (Some(ox), Some(oy)) = (patch.offset_x, patch.offset_y) {
                    let max_x = (ox + patch.mask.width()).min(base_w);
                    let max_y = (oy + patch.mask.height()).min(base_h);

                    let crop_w = max_x.saturating_sub(ox) as usize;
                    let crop_h = max_y.saturating_sub(oy) as usize;

                    if crop_w == 0 || crop_h == 0 {
                        continue;
                    }

                    let base_w_usize = base_w as usize;
                    let ox_usize = ox as usize;
                    let oy_usize = oy as usize;

                    img_buf
                        .par_chunks_mut(base_w_usize * 3)
                        .enumerate()
                        .skip(oy_usize)
                        .take(crop_h)
                        .for_each(|(y, row)| {
                            let py = y - oy_usize;
                            let patch_row_start = py * patch_w;

                            for x in ox_usize..(ox_usize + crop_w) {
                                let px = x - ox_usize;
                                let mask_idx = patch_row_start + px;
                                let mask_value = mask_raw[mask_idx];

                                if mask_value > 0 {
                                    let color_idx = mask_idx * 3;
                                    let pr_u8 = color_raw[color_idx];
                                    let pg_u8 = color_raw[color_idx + 1];
                                    let pb_u8 = color_raw[color_idx + 2];

                                    let (pr, pg, pb) = get_color(&patch, pr_u8, pg_u8, pb_u8);

                                    let alpha = mask_value as f32 / 255.0;
                                    let one_minus_alpha = 1.0 - alpha;

                                    let base_idx = x * 3;
                                    row[base_idx] = pr * alpha + row[base_idx] * one_minus_alpha;
                                    row[base_idx + 1] =
                                        pg * alpha + row[base_idx + 1] * one_minus_alpha;
                                    row[base_idx + 2] =
                                        pb * alpha + row[base_idx + 2] * one_minus_alpha;
                                }
                            }
                        });
                } else {
                    img_buf
                        .par_chunks_mut((base_w * 3) as usize)
                        .enumerate()
                        .for_each(|(y, row)| {
                            let patch_row_start = y * patch_w;
                            for x in 0..base_w as usize {
                                let mask_idx = patch_row_start + x;
                                let mask_value = mask_raw[mask_idx];
                                if mask_value > 0 {
                                    let color_idx = mask_idx * 3;
                                    let pr_u8 = color_raw[color_idx];
                                    let pg_u8 = color_raw[color_idx + 1];
                                    let pb_u8 = color_raw[color_idx + 2];

                                    let (pr, pg, pb) = get_color(&patch, pr_u8, pg_u8, pb_u8);

                                    let alpha = mask_value as f32 / 255.0;
                                    let one_minus_alpha = 1.0 - alpha;

                                    row[x * 3] = pr * alpha + row[x * 3] * one_minus_alpha;
                                    row[x * 3 + 1] = pg * alpha + row[x * 3 + 1] * one_minus_alpha;
                                    row[x * 3 + 2] = pb * alpha + row[x * 3 + 2] * one_minus_alpha;
                                }
                            }
                        });
                }
            }
        }
        DynamicImage::ImageRgba32F(img_buf) => {
            for patch in decoded_patches {
                let mask_raw = patch.mask.as_raw();
                let color_raw = patch.color.as_raw();
                let patch_w = patch.mask.width() as usize;

                if let (Some(ox), Some(oy)) = (patch.offset_x, patch.offset_y) {
                    let max_x = (ox + patch.mask.width()).min(base_w);
                    let max_y = (oy + patch.mask.height()).min(base_h);

                    let crop_w = max_x.saturating_sub(ox) as usize;
                    let crop_h = max_y.saturating_sub(oy) as usize;

                    if crop_w == 0 || crop_h == 0 {
                        continue;
                    }

                    let base_w_usize = base_w as usize;
                    let ox_usize = ox as usize;
                    let oy_usize = oy as usize;

                    img_buf
                        .par_chunks_mut(base_w_usize * 4)
                        .enumerate()
                        .skip(oy_usize)
                        .take(crop_h)
                        .for_each(|(y, row)| {
                            let py = y - oy_usize;
                            let patch_row_start = py * patch_w;

                            for x in ox_usize..(ox_usize + crop_w) {
                                let px = x - ox_usize;
                                let mask_idx = patch_row_start + px;
                                let mask_value = mask_raw[mask_idx];

                                if mask_value > 0 {
                                    let color_idx = mask_idx * 3;
                                    let pr_u8 = color_raw[color_idx];
                                    let pg_u8 = color_raw[color_idx + 1];
                                    let pb_u8 = color_raw[color_idx + 2];

                                    let (pr, pg, pb) = get_color(&patch, pr_u8, pg_u8, pb_u8);
                                    let alpha = mask_value as f32 / 255.0;
                                    let one_minus_alpha = 1.0 - alpha;

                                    let base_idx = x * 4;
                                    row[base_idx] = pr * alpha + row[base_idx] * one_minus_alpha;
                                    row[base_idx + 1] =
                                        pg * alpha + row[base_idx + 1] * one_minus_alpha;
                                    row[base_idx + 2] =
                                        pb * alpha + row[base_idx + 2] * one_minus_alpha;
                                }
                            }
                        });
                } else {
                    img_buf
                        .par_chunks_mut((base_w * 4) as usize)
                        .enumerate()
                        .for_each(|(y, row)| {
                            let patch_row_start = y * patch_w;
                            for x in 0..base_w as usize {
                                let mask_idx = patch_row_start + x;
                                let mask_value = mask_raw[mask_idx];
                                if mask_value > 0 {
                                    let color_idx = mask_idx * 3;
                                    let pr_u8 = color_raw[color_idx];
                                    let pg_u8 = color_raw[color_idx + 1];
                                    let pb_u8 = color_raw[color_idx + 2];

                                    let (pr, pg, pb) = get_color(&patch, pr_u8, pg_u8, pb_u8);

                                    let alpha = mask_value as f32 / 255.0;
                                    let one_minus_alpha = 1.0 - alpha;

                                    row[x * 4] = pr * alpha + row[x * 4] * one_minus_alpha;
                                    row[x * 4 + 1] = pg * alpha + row[x * 4 + 1] * one_minus_alpha;
                                    row[x * 4 + 2] = pb * alpha + row[x * 4 + 2] * one_minus_alpha;
                                }
                            }
                        });
                }
            }
        }
        _ => {
            let mut rgba32_img = composited_image.to_rgba32f();
            for patch in decoded_patches {
                let mask_raw = patch.mask.as_raw();
                let color_raw = patch.color.as_raw();
                let patch_w = patch.mask.width() as usize;

                if let (Some(ox), Some(oy)) = (patch.offset_x, patch.offset_y) {
                    let max_x = (ox + patch.mask.width()).min(base_w);
                    let max_y = (oy + patch.mask.height()).min(base_h);

                    let crop_w = max_x.saturating_sub(ox) as usize;
                    let crop_h = max_y.saturating_sub(oy) as usize;

                    if crop_w == 0 || crop_h == 0 {
                        continue;
                    }

                    let base_w_usize = base_w as usize;
                    let ox_usize = ox as usize;
                    let oy_usize = oy as usize;

                    rgba32_img
                        .par_chunks_mut(base_w_usize * 4)
                        .enumerate()
                        .skip(oy_usize)
                        .take(crop_h)
                        .for_each(|(y, row)| {
                            let py = y - oy_usize;
                            let patch_row_start = py * patch_w;

                            for x in ox_usize..(ox_usize + crop_w) {
                                let px = x - ox_usize;
                                let mask_idx = patch_row_start + px;
                                let mask_value = mask_raw[mask_idx];

                                if mask_value > 0 {
                                    let color_idx = mask_idx * 3;
                                    let pr_u8 = color_raw[color_idx];
                                    let pg_u8 = color_raw[color_idx + 1];
                                    let pb_u8 = color_raw[color_idx + 2];

                                    let (pr, pg, pb) = get_color(&patch, pr_u8, pg_u8, pb_u8);
                                    let alpha = mask_value as f32 / 255.0;
                                    let one_minus_alpha = 1.0 - alpha;

                                    let base_idx = x * 4;
                                    row[base_idx] = pr * alpha + row[base_idx] * one_minus_alpha;
                                    row[base_idx + 1] =
                                        pg * alpha + row[base_idx + 1] * one_minus_alpha;
                                    row[base_idx + 2] =
                                        pb * alpha + row[base_idx + 2] * one_minus_alpha;
                                }
                            }
                        });
                } else {
                    rgba32_img
                        .par_chunks_mut((base_w * 4) as usize)
                        .enumerate()
                        .for_each(|(y, row)| {
                            let patch_row_start = y * patch_w;
                            for x in 0..base_w as usize {
                                let mask_idx = patch_row_start + x;
                                let mask_value = mask_raw[mask_idx];
                                if mask_value > 0 {
                                    let color_idx = mask_idx * 3;
                                    let pr_u8 = color_raw[color_idx];
                                    let pg_u8 = color_raw[color_idx + 1];
                                    let pb_u8 = color_raw[color_idx + 2];

                                    let (pr, pg, pb) = get_color(&patch, pr_u8, pg_u8, pb_u8);
                                    let alpha = mask_value as f32 / 255.0;
                                    let one_minus_alpha = 1.0 - alpha;

                                    row[x * 4] = pr * alpha + row[x * 4] * one_minus_alpha;
                                    row[x * 4 + 1] = pg * alpha + row[x * 4 + 1] * one_minus_alpha;
                                    row[x * 4 + 2] = pb * alpha + row[x * 4 + 2] * one_minus_alpha;
                                }
                            }
                        });
                }
            }
            composited_image = DynamicImage::ImageRgba32F(rgba32_img);
        }
    }

    Ok(composited_image)
}

#[tauri::command]
pub fn is_image_cached(path: String, state: tauri::State<'_, AppState>) -> bool {
    let (source_path, _) = parse_virtual_path(&path);
    let source_path_str = source_path.to_string_lossy().to_string();
    state
        .decoded_image_cache
        .lock()
        .unwrap()
        .get(&source_path_str)
        .is_some()
}

#[tauri::command]
pub async fn load_image(
    path: String,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<LoadImageResult, String> {
    load_image_inner(path, None, &state, &app_handle).await
}

/// Profile-aware companion to [`load_image`].
///
/// Exists as a separate command so the existing no-profile callers keep calling
/// `load_image` unchanged.
#[tauri::command]
pub async fn load_image_with_xmp_profile(
    path: String,
    xmp_profile_path: String,
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<LoadImageResult, String> {
    load_image_inner(path, Some(PathBuf::from(xmp_profile_path)), &state, &app_handle).await
}

async fn load_image_inner(
    path: String,
    xmp_profile_path: Option<PathBuf>,
    state: &AppState,
    app_handle: &tauri::AppHandle,
) -> Result<LoadImageResult, String> {
    let my_generation = state.load_image_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let generation_tracker = state.load_image_generation.clone();
    let cancel_token = Some((generation_tracker.clone(), my_generation));
    let is_profile_load = xmp_profile_path.is_some();
    let same_image_reload = is_same_image_reload(state, is_profile_load, &path);

    prepare_image_load(state, same_image_reload);

    let (source_path, sidecar_path) = parse_virtual_path(&path);
    let source_path_str = source_path.to_string_lossy().to_string();

    let metadata: ImageMetadata = crate::exif_processing::load_sidecar(&sidecar_path);

    let settings = load_settings(app_handle.clone()).unwrap_or_default();

    let path_clone = source_path_str.clone();

    let cached_data = if is_profile_load {
        // A profile changes the decoded pixels, so the path-keyed pristine cache
        // must be neither read nor written here: reading it would silently drop
        // the profile, and writing it would poison the baseline entry.
        None
    } else {
        state
            .decoded_image_cache
            .lock()
            .unwrap()
            .get(&source_path_str)
    };

    let (pristine_arc, exif_data) = if let Some((cached_img, cached_exif)) = cached_data {
        (cached_img, cached_exif)
    } else {
        if crate::file_management::is_cloud_placeholder(&source_path) {
            return Err(format!(
                "'{}' is stored in iCloud and hasn't been downloaded yet. Download it in Finder, then try again.",
                source_path_str
            ));
        }

        let (pristine_img, exif_data_loaded) = tokio::task::spawn_blocking(move || {
            if generation_tracker.load(Ordering::SeqCst) != my_generation {
                return Err("Load cancelled".to_string());
            }

            let result: Result<(DynamicImage, HashMap<String, String>), String> =
                (|| match read_file_mapped(Path::new(&path_clone)) {
                    Ok(mmap) => {
                        if generation_tracker.load(Ordering::SeqCst) != my_generation {
                            return Err("Load cancelled".to_string());
                        }

                        let img = load_base_image_from_bytes_with_xmp_profile(
                            &mmap,
                            &path_clone,
                            false,
                            &settings,
                            xmp_profile_path.as_deref(),
                            cancel_token.clone(),
                        )
                        .map_err(|e| e.to_string())?;
                        let exif = exif_processing::read_exif_data(&path_clone, &mmap);
                        Ok((img, exif))
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to memory-map file '{}': {}. Falling back to standard read.",
                            path_clone,
                            e
                        );
                        let bytes = fs::read(&path_clone).map_err(|io_err| {
                            format!("Fallback read failed for {}: {}", path_clone, io_err)
                        })?;

                        if generation_tracker.load(Ordering::SeqCst) != my_generation {
                            return Err("Load cancelled".to_string());
                        }

                        let img = load_base_image_from_bytes_with_xmp_profile(
                            &bytes,
                            &path_clone,
                            false,
                            &settings,
                            xmp_profile_path.as_deref(),
                            cancel_token.clone(),
                        )
                        .map_err(|e| e.to_string())?;
                        let exif = exif_processing::read_exif_data(&path_clone, &bytes);
                        Ok((img, exif))
                    }
                })();
            result
        })
        .await
        .map_err(|e| e.to_string())??;

        let arc_img = Arc::new(pristine_img);

        if !is_profile_load {
            state.decoded_image_cache.lock().unwrap().insert(
                source_path_str.clone(),
                arc_img.clone(),
                exif_data_loaded.clone(),
            );
        }

        (arc_img, exif_data_loaded)
    };

    if state.load_image_generation.load(Ordering::SeqCst) != my_generation {
        return Err("Load cancelled".to_string());
    }

    let is_raw = is_raw_file(&source_path_str);
    let (orig_width, orig_height) = pristine_arc.dimensions();

    commit_loaded_image(
        state,
        my_generation,
        same_image_reload,
        path,
        pristine_arc,
        is_raw,
    )?;

    Ok(LoadImageResult {
        width: orig_width,
        height: orig_height,
        metadata,
        exif: exif_data,
        is_raw,
    })
}

/// Drops the image that is currently loaded and everything derived from it.
fn reset_image_state(state: &AppState) {
    *state
        .original_image
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    *state
        .cached_preview
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    *state
        .gpu_image_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    *state
        .full_warped_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    *state
        .full_transformed_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;

    state
        .mask_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    state
        .patch_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    state
        .geometry_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();

    *state
        .denoise_result
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    *state.hdr_result.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *state
        .panorama_result
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

/// Whether this load replaces the image that is already open instead of opening a
/// different one.
///
/// A profile-aware command always is such a reload; a plain load is one when it
/// asks for the path that is already loaded, which is how a profile is removed.
/// Those reloads are transactional: the image on screen must stay usable until
/// its replacement has been developed, so their destructive reset is deferred.
fn is_same_image_reload(state: &AppState, is_profile_load: bool, path: &str) -> bool {
    if is_profile_load {
        return true;
    }

    state
        .original_image
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|loaded| loaded.path == path)
}

/// Prepares `state` for an incoming load.
///
/// A reload of the image that is already open must not destroy that image (nor
/// anything derived from it) until the replacement has been developed: it defers
/// the reset to [`commit_loaded_image`], which leaves the previous image usable if
/// the attempt fails. Opening a different image keeps clearing up front, exactly
/// as before.
fn prepare_image_load(state: &AppState, same_image_reload: bool) {
    if !same_image_reload {
        reset_image_state(state);
    }
}

/// Installs a freshly developed image, replacing the previous one (and everything
/// derived from it) once it is known-good.
///
/// Re-checks the load generation so a cancelled or superseded request can never
/// replace the image of the newer request that won.
fn commit_loaded_image(
    state: &AppState,
    my_generation: usize,
    same_image_reload: bool,
    path: String,
    image: Arc<DynamicImage>,
    is_raw: bool,
) -> Result<(), String> {
    if state.load_image_generation.load(Ordering::SeqCst) != my_generation {
        return Err("Load cancelled".to_string());
    }

    if same_image_reload {
        reset_image_state(state);
    }

    *state.original_image.lock().unwrap() = Some(LoadedImage {
        path,
        image,
        is_raw,
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::app_state::{CachedPreview, MetadataManager, ThumbnailManager, ThumbnailProgressTracker};
    use crate::cache_utils::DecodedImageCache;
    use crate::camera_tethering::CameraSession;
    use image::{GrayImage, Rgb, RgbImage};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Mutex as TokioMutex;

    /// Backend state with nothing loaded. Every field is an empty slot, so the
    /// image-transaction helpers can be exercised without a Tauri app instance.
    fn test_state() -> AppState {
        AppState {
            window_setup_complete: AtomicBool::new(false),
            gpu_crash_flag_path: Mutex::new(None),
            original_image: Mutex::new(None),
            cached_preview: Mutex::new(None),
            gpu_context: Mutex::new(None),
            gpu_image_cache: Mutex::new(None),
            gpu_processor: Mutex::new(None),
            ai_state: Mutex::new(None),
            ai_init_lock: TokioMutex::new(()),
            export_task_token: Arc::new(Mutex::new(None)),
            hdr_result: Arc::new(Mutex::new(None)),
            panorama_result: Arc::new(Mutex::new(None)),
            focus_stack_result: Arc::new(Mutex::new(None)),
            denoise_result: Arc::new(Mutex::new(None)),
            indexing_task_handle: Mutex::new(None),
            lut_cache: Mutex::new(HashMap::new()),
            initial_file_path: Mutex::new(None),
            pending_edit_session: Mutex::new(None),
            thumbnail_cancellation_token: Arc::new(AtomicBool::new(false)),
            thumbnail_progress: Mutex::new(ThumbnailProgressTracker { total: 0, completed: 0 }),
            preview_worker_tx: Mutex::new(None),
            analytics_worker_tx: Mutex::new(None),
            mask_cache: Mutex::new(HashMap::new()),
            patch_cache: Mutex::new(HashMap::new()),
            geometry_cache: Mutex::new(HashMap::new()),
            thumbnail_geometry_cache: Mutex::new(HashMap::new()),
            lens_db: Mutex::new(None),
            load_image_generation: Arc::new(AtomicUsize::new(0)),
            full_warped_cache: Mutex::new(None),
            full_transformed_cache: Mutex::new(None),
            decoded_image_cache: Mutex::new(DecodedImageCache::new(5)),
            thumbnail_manager: ThumbnailManager::new(),
            metadata_manager: MetadataManager::new(),
            disks_cache: Mutex::new(None),
            disks_cache_refreshing: AtomicBool::new(false),
            camera_session: Mutex::new(CameraSession::new()),
        }
    }

    fn tiny_image(rgb: [u8; 3]) -> Arc<DynamicImage> {
        Arc::new(DynamicImage::ImageRgb8(RgbImage::from_pixel(
            2,
            2,
            Rgb(rgb),
        )))
    }

    fn first_pixel(image: &DynamicImage) -> [u8; 3] {
        image.to_rgb8().get_pixel(0, 0).0
    }

    /// Installs an image as if a previous load had succeeded, together with
    /// derived state that a new image must invalidate.
    fn install_previous_image(state: &AppState, rgb: [u8; 3]) -> Arc<DynamicImage> {
        let previous = tiny_image(rgb);
        *state.original_image.lock().unwrap() = Some(LoadedImage {
            path: "TLP_8278.NEF".to_string(),
            image: previous.clone(),
            is_raw: true,
        });
        *state.cached_preview.lock().unwrap() = Some(CachedPreview {
            image: previous.clone(),
            small_image: previous.clone(),
            transform_hash: 1,
            scale: 1.0,
            unscaled_crop_offset: (0.0, 0.0),
            preview_dim: 512,
            interactive_divisor: 1.0,
        });
        state.mask_cache.lock().unwrap().insert(7, GrayImage::new(2, 2));

        previous
    }

    /// Mirrors the production sequence at the start of `load_image_inner` and
    /// returns the flag the commit step receives for the same request.
    fn begin_load(state: &AppState, is_profile_load: bool, path: &str) -> bool {
        let same_image_reload = is_same_image_reload(state, is_profile_load, path);
        prepare_image_load(state, same_image_reload);
        same_image_reload
    }

    #[test]
    fn profile_reload_failure_preserves_previous_original_image() {
        let state = test_state();
        let previous = install_previous_image(&state, [10, 20, 30]);

        // A profile-aware reload of the image that is already open begins here. Its
        // decode fails, so `commit_loaded_image` is never reached and the load
        // returns an error with no further state mutation.
        assert!(begin_load(&state, true, "TLP_8278.NEF"));

        let guard = state.original_image.lock().unwrap();
        let loaded = guard
            .as_ref()
            .expect("a failed profile reload must keep the previous image loaded");
        assert!(
            Arc::ptr_eq(&loaded.image, &previous),
            "the previous image must not be replaced or copied"
        );
        assert_eq!(loaded.path, "TLP_8278.NEF");
        assert_eq!(first_pixel(&loaded.image), [10, 20, 30]);
        drop(guard);

        assert!(
            state.cached_preview.lock().unwrap().is_some(),
            "the render cache of the previous image must survive a failed reload"
        );
        assert_eq!(
            state.mask_cache.lock().unwrap().len(),
            1,
            "mask results of the previous image must survive a failed reload"
        );
    }

    #[test]
    fn failed_clear_reload_preserves_previous_original_image() {
        let state = test_state();
        let previous = install_previous_image(&state, [10, 20, 30]);

        // Clear reloads the same image without a profile. The decode fails, so
        // `commit_loaded_image` is never reached.
        assert!(
            begin_load(&state, false, "TLP_8278.NEF"),
            "reloading the image that is open must be treated as a same-image reload"
        );

        let guard = state.original_image.lock().unwrap();
        let loaded = guard
            .as_ref()
            .expect("a failed Clear must keep the profiled image loaded");
        assert!(Arc::ptr_eq(&loaded.image, &previous));
        assert_eq!(first_pixel(&loaded.image), [10, 20, 30]);
        drop(guard);

        assert!(state.cached_preview.lock().unwrap().is_some());
    }

    #[test]
    fn same_image_reload_detection() {
        let state = test_state();

        assert!(
            !is_same_image_reload(&state, false, "TLP_8278.NEF"),
            "with nothing loaded this is an image opening, not a reload"
        );
        assert!(is_same_image_reload(&state, true, "TLP_8278.NEF"));

        install_previous_image(&state, [10, 20, 30]);

        assert!(
            is_same_image_reload(&state, false, "TLP_8278.NEF"),
            "a plain load of the open image reloads it"
        );
        assert!(
            !is_same_image_reload(&state, false, "other.NEF"),
            "a plain load of another image opens it"
        );
        assert!(
            is_same_image_reload(&state, true, "other.NEF"),
            "the profile-aware command is always a reload"
        );
    }

    #[test]
    fn normal_image_load_still_clears_the_previous_image_up_front() {
        let state = test_state();
        install_previous_image(&state, [10, 20, 30]);

        begin_load(&state, false, "other.NEF");

        assert!(
            state.original_image.lock().unwrap().is_none(),
            "opening another image must keep clearing up front"
        );
        assert!(state.cached_preview.lock().unwrap().is_none());
        assert!(state.mask_cache.lock().unwrap().is_empty());
    }

    #[test]
    fn successful_profile_commit_replaces_the_previous_image() {
        let state = test_state();
        let previous = install_previous_image(&state, [10, 20, 30]);

        let same_image_reload = begin_load(&state, true, "TLP_8278.NEF");
        state.load_image_generation.store(4, Ordering::SeqCst);

        let candidate = tiny_image([200, 100, 50]);
        commit_loaded_image(
            &state,
            4,
            same_image_reload,
            "TLP_8278.NEF".to_string(),
            candidate.clone(),
            true,
        )
        .expect("the winning request must be able to commit");

        let guard = state.original_image.lock().unwrap();
        let loaded = guard.as_ref().expect("the profiled image must be installed");
        assert!(
            Arc::ptr_eq(&loaded.image, &candidate),
            "the profiled candidate must become the loaded image"
        );
        assert!(
            !Arc::ptr_eq(&loaded.image, &previous),
            "the previous pixels must not be retained"
        );
        assert_eq!(first_pixel(&loaded.image), [200, 100, 50]);
        drop(guard);

        assert!(
            state.cached_preview.lock().unwrap().is_none(),
            "the previous transform cache must be dropped"
        );
        assert!(
            state.mask_cache.lock().unwrap().is_empty(),
            "mask results of the previous image must be dropped"
        );
    }

    #[test]
    fn superseded_profile_commit_cannot_replace_the_newer_image() {
        let state = test_state();
        let previous = install_previous_image(&state, [10, 20, 30]);

        let same_image_reload = begin_load(&state, true, "TLP_8278.NEF");

        // This candidate belongs to generation 4, but a newer load has taken over.
        state.load_image_generation.store(5, Ordering::SeqCst);

        let error = commit_loaded_image(
            &state,
            4,
            same_image_reload,
            "TLP_8278.NEF".to_string(),
            tiny_image([200, 100, 50]),
            true,
        )
        .expect_err("a superseded request must not commit its image");

        assert_eq!(error, "Load cancelled");

        let guard = state.original_image.lock().unwrap();
        let loaded = guard.as_ref().expect("the newer image must stay in place");
        assert!(Arc::ptr_eq(&loaded.image, &previous));
        assert_eq!(first_pixel(&loaded.image), [10, 20, 30]);
        drop(guard);

        assert!(state.cached_preview.lock().unwrap().is_some());
        assert_eq!(state.mask_cache.lock().unwrap().len(), 1);
    }

    #[test]
    fn non_raw_image_rejects_xmp_profile() {
        let settings = AppSettings::default();

        // The rejection must happen before any profile file read, so the
        // supplied path is deliberately nonexistent and the bytes are empty.
        let profile_path = Path::new("/definitely/not/a/real/profile.xmp");

        let error = load_base_image_from_bytes_with_xmp_profile(
            b"",
            "photo.jpg",
            false,
            &settings,
            Some(profile_path),
            None,
        )
        .expect_err("a non-RAW image with an XMP profile must be rejected");

        assert_eq!(
            error.to_string(),
            "XMP RGB profiles are supported only for RAW images"
        );
    }
}
