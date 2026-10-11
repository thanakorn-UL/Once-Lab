use std::path::PathBuf;

use tauri::{AppHandle, Manager};

mod adobe_apply;
mod apply;
mod color_space;
pub(crate) mod default_roots;
pub(crate) mod discovery;
mod integration;
mod linear_rgb;
mod loader;
mod look_table;
mod look_table_apply;
mod parser;
mod rgb_table;
mod sampled_gamma;
mod srgb_transfer;
pub(crate) mod summary;

#[cfg(test)]
pub(crate) use apply::apply_rgb_table;
#[cfg(test)]
pub(crate) use integration::apply_profile_to_three_color_pixels;
pub(crate) use integration::{
    ProfileRenderMode, apply_profile_to_three_color_pixels_with_mode_space_and_tone,
    effective_profile_amount, profile_render_mode,
};
pub(crate) use loader::load_xmp_rgb_profile_from_path;
pub(crate) use parser::{XmpRgbProfile, parse_xmp_rgb_profile};
#[cfg(test)]
pub(crate) use rgb_table::RgbTable;

/// Returns the UI-facing summary (parsed profile name + path) for an XMP file.
#[tauri::command]
pub fn inspect_xmp_profile(path: String) -> Result<summary::XmpProfileSummary, String> {
    summary::inspect_xmp_profile(&path)
}

/// Discovers supported XMP RGB profiles under the caller-provided roots.
///
/// Roots are explicit: nothing is scanned implicitly, and no Adobe installation
/// directory is hardcoded here.
#[tauri::command]
pub fn discover_xmp_profiles(
    roots: Vec<String>,
) -> Result<Vec<discovery::XmpProfileEntry>, String> {
    let roots = roots.into_iter().map(PathBuf::from).collect::<Vec<_>>();
    discovery::discover_xmp_profiles(&roots)
}

/// Detects the Adobe Camera Raw profile folders that already exist on this
/// machine.
///
/// Read-only: only existing directories are returned, nothing is created and no
/// profile is scanned here. A returned root is meant to be handed to
/// [`discover_xmp_profiles`] like any other root, which keeps discovery in one
/// place.
#[tauri::command]
pub fn discover_default_xmp_profile_roots(
    app_handle: AppHandle,
) -> Result<Vec<default_roots::XmpProfileRootDescriptor>, String> {
    let home = app_handle
        .path()
        .home_dir()
        .map_err(|error| error.to_string())?;

    Ok(default_roots::existing_default_xmp_profile_roots(&home))
}

/// Camera-calibration path for Adobe mode: precompose Rawler's camera->linear
/// sRGB matrix with the D50-adapted ProPhoto transform. The RAW source then
/// enters the Adobe Profile stages *directly* in linear ProPhoto, preserving
/// signed out-of-sRGB-gamut coordinates without a per-pixel sRGB round-trip.
pub(crate) fn adobe_linear_srgb_to_prophoto_matrix() -> [[f32; 3]; 3] {
    look_table_apply::linear_srgb_to_linear_prophoto()
}

/// Inverse of [`adobe_linear_srgb_to_prophoto_matrix`], used by the profile-less
/// Adobe comparison baseline to hop back into the linear-sRGB working buffer
/// at the same boundary the profiled render uses.
pub(crate) fn adobe_linear_prophoto_to_srgb_matrix() -> [[f32; 3]; 3] {
    look_table_apply::linear_prophoto_to_linear_srgb()
}

/// `dng_space_ProPhoto::MatrixFromPCS` — XYZ(D50) -> linear ProPhoto(D50), in
/// the same derived construction (`Invert(SetMatrixToPCS(PROPHOTO_RAW))`) the
/// XMP boundary hops use. Needed by the camera-profile stage, whose
/// `CameraToPCS` output is XYZ(D50), to land in Linear ProPhoto D50.
pub(crate) fn adobe_xyz_to_linear_prophoto_matrix() -> [[f64; 3]; 3] {
    linear_rgb::invert3(linear_rgb::set_matrix_to_pcs(linear_rgb::PROPHOTO_RAW))
}

/// The shared hue/sat/value grid type; the DCP `ProfileHueSatMapData` and
/// `ProfileLookTableData` tags decode into the same layout.
pub(crate) use look_table::{ENCODING_LINEAR, ENCODING_SRGB, LookTable, MIN_SAT_DIVISIONS};
/// The per-pixel `dng_hue_sat_map` evaluator used by both the XMP LookTable
/// stage and (identically, `DoBaselineHueSatMap` in `dng_render.cpp`) the DCP
/// `ProfileHueSatMap`/`ProfileLookTable` stages.
pub(crate) use look_table_apply::LookTableAdobeContext;
