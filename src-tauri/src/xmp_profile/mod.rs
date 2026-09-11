mod apply;
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
