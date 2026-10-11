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
pub(crate) use integration::{
    ProfileRenderMode, apply_profile_to_three_color_pixels,
    apply_profile_to_three_color_pixels_with_amount, apply_profile_to_three_color_pixels_with_mode,
    apply_profile_to_three_color_pixels_with_mode_and_space, profile_render_mode,
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
