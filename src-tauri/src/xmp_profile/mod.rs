mod parser;
mod rgb_table;

pub(crate) use parser::{XmpRgbProfile, parse_xmp_rgb_profile};
pub(crate) use rgb_table::{RgbTable, decode_adobe_rgb_table};
