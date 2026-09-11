mod apply;
mod integration;
mod parser;
mod rgb_table;

pub(crate) use apply::apply_rgb_table;
pub(crate) use integration::apply_profile_to_three_color_pixels;
pub(crate) use parser::{XmpRgbProfile, parse_xmp_rgb_profile};
pub(crate) use rgb_table::{RgbTable, decode_adobe_rgb_table};
