use std::path::PathBuf;

mod apply;
mod discovery;
mod integration;
mod loader;
mod parser;
mod rgb_table;
mod summary;

pub(crate) use apply::apply_rgb_table;
pub(crate) use integration::{
    apply_profile_to_three_color_pixels, apply_profile_to_three_color_pixels_with_amount,
};
pub(crate) use loader::load_xmp_rgb_profile_from_path;
pub(crate) use parser::{XmpRgbProfile, parse_xmp_rgb_profile};
pub(crate) use rgb_table::{RgbTable, decode_adobe_rgb_table};

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
