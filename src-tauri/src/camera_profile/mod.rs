//! File-backed Adobe DCP (DNG Camera Profile) support for the Adobe
//! comparison pipeline.
//!
//! [`dcp`] parses and validates a `.dcp` file, [`color_spec`] resolves the
//! white-balanced camera→PCS transform the way the public DNG SDK does, and
//! [`render`] executes the per-pixel camera-profile stages. Everything that
//! cannot be faithfully rendered is reported in [`DcpProfile::unsupported`]
//! (and in the [`AdobePipelineReport`]), never silently substituted.
//!
//! Nothing in this module ships, downloads, or fabricates Adobe's proprietary
//! camera profiles: the user supplies a `.dcp` they are licensed to use.

pub(crate) mod color_spec;
pub(crate) mod dcp;
pub(crate) mod render;
pub(crate) mod spline;
pub(crate) mod tiff;

pub(crate) use dcp::{CameraMatch, DcpProfile};
pub(crate) use render::{DcpRender, RenderReport};

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Loads a DCP from disk. Parse errors are fatal (returned), never degraded
/// into a partial profile.
pub(crate) fn load_dcp_from_path(path: &Path) -> Result<DcpProfile, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("cannot read camera profile '{}': {e}", path.display()))?;
    DcpProfile::parse(&bytes)
        .map_err(|e| format!("invalid camera profile '{}': {e}", path.display()))
}

/// UI-facing summary of one `.dcp` file (inspect-before-apply).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DcpSummary {
    pub path: String,
    pub profile_name: Option<String>,
    pub unique_camera_model: Option<String>,
    pub copyright: Option<String>,
    pub illuminants: Vec<String>,
    /// `ProfileEmbedPolicy` (informational: whether the profile permits
    /// embedding in DNG files; we never embed it).
    pub embed_policy: Option<u32>,
    pub has_forward_matrix: bool,
    pub has_hue_sat_map: bool,
    pub has_look_table: bool,
    pub has_tone_curve: bool,
    pub warnings: Vec<String>,
    pub unsupported: Vec<String>,
}

/// Inspects a `.dcp` without applying it — used by the file picker to show
/// what the profile contains and what it cannot express.
#[tauri::command]
pub fn inspect_camera_profile(path: String) -> Result<DcpSummary, String> {
    let dcp = load_dcp_from_path(Path::new(&path))?;
    Ok(DcpSummary {
        path,
        profile_name: dcp.profile_name.clone(),
        unique_camera_model: dcp.unique_camera_model.clone(),
        copyright: dcp.copyright.clone(),
        illuminants: dcp.illuminant_labels(),
        embed_policy: dcp.embed_policy,
        has_forward_matrix: dcp.matrices.iter().any(|m| m.forward_matrix.is_some()),
        has_hue_sat_map: dcp.hue_sat_maps.iter().any(|m| m.is_some()),
        has_look_table: dcp.look_table.is_some(),
        has_tone_curve: dcp.tone_curve.is_some(),
        warnings: dcp.warnings.clone(),
        unsupported: dcp.unsupported.clone(),
    })
}

// ------------------------------------------------------------ diagnostics

/// Which camera calibration ran for a develop.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CameraProfileChoice {
    /// A user-supplied DCP rendered the camera stages.
    Dcp {
        name: String,
        path: String,
        model_match: String,
    },
    /// No compatible DCP — Rawler's D65-preferred color matrix calibration
    /// with the unclipped ProPhoto conversion. NOT Adobe Standard parity.
    RawlerFallback { reason: String },
}

/// The Creative (XMP) profile part of the report.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreativeProfileReport {
    pub name: String,
    pub uuid: String,
    pub rgb_table_id: Option<String>,
    pub effective_amount: f32,
    pub gamut_policy: String,
}

/// The structured trace of one Adobe-comparison develop. Recorded per source
/// path so preview and export can be inspected after the fact.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdobePipelineReport {
    pub source_path: String,
    pub camera_make: String,
    pub camera_model: String,
    /// Rawler-decoded as-shot white balance multipliers.
    pub raw_wb_coeffs: [f32; 4],
    pub camera_profile: CameraProfileChoice,
    /// Camera-stage resolution details (only for the DCP path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render: Option<RenderReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creative_profile: Option<CreativeProfileReport>,
    /// Output-stage description (tonemapper mode + encoding).
    pub output_stage: String,
    /// Lens-correction status observed by the caller when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lens_correction: Option<String>,
    pub warnings: Vec<String>,
}

static REPORTS: OnceLock<Mutex<HashMap<String, AdobePipelineReport>>> = OnceLock::new();

fn reports() -> &'static Mutex<HashMap<String, AdobePipelineReport>> {
    REPORTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records (or replaces) the report for a source path. Also mirrors the
/// report to the structured log so headless runs keep the same trace.
pub(crate) fn record_adobe_report(report: AdobePipelineReport) {
    log::info!(
        "Adobe comparison pipeline report: {}",
        serde_json::to_string(&report).unwrap_or_else(|_| "<unserializable>".to_string())
    );
    if let Ok(mut guard) = reports().lock() {
        // Keep the map bounded: this is a diagnostic, not a cache.
        if guard.len() > 64 {
            guard.clear();
        }
        guard.insert(report.source_path.clone(), report);
    }
}

/// Returns the report recorded for a source path, if any. With no argument,
/// returns the most recently recorded report.
#[tauri::command]
pub fn get_adobe_pipeline_report(
    path: Option<String>,
) -> Result<Option<AdobePipelineReport>, String> {
    let guard = reports()
        .lock()
        .map_err(|_| "report store poisoned".to_string())?;
    match path {
        Some(p) => Ok(guard.get(&p).cloned()),
        None => Ok(guard.values().next().cloned()),
    }
}

/// Summarizes the lens-correction state from the same adjustments JSON the
/// renderer consumes: which correction ran (lensfun-resolved params vs manual
/// amounts vs none) and at what strength.
pub(crate) fn lens_status_from_adjustments(adjustments: &serde_json::Value) -> String {
    let mode = adjustments
        .get("lensCorrectionMode")
        .and_then(|v| v.as_str())
        .unwrap_or("manual");
    let enabled = adjustments
        .get("lensDistortionEnabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let amount = adjustments
        .get("lensDistortionAmount")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let has_params = adjustments
        .get("lensDistortionParams")
        .is_some_and(|v| !v.is_null());
    let maker = adjustments
        .get("lensMaker")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let model = adjustments
        .get("lensModel")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !enabled || amount == 0.0 {
        "disabled".to_string()
    } else if has_params {
        let lens = [maker, model]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        if lens.is_empty() {
            format!("lensfun params, distortion {amount:.0}%")
        } else {
            format!("lensfun ({lens}), distortion {amount:.0}%")
        }
    } else if mode == "auto" {
        "auto requested; no lensfun profile match".to_string()
    } else {
        format!("manual distortion {amount:.0}% (no lensfun params)")
    }
}

/// Updates the stored report's lens status for a source path, if a report
/// exists (called by the preview/export seams that own the adjustments).
pub(crate) fn annotate_lens_status(path: &str, status: String) {
    if let Ok(mut guard) = reports().lock()
        && let Some(report) = guard.get_mut(path)
    {
        report.lens_correction = Some(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lens_status_reports_disabled_when_off() {
        let adjustments = serde_json::json!({
            "lensCorrectionMode": "manual",
            "lensDistortionEnabled": false,
            "lensDistortionAmount": 100
        });
        assert_eq!(lens_status_from_adjustments(&adjustments), "disabled");
    }

    #[test]
    fn lens_status_reports_lensfun_match() {
        let adjustments = serde_json::json!({
            "lensCorrectionMode": "auto",
            "lensDistortionEnabled": true,
            "lensDistortionAmount": 100,
            "lensDistortionParams": {"k1": 0.1},
            "lensMaker": "Canon",
            "lensModel": "RF24-240mm"
        });
        let status = lens_status_from_adjustments(&adjustments);
        assert!(status.contains("lensfun"));
        assert!(status.contains("RF24-240mm"));
    }

    #[test]
    fn lens_status_reports_auto_without_match() {
        let adjustments = serde_json::json!({
            "lensCorrectionMode": "auto",
            "lensDistortionEnabled": true,
            "lensDistortionAmount": 100
        });
        let status = lens_status_from_adjustments(&adjustments);
        assert!(status.contains("no lensfun"), "{status}");
    }

    #[test]
    fn report_store_records_and_annotates() {
        let report = AdobePipelineReport {
            source_path: "/tmp/lens_test.cr3".to_string(),
            camera_make: "Canon".to_string(),
            camera_model: "EOS R5".to_string(),
            raw_wb_coeffs: [1.0, 1.0, 1.0, 1.0],
            camera_profile: CameraProfileChoice::RawlerFallback {
                reason: "test".to_string(),
            },
            render: None,
            creative_profile: None,
            output_stage: "test".to_string(),
            lens_correction: None,
            warnings: vec![],
        };
        record_adobe_report(report);
        annotate_lens_status("/tmp/lens_test.cr3", "disabled".to_string());
        let guard = reports().lock().unwrap();
        let stored = guard.get("/tmp/lens_test.cr3").unwrap();
        assert_eq!(stored.lens_correction.as_deref(), Some("disabled"));
    }
}
