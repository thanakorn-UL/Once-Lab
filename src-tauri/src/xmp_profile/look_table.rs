//! Decoder for the Adobe `dng_look_table` ("LookTable") big-table payload.
//!
//! A "Look" XMP profile (ACR/Lightroom `crs:PresetType="Look"`) authors its colour
//! look as a `crs:LookTable` fingerprint plus a `crs:Table_<32hex>` attribute. That
//! attribute travels inside the same Base85 + zlib framing as the RGBTable, so the
//! wire plumbing lives once in
//! [`super::rgb_table::decode_big_table_payload`]; only the decompressed stream
//! differs. A LookTable is a hue/saturation/value grid, not an RGB lattice.
//!
//! Decompressed version-1 layout, all little-endian:
//!
//! ```text
//! u32 type              must be 0 (btt_LookTable)
//! u32 version           must be 1 (see "version" below)
//! u32 hueDivisions      1..=360
//! u32 satDivisions      2..=256
//! u32 valDivisions      1..=256
//! [ f32 hueShiftDeg, f32 satScale, f32 valScale ] * n
//!                       n = hueDivisions * satDivisions * valDivisions, n <= 18432
//! u32 encoding          0 = Linear, 1 = sRGB
//! u32 flags             optional; present only when exactly 4 bytes remain
//! ```
//!
//! The `n` entries are stored value-outermost, then hue, then saturation
//! innermost, so the entry for `(val, hue, sat)` sits at
//! `val * hueDivisions * satDivisions + hue * satDivisions + sat`. This ordering
//! is what Adobe's own index math in `dng_hue_sat_map` uses and what Fe's
//! 36x16x16 table confirms (`20 + 9216 * 12 + 4 = 110616` bytes exactly).
//!
//! Version 2 additionally stores `f64 minAmount, f64 maxAmount` after `encoding`;
//! those are the look's supported *amount* range (an ACR strength slider), not a
//! render clamp, and version 1 forces them to `1.0 / 1.0`. Once-Lab supports
//! version 1 only, so any other version fails explicitly instead of being
//! half-parsed.

use super::rgb_table::{decode_big_table_payload, read_f32, read_u32};

/// `BigTableTypeEnum::btt_LookTable`.
const LOOK_TABLE_TYPE: u32 = 0;

/// `dng_look_table::kLookTableVersion1`. Once-Lab deliberately supports only this
/// version: version 2 adds stored amounts that are not render parameters.
const LOOK_TABLE_VERSION: u32 = 1;

/// `dng_look_table::kMaxHueSamples`.
pub(super) const MAX_HUE_DIVISIONS: u32 = 360;
/// `dng_look_table::kMaxSatSamples`.
pub(super) const MAX_SAT_DIVISIONS: u32 = 256;
/// `dng_look_table::kMaxValSamples`.
pub(super) const MAX_VAL_DIVISIONS: u32 = 256;
/// `dng_look_table::kMaxTotalSamples` = 36 * 32 * 16.
pub(super) const MAX_TOTAL_SAMPLES: u32 = 18432;

/// Lower bounds mirror `dng_hue_sat_map::SetDivisions`, which Adobe calls while
/// reading the table: at least one hue division, at least two saturation
/// divisions, and a value-division count of zero folded up to one.
pub(super) const MIN_HUE_DIVISIONS: u32 = 1;
pub(super) const MIN_SAT_DIVISIONS: u32 = 2;
pub(super) const MIN_VAL_DIVISIONS: u32 = 1;

/// `encoding_Linear`.
pub(super) const ENCODING_LINEAR: u32 = 0;
/// `encoding_sRGB`.
pub(super) const ENCODING_SRGB: u32 = 1;

/// Label woven into every LookTable error string.
const LOOK_TABLE_LABEL: &str = "LookTable";

/// type + version + the three division counts.
const HEADER_BYTES: usize = 20;
/// Three `f32` per grid entry.
const ENTRY_BYTES: usize = 12;
const ENCODING_BYTES: usize = 4;
/// The optional trailing big-table flags word.
const FLAGS_BYTES: usize = 4;

/// Smallest legal grid: 1 hue x 2 sat x 1 val.
const MIN_SAMPLES: usize = (MIN_HUE_DIVISIONS * MIN_SAT_DIVISIONS * MIN_VAL_DIVISIONS) as usize;

/// Smallest legal decompressed block (a minimal version-1 table).
const MIN_BLOCK_BYTES: usize = HEADER_BYTES + MIN_SAMPLES * ENTRY_BYTES + ENCODING_BYTES;

/// Largest legal decompressed block (18432 samples plus the optional flags word).
const MAX_BLOCK_BYTES: usize = HEADER_BYTES
    + (MAX_TOTAL_SAMPLES as usize) * ENTRY_BYTES
    + ENCODING_BYTES
    + FLAGS_BYTES;

/// A decoded Adobe LookTable (a hue/saturation/value mapping grid).
///
/// The table is the raw, validated wire content: it carries no amount, because a
/// version-1 LookTable is always applied at full strength (Adobe forces the
/// stored `min`/`max` amounts to `1.0 / 1.0` for version 1).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LookTable {
    pub hue_divisions: u32,
    pub sat_divisions: u32,
    pub val_divisions: u32,
    /// Entries in Adobe wire order (value outermost, hue middle, saturation
    /// innermost). Each entry is `[hueShiftDegrees, satScale, valScale]`.
    pub entries: Vec<[f32; 3]>,
    /// `0 = Linear`, `1 = sRGB`. For `sRGB`, only the `V` coordinate is transfer
    /// encoded before the lookup and decoded after it.
    pub encoding: u32,
    /// The generic big-table flags word, if the stream carried one. Adobe defines
    /// only bit 0 ("embed never"), which affects file writing, never rendering.
    pub flags: Option<u32>,
}

/// Decodes a `crs:Table_<hex>` LookTable attribute value.
///
/// Structural validation (type, version, division bounds, sample total, exact
/// length, encoding value and entry finiteness) happens here, so the renderer can
/// trust a successfully decoded [`LookTable`].
pub(crate) fn decode_adobe_look_table(encoded: &str) -> Result<LookTable, String> {
    let block = decode_big_table_payload(encoded, LOOK_TABLE_LABEL, MIN_BLOCK_BYTES, MAX_BLOCK_BYTES)?;

    parse_look_table_block(&block)
}

fn parse_look_table_block(block: &[u8]) -> Result<LookTable, String> {
    let table_type = read_u32(block, 0, LOOK_TABLE_LABEL)?;
    let version = read_u32(block, 4, LOOK_TABLE_LABEL)?;
    let hue_divisions = read_u32(block, 8, LOOK_TABLE_LABEL)?;
    let sat_divisions = read_u32(block, 12, LOOK_TABLE_LABEL)?;
    let val_divisions = read_u32(block, 16, LOOK_TABLE_LABEL)?;

    if table_type != LOOK_TABLE_TYPE {
        return Err(format!(
            "unsupported embedded LookTable type {table_type}, expected {LOOK_TABLE_TYPE}"
        ));
    }

    // Reject version 2 (and anything else) before touching the entry block: a
    // version-2 stream carries two extra f64 amounts we do not support, so
    // parsing it as if it were version 1 would silently mis-read every entry.
    if version != LOOK_TABLE_VERSION {
        return Err(format!(
            "unsupported embedded LookTable version {version}, expected {LOOK_TABLE_VERSION}"
        ));
    }

    validate_divisions(hue_divisions, sat_divisions, val_divisions)?;

    let total = u64::from(hue_divisions) * u64::from(sat_divisions) * u64::from(val_divisions);
    let sample_count = total as usize;

    let entry_bytes = sample_count
        .checked_mul(ENTRY_BYTES)
        .ok_or_else(|| "embedded LookTable sample byte count overflows usize".to_string())?;

    let expected_block_size = HEADER_BYTES
        .checked_add(entry_bytes)
        .and_then(|base| base.checked_add(ENCODING_BYTES))
        .ok_or_else(|| "embedded LookTable block size overflows usize".to_string())?;

    // The stream is either exactly the version-1 body, or that body plus the
    // optional flags word. Anything else is corrupt; trailing garbage must never
    // be silently ignored.
    let flags_present = if block.len() == expected_block_size {
        false
    } else if block.len() == expected_block_size + FLAGS_BYTES {
        true
    } else {
        return Err(format!(
            "embedded LookTable block size mismatch: expected {expected_block_size} or {} bytes, got {} bytes",
            expected_block_size + FLAGS_BYTES,
            block.len()
        ));
    };

    let mut entries = Vec::with_capacity(sample_count);
    let mut offset = HEADER_BYTES;

    for index in 0..sample_count {
        let hue_shift = read_f32(block, offset, LOOK_TABLE_LABEL)?;
        let sat_scale = read_f32(block, offset + 4, LOOK_TABLE_LABEL)?;
        let val_scale = read_f32(block, offset + 8, LOOK_TABLE_LABEL)?;
        offset += ENTRY_BYTES;

        // A NaN/Inf entry would poison every pixel that samples it, so reject the
        // whole table rather than deferring to the renderer.
        if !hue_shift.is_finite() || !sat_scale.is_finite() || !val_scale.is_finite() {
            return Err(format!("embedded LookTable entry {index} is not finite"));
        }

        entries.push([hue_shift, sat_scale, val_scale]);
    }

    let encoding = read_u32(block, offset, LOOK_TABLE_LABEL)?;

    if encoding != ENCODING_LINEAR && encoding != ENCODING_SRGB {
        return Err(format!(
            "unsupported embedded LookTable encoding {encoding}, expected {ENCODING_LINEAR} or {ENCODING_SRGB}"
        ));
    }

    let flags = if flags_present {
        Some(read_u32(block, expected_block_size, LOOK_TABLE_LABEL)?)
    } else {
        None
    };

    Ok(LookTable {
        hue_divisions,
        sat_divisions,
        val_divisions,
        entries,
        encoding,
        flags,
    })
}

/// Enforces the same division limits Adobe's `dng_look_table::GetStream` does,
/// including the lower bounds `dng_hue_sat_map::SetDivisions` adds.
fn validate_divisions(hue: u32, sat: u32, val: u32) -> Result<(), String> {
    if !(MIN_HUE_DIVISIONS..=MAX_HUE_DIVISIONS).contains(&hue) {
        return Err(format!(
            "embedded LookTable hue divisions {hue} out of supported range {MIN_HUE_DIVISIONS}..={MAX_HUE_DIVISIONS}"
        ));
    }

    if !(MIN_SAT_DIVISIONS..=MAX_SAT_DIVISIONS).contains(&sat) {
        return Err(format!(
            "embedded LookTable sat divisions {sat} out of supported range {MIN_SAT_DIVISIONS}..={MAX_SAT_DIVISIONS}"
        ));
    }

    if !(MIN_VAL_DIVISIONS..=MAX_VAL_DIVISIONS).contains(&val) {
        return Err(format!(
            "embedded LookTable val divisions {val} out of supported range {MIN_VAL_DIVISIONS}..={MAX_VAL_DIVISIONS}"
        ));
    }

    // Each dimension is in range individually, yet the product can still exceed
    // the SDK's flat 18432-sample ceiling (e.g. 360 x 256 x 2).
    let total = u64::from(hue) * u64::from(sat) * u64::from(val);

    if total > u64::from(MAX_TOTAL_SAMPLES) {
        return Err(format!(
            "embedded LookTable total samples {total} exceed supported maximum {MAX_TOTAL_SAMPLES}"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp_profile::rgb_table::ADOBE_BASE85_ALPHABET;

    fn encode_group(out: &mut String, bytes: &[u8]) {
        let mut value = 0u32;
        for (index, byte) in bytes.iter().enumerate() {
            value += u32::from(*byte) << (8 * index);
        }

        let characters = bytes.len() + 1;
        for _ in 0..characters {
            let digit = (value % 85) as usize;
            out.push(ADOBE_BASE85_ALPHABET[digit] as char);
            value /= 85;
        }
    }

    fn base85_encode(data: &[u8]) -> String {
        let mut out = String::new();
        let full = data.len() - (data.len() % 4);

        for chunk in data[..full].chunks(4) {
            encode_group(&mut out, chunk);
        }

        let remainder = &data[full..];
        if !remainder.is_empty() {
            encode_group(&mut out, remainder);
        }

        out
    }

    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write;

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).expect("test zlib compression");
        encoder.finish().expect("test zlib finish")
    }

    fn encode_payload(declared_size: u32, body: &[u8]) -> String {
        let mut payload = declared_size.to_le_bytes().to_vec();
        payload.extend_from_slice(&zlib_compress(body));
        base85_encode(&payload)
    }

    /// Builds a version-1 LookTable body with `entries` in Adobe wire order.
    fn build_body(hue: u32, sat: u32, val: u32, entries: &[[f32; 3]], encoding: u32) -> Vec<u8> {
        build_body_versioned(hue, sat, val, entries, encoding, LOOK_TABLE_VERSION)
    }

    fn build_body_versioned(
        hue: u32,
        sat: u32,
        val: u32,
        entries: &[[f32; 3]],
        encoding: u32,
        version: u32,
    ) -> Vec<u8> {
        let mut block = Vec::new();
        block.extend_from_slice(&LOOK_TABLE_TYPE.to_le_bytes());
        block.extend_from_slice(&version.to_le_bytes());
        block.extend_from_slice(&hue.to_le_bytes());
        block.extend_from_slice(&sat.to_le_bytes());
        block.extend_from_slice(&val.to_le_bytes());
        for entry in entries {
            block.extend_from_slice(&entry[0].to_le_bytes());
            block.extend_from_slice(&entry[1].to_le_bytes());
            block.extend_from_slice(&entry[2].to_le_bytes());
        }
        block.extend_from_slice(&encoding.to_le_bytes());
        block
    }

    /// A distinct, recognizable entry per grid cell: hue shift by hue, sat scale
    /// by sat, value scale by val. Lets a test see exactly which cell was read.
    fn cell_entries(hue: u32, sat: u32, val: u32) -> Vec<[f32; 3]> {
        let mut entries = Vec::new();
        for v in 0..val {
            for h in 0..hue {
                for s in 0..sat {
                    entries.push([h as f32, 1.0 + s as f32, 1.0 + v as f32]);
                }
            }
        }
        entries
    }

    fn assert_close(actual: f32, expected: f32, epsilon: f32, context: &str) {
        assert!(
            (actual - expected).abs() < epsilon,
            "{context}: expected {expected}, got {actual}"
        );
    }

    #[test]
    fn decodes_synthetic_look_table_with_exact_entries() {
        let (hue, sat, val) = (3u32, 2u32, 2u32);
        let entries = cell_entries(hue, sat, val);
        assert_eq!(entries.len(), 12);

        let body = build_body(hue, sat, val, &entries, ENCODING_LINEAR);
        let encoded = encode_payload(body.len() as u32, &body);

        let table = decode_adobe_look_table(&encoded).expect("synthetic table should decode");

        assert_eq!(table.hue_divisions, 3);
        assert_eq!(table.sat_divisions, 2);
        assert_eq!(table.val_divisions, 2);
        assert_eq!(table.encoding, ENCODING_LINEAR);
        assert_eq!(table.flags, None);
        assert_eq!(table.entries.len(), 12);

        for (index, expected) in entries.iter().enumerate() {
            let actual = table.entries[index];
            assert_close(actual[0], expected[0], 1e-6, &format!("entry {index} hue shift"));
            assert_close(actual[1], expected[1], 1e-6, &format!("entry {index} sat scale"));
            assert_close(actual[2], expected[2], 1e-6, &format!("entry {index} val scale"));
        }

        // Non-vacuity: the wire order really is value-outermost / sat-innermost, so
        // entry 1 must be the sat=1 neighbour of entry 0, not the hue=1 neighbour.
        assert_ne!(
            table.entries[0], table.entries[1],
            "entry 0 and 1 must differ so the ordering assertion can fail"
        );
        assert_eq!(table.entries[1], [0.0, 2.0, 1.0], "entry 1 is (v0,h0,s1)");
        assert_eq!(table.entries[2], [1.0, 1.0, 1.0], "entry 2 is (v0,h1,s0)");
    }

    #[test]
    fn decodes_optional_flags_word() {
        let entries = cell_entries(1, 2, 1);
        let mut body = build_body(1, 2, 1, &entries, ENCODING_SRGB);
        body.extend_from_slice(&1u32.to_le_bytes()); // "embed never"

        let encoded = encode_payload(body.len() as u32, &body);
        let table = decode_adobe_look_table(&encoded).expect("table with flags should decode");

        assert_eq!(table.flags, Some(1));
        assert_eq!(table.encoding, ENCODING_SRGB);
    }

    #[test]
    fn accepts_encoding_srgb_and_linear_but_rejects_unknown() {
        let entries = cell_entries(1, 2, 1);

        for encoding in [ENCODING_LINEAR, ENCODING_SRGB] {
            let body = build_body(1, 2, 1, &entries, encoding);
            let encoded = encode_payload(body.len() as u32, &body);
            assert!(
                decode_adobe_look_table(&encoded).is_ok(),
                "encoding {encoding} should be supported"
            );
        }

        let body = build_body(1, 2, 1, &entries, 2);
        let encoded = encode_payload(body.len() as u32, &body);
        let error = decode_adobe_look_table(&encoded)
            .expect_err("unknown encoding must be rejected");
        assert!(
            error.contains("unsupported embedded LookTable encoding 2"),
            "unexpected error: {error}"
        );
    }

    // ------------------------------------------------------------- framing

    #[test]
    fn rejects_invalid_base85_character() {
        let body = build_body(1, 2, 1, &cell_entries(1, 2, 1), ENCODING_LINEAR);
        let mut encoded = encode_payload(body.len() as u32, &body);
        // '~' is not part of Adobe's Base85 alphabet.
        encoded.replace_range(2..3, "~");

        let error = decode_adobe_look_table(&encoded).expect_err("bad Base85 digit must be rejected");
        assert!(
            error.contains("invalid Adobe Base85 character '~' in embedded LookTable"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_declared_length_mismatch() {
        let body = build_body(1, 2, 1, &cell_entries(1, 2, 1), ENCODING_LINEAR);
        let encoded = encode_payload(body.len() as u32 + 12, &body);

        let error =
            decode_adobe_look_table(&encoded).expect_err("declared/actual size mismatch must fail");
        assert!(error.contains("LookTable size mismatch"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_truncated_payload() {
        let body = build_body(1, 2, 1, &cell_entries(1, 2, 1), ENCODING_LINEAR);
        // Declare the full size but compress a truncated body.
        let truncated = &body[..body.len() - ENCODING_BYTES];
        let encoded = encode_payload(body.len() as u32, truncated);

        assert!(
            decode_adobe_look_table(&encoded).is_err(),
            "a body shorter than its declared length must be rejected"
        );
    }

    #[test]
    fn rejects_trailing_bytes_after_zlib_stream() {
        let body = build_body(1, 2, 1, &cell_entries(1, 2, 1), ENCODING_LINEAR);

        let mut payload = (body.len() as u32).to_le_bytes().to_vec();
        payload.extend_from_slice(&zlib_compress(&body));
        payload.push(0x00);

        let encoded = base85_encode(&payload);
        let error =
            decode_adobe_look_table(&encoded).expect_err("trailing bytes must be rejected");
        assert!(
            error.contains("trailing bytes after the zlib stream"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_declared_size_above_supported_maximum() {
        let body = build_body(1, 2, 1, &cell_entries(1, 2, 1), ENCODING_LINEAR);
        let encoded = encode_payload(u32::MAX, &body);

        let error = decode_adobe_look_table(&encoded)
            .expect_err("declared size above the maximum must be rejected");
        assert!(
            error.contains("embedded LookTable declared size exceeds supported maximum"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_declared_size_below_supported_minimum() {
        let encoded = encode_payload((MIN_BLOCK_BYTES as u32) - 1, &[0u8; 4]);

        let error = decode_adobe_look_table(&encoded)
            .expect_err("declared size below the minimum must be rejected");
        assert!(
            error.contains("embedded LookTable declared size is below the supported minimum"),
            "unexpected error: {error}"
        );
    }

    // ------------------------------------------------------- block content

    #[test]
    fn rejects_trailing_garbage_inside_the_block() {
        let entries = cell_entries(1, 2, 1);
        let mut body = build_body(1, 2, 1, &entries, ENCODING_LINEAR);
        body.extend_from_slice(&[0u8; 3]); // neither a clean body nor body + flags

        let encoded = encode_payload(body.len() as u32, &body);
        let error =
            decode_adobe_look_table(&encoded).expect_err("block trailing garbage must be rejected");
        assert!(
            error.contains("embedded LookTable block size mismatch"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_wrong_table_type() {
        let mut block = build_body(1, 2, 1, &cell_entries(1, 2, 1), ENCODING_LINEAR);
        block[0..4].copy_from_slice(&1u32.to_le_bytes()); // btt_RGBTable

        let error = parse_look_table_block(&block).expect_err("wrong type must be rejected");
        assert!(
            error.contains("unsupported embedded LookTable type 1, expected 0"),
            "unexpected error: {error}"
        );

        // Non-vacuity: the untouched body (type 0) parses.
        let clean = build_body(1, 2, 1, &cell_entries(1, 2, 1), ENCODING_LINEAR);
        assert!(parse_look_table_block(&clean).is_ok());
    }

    #[test]
    fn rejects_unsupported_version() {
        let block = build_body_versioned(1, 2, 1, &cell_entries(1, 2, 1), ENCODING_LINEAR, 2);

        let error = parse_look_table_block(&block).expect_err("version 2 must be rejected");
        assert!(
            error.contains("unsupported embedded LookTable version 2, expected 1"),
            "unexpected error: {error}"
        );

        let clean = build_body_versioned(1, 2, 1, &cell_entries(1, 2, 1), ENCODING_LINEAR, 1);
        assert!(parse_look_table_block(&clean).is_ok());
    }

    #[test]
    fn rejects_division_counts_out_of_bounds() {
        let entries = cell_entries(1, 2, 1);

        // hue < min, hue > max
        assert!(parse_look_table_block(&build_body(0, 2, 1, &[], ENCODING_LINEAR)).is_err());
        assert!(
            parse_look_table_block(&build_body(MAX_HUE_DIVISIONS + 1, 2, 1, &[], ENCODING_LINEAR))
                .is_err()
        );

        // sat < min (Adobe requires >= 2), sat > max
        assert!(parse_look_table_block(&build_body(1, 1, 1, &[], ENCODING_LINEAR)).is_err());
        assert!(
            parse_look_table_block(&build_body(1, MAX_SAT_DIVISIONS + 1, 1, &[], ENCODING_LINEAR))
                .is_err()
        );

        // val < min, val > max
        assert!(parse_look_table_block(&build_body(1, 2, 0, &[], ENCODING_LINEAR)).is_err());
        assert!(
            parse_look_table_block(&build_body(1, 2, MAX_VAL_DIVISIONS + 1, &[], ENCODING_LINEAR))
                .is_err()
        );

        // Non-vacuity: the exact minimum grid (1 x 2 x 1) is accepted, proving the
        // rejections above are caused by the bounds, not by the table shape.
        let minimal = build_body(1, 2, 1, &entries, ENCODING_LINEAR);
        assert!(parse_look_table_block(&minimal).is_ok());
    }

    #[test]
    fn rejects_total_samples_above_maximum() {
        // Each dimension is individually legal, yet 360 * 256 * 2 = 184320 > 18432.
        let block = build_body(MAX_HUE_DIVISIONS, MAX_SAT_DIVISIONS, 2, &[], ENCODING_LINEAR);

        let error =
            parse_look_table_block(&block).expect_err("product above 18432 must be rejected");
        assert!(
            error.contains("total samples 184320 exceed supported maximum 18432"),
            "unexpected error: {error}"
        );

        // Non-vacuity: 360 * 16 * 2 = 11520 <= 18432 would be accepted if the body
        // were complete; the rejection above is the product, not a dimension bound.
        assert!(validate_divisions(MAX_HUE_DIVISIONS, MAX_SAT_DIVISIONS, 2).is_err());
        assert!(validate_divisions(MAX_HUE_DIVISIONS, 16, 2).is_ok());
    }

    #[test]
    fn rejects_non_finite_entry() {
        let mut entries = cell_entries(1, 2, 1);
        entries[1] = [f32::NAN, 1.0, 1.0];

        let body = build_body(1, 2, 1, &entries, ENCODING_LINEAR);
        let encoded = encode_payload(body.len() as u32, &body);

        let error = decode_adobe_look_table(&encoded)
            .expect_err("a non-finite entry must be rejected");
        assert!(
            error.contains("embedded LookTable entry 1 is not finite"),
            "unexpected error: {error}"
        );

        // Non-vacuity: the same body with a finite entry 1 decodes.
        let mut clean = cell_entries(1, 2, 1);
        clean[1] = [0.0, 2.0, 1.0];
        let clean_body = build_body(1, 2, 1, &clean, ENCODING_LINEAR);
        let clean_encoded = encode_payload(clean_body.len() as u32, &clean_body);
        assert!(decode_adobe_look_table(&clean_encoded).is_ok());
    }
}
