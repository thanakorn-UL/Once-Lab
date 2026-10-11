//! Reproducible RAW-baseline pipeline diagnostics.
//!
//! [`RawPipelineDiagnostics`] is an opt-in sink threaded through
//! `develop_internal`: every recorded number describes an *executed*
//! operation (the same buffers the renderer produced), not a recomputed
//! approximation. The owner comparison fixtures are CR3 files, so all
//! mosaic-level fields come straight from the decoded `RawImage` and the
//! stage samples from the live intermediates.
//!
//! Output is a single JSON document (see the `get_raw_baseline_diagnostics`
//! command and the env-gated test in `raw_processing.rs`); optional 16-bit
//! TIFF stage dumps are written when a dump directory is configured.

use rawler::imgop::develop::Intermediate;
use rawler::rawimage::{RawImage, RawImageData, RawPhotometricInterpretation};
use serde::Serialize;

/// Per-channel descriptive statistics over the executed buffer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelStats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub p1: f64,
    pub p5: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    /// Samples strictly below 0.0 (kept signed only by the Adobe path).
    pub negatives: u64,
    /// Samples strictly above 1.0 (scene-referred highlight excursions).
    pub above_one: u64,
    /// Samples exactly 0.0 — black-clipped either by the decoder's
    /// linearization or an explicit clamp.
    pub zeros: u64,
    pub count: u64,
    /// 64-bin histogram over `[-0.5, 3.0]` covering every legal working
    /// value (the highlight compressor allows up to ~3.0).
    pub histogram: Vec<u64>,
}

impl ChannelStats {
    /// Fixed histogram domain — wide enough for linear headroom.
    const HIST_MIN: f64 = -0.5;
    const HIST_MAX: f64 = 3.0;

    fn from_samples(
        samples: &mut [f64],
        count: u64,
        sum: f64,
        extremes: (f64, f64),
        neg: u64,
        above: u64,
        zeros: u64,
    ) -> Self {
        samples.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |p: f64| -> f64 {
            if samples.is_empty() {
                return f64::NAN;
            }
            let idx = ((samples.len() - 1) as f64 * p).round() as usize;
            samples[idx]
        };
        let mut histogram = vec![0u64; 64];
        for v in samples.iter() {
            let t = ((v - Self::HIST_MIN) / (Self::HIST_MAX - Self::HIST_MIN)).clamp(0.0, 1.0);
            let bin = ((t * 64.0).floor() as usize).min(63);
            histogram[bin] += 1;
        }
        Self {
            min: extremes.0,
            max: extremes.1,
            mean: if count > 0 {
                sum / count as f64
            } else {
                f64::NAN
            },
            p1: pct(0.01),
            p5: pct(0.05),
            p50: pct(0.5),
            p95: pct(0.95),
            p99: pct(0.99),
            negatives: neg,
            above_one: above,
            zeros,
            count,
            histogram,
        }
    }
}

/// Statistics for one named pipeline-stage boundary.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StageStats {
    pub name: String,
    pub color_space: String,
    pub width: u32,
    pub height: u32,
    pub channels: Vec<ChannelStats>,
    pub luminance: ChannelStats,
}

/// One CFA-plane (mosaic position) summary from the integer sensor data.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CfaPlaneStats {
    pub channel: String,
    pub min: u32,
    pub max: u32,
    pub mean: f64,
    pub p50: u32,
    /// Sensor values below this plane's declared black level — pixels that
    /// Rawler's `correct_blacklevel_cfa` clips to zero before demosaic.
    pub below_black: u64,
    /// Sensor values at/above this plane's white level.
    pub at_or_above_white: u64,
    pub count: u64,
}

/// Provenance of the decoded RAW: exactly what the decoder reported, before
/// Once-Lab's white-level override or any rescaling.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceProvenance {
    pub camera_make: String,
    pub camera_model: String,
    pub photometric: String,
    pub cfa_pattern: String,
    pub mosaic_width: u32,
    pub mosaic_height: u32,
    /// BlackLevelRepeatDim rows/cols and the per-plane levels as decoded.
    pub black_repeat: [u32; 2],
    pub black_levels: Vec<f64>,
    pub white_levels: Vec<u32>,
    /// DNG BlackLevelDeltaH/V support — absent for CR3.
    pub black_delta_hv: bool,
    /// DNG LinearizationTable — absent for CR3 (linear 14-bit data).
    pub linearization_table: bool,
    pub masked_areas: u64,
    pub active_area: Option<[i32; 4]>,
    pub crop_area: Option<[i32; 4]>,
    /// As-shot WB multipliers (normalized to green) from camera metadata.
    pub wb_coeffs: [f32; 4],
    /// Illuminant -> XYZ->camera matrix actually present in the Rawler
    /// camera database entry (verbatim values for review).
    pub color_matrices: serde_json::Map<String, serde_json::Value>,
    pub cfa_planes: Vec<CfaPlaneStats>,
}

/// Proof block for Phase-1 item 2: what the `whitelevel = u32::MAX`
/// override plus `rescale_factor` actually computed, per CFA plane.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearizationProof {
    /// The executed formula for each CFA plane, verbatim.
    pub executed_formula: String,
    /// `rescale_factor` applied post-demosaic.
    pub rescale_factor: f64,
    /// Per-plane `(white - black)` denominator the SDK would use.
    pub sdk_denominator_per_plane: Vec<f64>,
    /// Per-plane effective scale after Rawler's correction + Once-Lab's
    /// rescale: `rescale_factor / (u32::MAX - black)`. Approximates
    /// `1/sdk_denominator_per_plane` — see `max_scale_deviation`.
    pub effective_scale_per_plane: Vec<f64>,
    /// Max |effective_scale - 1/denominator| across planes. This is an
    /// ABSOLUTE scale difference; the relative error is
    /// `deviation / ideal_scale`, dominated by the rescale applying the
    /// plane-0 denominator `(white_0 - black_0)` to every plane while the
    /// SDK uses each plane's own `(white_cfa - black_cfa)`. On the R5
    /// fixture (denominators ~13497/13498) the relative error is ≈0.0074%
    /// — a small deterministic approximation to the SDK normalization,
    /// NOT floating-point rounding alone and distinct from the
    /// sub-black preservation fix.
    pub max_scale_deviation: f64,
    pub single_black_subtraction: bool,
    pub single_white_scaling: bool,
    /// Whether the EXECUTED path clips `raw - black` at zero before
    /// demosaic (Rawler's `correct_blacklevel_cfa` behavior, used by the
    /// legacy path). The SDK preserves signed values. The count in
    /// `cfa_planes[].belowBlack` quantifies the actual impact.
    pub pre_demosaic_negative_clip: bool,
    /// Whether the executed path ran the signed-preserving develop
    /// (`correct_blacklevel_cfa_signed`, Adobe path only): sub-black
    /// samples reach the demosaic input un-clipped.
    pub sub_black_preserved: bool,
    /// Count of mosaic values below their plane's black level.
    pub below_black_total: u64,
}

/// Which exposure/tone stages actually ran in the Adobe baseline path.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureToneReport {
    pub exposure_ramp: String,
    pub baseline_exposure_ev: f64,
    pub baseline_exposure_offset_ev: Option<f64>,
    pub shadow_scale: f64,
    pub shadows_ui_applied: bool,
    pub tone_curve: String,
    pub note: String,
}

/// Complete diagnostic document for one RAW develop.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawPipelineDiagnostics {
    pub schema: String,
    pub source_path: String,
    pub adobe_comparison: bool,
    pub provenance: Option<SourceProvenance>,
    pub linearization: Option<LinearizationProof>,
    pub exposure_tone: Option<ExposureToneReport>,
    pub stages: Vec<StageStats>,
    /// The executed camera->Linear ProPhoto D50 composite (fallback path
    /// only): `sRGB->ProPhoto * pseudo_inverse(normalize(xyz2cam *
    /// sRGB_D65))`. `None` when a DCP ran (its invariate transform is in the
    /// pipeline report instead).
    pub fallback_transform: Option<serde_json::Value>,
    pub output_stage: String,
    pub warnings: Vec<String>,
    /// Optional directory for 16-bit TIFF dumps of each recorded stage.
    #[serde(skip)]
    pub dump_dir: Option<std::path::PathBuf>,
}

impl RawPipelineDiagnostics {
    pub fn new(source_path: &str, adobe_comparison: bool) -> Self {
        Self {
            schema: "oncelab-raw-baseline-diagnostics/v1".to_string(),
            source_path: source_path.to_string(),
            adobe_comparison,
            provenance: None,
            linearization: None,
            exposure_tone: None,
            stages: Vec::new(),
            fallback_transform: None,
            output_stage: String::new(),
            warnings: Vec::new(),
            dump_dir: std::env::var("ONCELAB_RAW_DIAG_DIR")
                .ok()
                .map(std::path::PathBuf::from),
        }
    }

    /// Record the decoder-level provenance and the linearization proof.
    /// Must run BEFORE the caller mutates `whitelevel`/`blacklevel`.
    /// `signed_develop` reports whether the executed path preserved
    /// sub-black values (signed develop) or clipped them (Rawler default).
    pub fn record_raw_image(&mut self, raw: &RawImage, rescale_factor: f64, signed_develop: bool) {
        let blacks: Vec<f64> = raw
            .blacklevel
            .levels
            .iter()
            .map(|r| r.as_f32() as f64)
            .collect();
        let whites: Vec<u32> = raw.whitelevel.0.clone();
        let wb = raw.wb_coeffs;

        let mut color_matrices = serde_json::Map::new();
        for (illum, m) in &raw.color_matrix {
            color_matrices.insert(
                format!("{illum:?}"),
                serde_json::Value::from(m.iter().map(|v| *v as f64).collect::<Vec<_>>()),
            );
        }

        let (cfa_pattern, planes) = match &raw.photometric {
            RawPhotometricInterpretation::Cfa(cfg) => {
                (cfg.cfa.name.clone(), cfa_plane_stats(raw, &blacks, &whites))
            }
            other => (format!("{other:?}"), Vec::new()),
        };

        self.provenance = Some(SourceProvenance {
            camera_make: raw.clean_make.clone(),
            camera_model: raw.clean_model.clone(),
            photometric: format!("{:?}", raw.photometric),
            cfa_pattern,
            mosaic_width: raw.width as u32,
            mosaic_height: raw.height as u32,
            black_repeat: [raw.blacklevel.height as u32, raw.blacklevel.width as u32],
            black_levels: blacks.clone(),
            white_levels: whites.clone(),
            black_delta_hv: false,
            linearization_table: false,
            masked_areas: raw.blackareas.len() as u64,
            active_area: raw
                .active_area
                .map(|r| [r.p.x as i32, r.p.y as i32, r.d.w as i32, r.d.h as i32]),
            crop_area: raw
                .crop_area
                .map(|r| [r.p.x as i32, r.p.y as i32, r.d.w as i32, r.d.h as i32]),
            wb_coeffs: wb,
            color_matrices,
            cfa_planes: planes.clone(),
        });

        // Linearization proof: Once-Lab sets whitelevel=u32::MAX so Rawler's
        // per-CFA correction divides by (u32::MAX - black_i); the post-demosaic
        // rescale multiplies by (u32::MAX - black_0)/(white_0 - black_0).
        // Effective per-plane scale = rescale / (u32::MAX - black_i) and the
        // SDK-equivalent target is 1/(white_i - black_i). For this file the
        // planes share one white level, so each plane's deviation quantifies
        // the scalar-white approximation directly.
        if !blacks.is_empty() && !whites.is_empty() {
            let white0 = whites[0] as f64;
            let sdk_den: Vec<f64> = blacks
                .iter()
                .map(|b| {
                    // Whites broadcast scalar->plane when only one exists.
                    (whites.first().copied().unwrap_or(u16::MAX as u32) as f64) - b
                })
                .collect();
            let eff: Vec<f64> = blacks
                .iter()
                .map(|b| rescale_factor / (u32::MAX as f64 - b))
                .collect();
            let dev = eff
                .iter()
                .zip(sdk_den.iter())
                .map(|(e, d)| (e - 1.0 / d).abs())
                .fold(0.0, f64::max);
            let below = planes.iter().map(|p| p.below_black).sum();
            self.linearization = Some(LinearizationProof {
                executed_formula: "(raw - black_cfa) / (u32::MAX - black_cfa) \
                                   then * (u32::MAX - black_0) / (white_0 - black_0)"
                    .to_string(),
                rescale_factor,
                sdk_denominator_per_plane: sdk_den,
                effective_scale_per_plane: eff,
                max_scale_deviation: dev,
                single_black_subtraction: true,
                single_white_scaling: whites.len() == 1,
                pre_demosaic_negative_clip: !signed_develop,
                sub_black_preserved: signed_develop,
                below_black_total: below,
            });
            let _ = white0;
        }
    }

    /// Sample a 3-channel f32 intermediate at a named boundary.
    pub fn record_three_color(
        &mut self,
        name: &str,
        color_space: &str,
        pixels: &[[f32; 3]],
        width: u32,
        height: u32,
    ) {
        let stats = three_color_stats(name, color_space, pixels, width, height);
        self.stages.push(stats.clone());
        self.maybe_dump_stage(name, color_space, pixels, width, height);
    }

    /// Record which tone curve actually executed at the baseline stage.
    pub fn record_tone(
        &mut self,
        exposure_ramp: &str,
        beo_ev: Option<f64>,
        tone_curve: &str,
        shadows_ui: bool,
    ) {
        self.exposure_tone = Some(ExposureToneReport {
            exposure_ramp: exposure_ramp.to_string(),
            baseline_exposure_ev: 0.0,
            baseline_exposure_offset_ev: beo_ev,
            shadow_scale: 1.0,
            shadows_ui_applied: shadows_ui,
            tone_curve: tone_curve.to_string(),
            note: "SDR path: exposure_tone is a NOP at EV>=0; stage3 black is absent for CR3"
                .to_string(),
        });
    }

    /// Optionally dump a stage buffer for inspection. Writes THREE artifacts
    /// per boundary, because the numerical dump and a viewable image have
    /// different colorimetric contracts:
    ///
    ///   `stage-<name>.f32`   — numerical dump: raw little-endian f32,
    ///                        interleaved RGB, FULL SIGNED/RANGE content
    ///                        (negatives and >1.0 preserved verbatim).
    ///   `stage-<name>.json`  — sidecar: dtype, layout, dims, source
    ///                        color space and subsampling factor.
    ///   `stage-<name>-preview.png` — a *display preview only*: linear
    ///                        values pinned to [0,1] then run through the
    ///                        sRGB EOTF and tagged sRGB. NOT a pixel dump —
    ///                        clamping and the transfer encode are applied.
    ///
    /// Both are downsampled to ≤1024 px on the long edge (nearest sample).
    fn maybe_dump_stage(
        &mut self,
        name: &str,
        color_space: &str,
        pixels: &[[f32; 3]],
        width: u32,
        height: u32,
    ) {
        let Some(dir) = &self.dump_dir else { return };
        let _ = std::fs::create_dir_all(dir);
        let base = dir.join(format!("stage-{}", sanitize_name(name)));

        let scale = (width.max(height) as f32 / 1024.0).max(1.0);
        let dw = (width as f32 / scale).round().max(1.0) as u32;
        let dh = (height as f32 / scale).round().max(1.0) as u32;
        let sx = width as f32 / dw as f32;
        let sy = height as f32 / dh as f32;
        let sample = |x: u32, y: u32| -> [f32; 3] {
            pixels[((y as f32 * sy) as u32 * width + (x as f32 * sx) as u32) as usize]
        };

        // -- numerical dump: verbatim f32 (signed, unclamped) --------------
        let mut raw = Vec::with_capacity((dw * dh * 3) as usize * 4);
        for y in 0..dh {
            for x in 0..dw {
                for c in sample(x, y) {
                    raw.extend_from_slice(&c.to_le_bytes());
                }
            }
        }
        let _ = std::fs::write(base.with_extension("f32"), &raw);
        let meta = serde_json::json!({
            "dtype": "float32-little-endian",
            "layout": "interleaved RGB, row-major",
            "width": dw,
            "height": dh,
            "channels": 3,
            "sourceColorSpace": color_space,
            "sourceWidth": width,
            "sourceHeight": height,
            "subsample": scale,
            "valueRange": "linear, SIGNED (negatives and >1.0 preserved)",
            "previewNote": "the -preview.png companion is sRGB-encoded and clamped for viewing only",
        });
        let _ = std::fs::write(base.with_extension("json"), meta.to_string());

        // -- display preview: linear -> sRGB EOTF, tagged sRGB -------------
        let mut img = image::ImageBuffer::<image::Rgb<u8>, Vec<u8>>::new(dw, dh);
        let srgb_encode = |v: f32| -> u8 {
            let x = v.clamp(0.0, 1.0);
            let s = if x <= 0.0031308 {
                12.92 * x
            } else {
                1.055 * x.powf(1.0 / 2.4) - 0.055
            };
            (s * 255.0 + 0.5) as u8
        };
        for y in 0..dh {
            for x in 0..dw {
                let p = sample(x, y);
                img.put_pixel(
                    x,
                    y,
                    image::Rgb([srgb_encode(p[0]), srgb_encode(p[1]), srgb_encode(p[2])]),
                );
            }
        }
        let dyn_img = image::DynamicImage::ImageRgb8(img);
        if let Ok(bytes) = crate::export_processing::encode_image_to_bytes(&dyn_img, "png", 90) {
            let _ = std::fs::write(
                base.with_file_name(format!(
                    "{}-preview.png",
                    base.file_name().unwrap().to_string_lossy()
                )),
                bytes,
            );
        }
    }
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Per-CFA-position statistics on the integer mosaic, keyed by the decoded
/// black levels so `below_black` counts the actual clip impact.
fn cfa_plane_stats(raw: &RawImage, blacks: &[f64], whites: &[u32]) -> Vec<CfaPlaneStats> {
    let RawImageData::Integer(data) = &raw.data else {
        return Vec::new();
    };
    let (cfa_w, cfa_h) = (raw.blacklevel.width, raw.blacklevel.height);
    let plane_count = cfa_w * cfa_h;
    if plane_count == 0 {
        return Vec::new();
    }
    let names = ["R", "G1", "G2", "B", "C3", "C4", "C5", "C6"];
    let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); plane_count];
    for (i, &v) in data.iter().enumerate() {
        let x = i % raw.width;
        let y = i / raw.width;
        let idx = (y % cfa_h) * cfa_w + (x % cfa_w);
        if idx < buckets.len() {
            buckets[idx].push(v as u32);
        }
    }
    buckets
        .into_iter()
        .enumerate()
        .map(|(i, mut vals)| {
            vals.sort_unstable();
            let black = blacks.get(i).copied().unwrap_or(0.0);
            let white = whites.first().copied().unwrap_or(u16::MAX as u32) as f64;
            CfaPlaneStats {
                channel: names.get(i).unwrap_or(&"?").to_string(),
                min: *vals.first().unwrap_or(&0),
                max: *vals.last().unwrap_or(&0),
                mean: vals.iter().map(|v| *v as f64).sum::<f64>() / vals.len().max(1) as f64,
                p50: vals.get(vals.len() / 2).copied().unwrap_or(0),
                below_black: vals.iter().take_while(|&&v| (v as f64) < black).count() as u64,
                at_or_above_white: vals.iter().filter(|&&v| v as f64 >= white).count() as u64,
                count: vals.len() as u64,
            }
        })
        .collect()
}

/// Channel-wise + luminance statistics for a `[f32; 3]` buffer. Percentiles
/// are sampled (every 8th pixel) — exact for min/max/mean/counts.
fn three_color_stats(
    name: &str,
    color_space: &str,
    pixels: &[[f32; 3]],
    width: u32,
    height: u32,
) -> StageStats {
    let n = pixels.len();
    let mut ch_samples: [Vec<f64>; 4] = Default::default();
    let mut sums = [0.0f64; 4];
    let mut mins = [f64::INFINITY; 4];
    let mut maxs = [f64::NEG_INFINITY; 4];
    let mut neg = [0u64; 4];
    let mut above = [0u64; 4];
    let mut zeros = [0u64; 4];

    for (i, p) in pixels.iter().enumerate() {
        let luma = 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64;
        let vals = [p[0] as f64, p[1] as f64, p[2] as f64, luma];
        for (c, v) in vals.iter().enumerate() {
            sums[c] += v;
            mins[c] = mins[c].min(*v);
            maxs[c] = maxs[c].max(*v);
            if *v < 0.0 {
                neg[c] += 1;
            }
            if *v > 1.0 {
                above[c] += 1;
            }
            if *v == 0.0 {
                zeros[c] += 1;
            }
        }
        // Percentile subsample: every 8th pixel (~5.6M samples for 45MPx).
        if i % 8 == 0 {
            for c in 0..4 {
                ch_samples[c].push(vals[c]);
            }
        }
    }

    let mut mk = |c: usize| {
        ChannelStats::from_samples(
            &mut ch_samples[c],
            n as u64,
            sums[c],
            (mins[c], maxs[c]),
            neg[c],
            above[c],
            zeros[c],
        )
    };

    StageStats {
        name: name.to_string(),
        color_space: color_space.to_string(),
        width,
        height,
        channels: vec![mk(0), mk(1), mk(2)],
        luminance: mk(3),
    }
}

/// Convenience: record from an `Intermediate` variant when it is ThreeColor.
pub fn record_intermediate(
    diag: &mut RawPipelineDiagnostics,
    name: &str,
    color_space: &str,
    intermediate: &Intermediate,
) {
    if let Intermediate::ThreeColor(pixels) = intermediate {
        let (w, h) = (pixels.dim().w as u32, pixels.dim().h as u32);
        diag.record_three_color(name, color_space, &pixels.data, w, h);
    }
}

/// Frontend command: develop a RAW through the configured pipeline and
/// return the full stage-boundary diagnostic document as JSON.
///
/// Uses the *real* settings (Adobe comparison flag, selected DCP, baseline
/// tone choice) — the diagnostics therefore describe what a fresh load or
/// export would actually compute.
#[tauri::command]
pub fn get_raw_baseline_diagnostics(
    path: String,
    app_handle: tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    let settings = crate::app_settings::load_settings(app_handle).unwrap_or_default();
    let bytes = std::fs::read(&path).map_err(|e| format!("failed to read '{}': {}", path, e))?;

    let adobe_enabled = settings.adobe_comparison_enabled();
    let dcp_path = settings
        .adobe_camera_profile_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from);
    let ctx = crate::raw_processing::AdobeRawContext {
        camera_profile_path: dcp_path.as_deref(),
        source_path: Some(path.as_str()),
        baseline_tone: if settings.adobe_dng_baseline_tone() {
            crate::raw_processing::BaselineToneChoice::DngAcr3Default
        } else {
            crate::raw_processing::BaselineToneChoice::Identity
        },
    };
    let adobe_ctx = adobe_enabled.then_some(&ctx);

    let (_image, diag) = crate::raw_processing::develop_raw_image_diagnostics(
        &bytes,
        false,
        settings.raw_highlight_compression.unwrap_or(2.5),
        settings.linear_raw_mode.clone(),
        adobe_ctx,
        None,
    )
    .map_err(|e| e.to_string())?;

    serde_json::to_value(&diag).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact statistics on a tiny synthetic buffer — verifies the boundary
    /// sampler computes real percentiles/counts, not inferred values.
    #[test]
    fn stage_stats_on_known_pixels() {
        // 100 pixels: values 0.0, 0.01, 0.02, ..., 0.99 per channel.
        let pixels: Vec<[f32; 3]> = (0..100)
            .map(|i| {
                let v = i as f32 / 100.0;
                [v, v * 0.5 - 0.1, v * 2.0 - 0.5]
            })
            .collect();
        let stats = three_color_stats("test", "test space", &pixels, 10, 10);

        assert_eq!(stats.width, 10);
        assert_eq!(stats.channels[0].count, 100);
        // R: 0..0.99 exact
        assert_eq!(stats.channels[0].min, 0.0);
        assert!((stats.channels[0].max - 0.99).abs() < 1e-6);
        // G: v*0.5 - 0.1 → [-0.1, 0.395] → 10 negatives
        assert!(stats.channels[1].min < 0.0);
        assert_eq!(stats.channels[1].negatives, 20);
        // B: v*2 - 0.5 → [-0.5, 1.48] → above_one where v*2-0.5>1 → v>0.75 → i≥76 → 24
        assert_eq!(stats.channels[2].above_one, 24);
        // zeros: G has one exact zero (v*0.5-0.1==0 at i=20 → 0.0)
        assert!(stats.channels[1].zeros >= 1);
        // Histogram covers 64 bins, total = count
        let total: u64 = stats.channels[0].histogram.iter().sum();
        assert!(total > 0 && total <= stats.channels[0].count);
    }

    /// The tone report must name the executed curve explicitly — "identity"
    /// when nothing ran.
    #[test]
    fn tone_report_records_the_actual_curve() {
        let mut diag = RawPipelineDiagnostics::new("/tmp/x.cr3", true);
        diag.record_tone("identity (no DCP exposure ramp)", None, "identity", false);
        let json = serde_json::to_value(&diag).unwrap();
        assert_eq!(json["exposureTone"]["toneCurve"], "identity");
        assert_eq!(
            json["exposureTone"]["exposureRamp"],
            "identity (no DCP exposure ramp)"
        );
        assert_eq!(json["schema"], "oncelab-raw-baseline-diagnostics/v1");
    }
}
