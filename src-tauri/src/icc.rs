//! A self-contained, minimal but fully valid sRGB ICC profile for tagging
//! exports.
//!
//! Once-Lab encodes SDR output in sRGB (the Adobe comparison path explicitly,
//! the normal path by construction), so every PNG/TIFF it writes is correctly
//! described by a standard matrix/TRC sRGB display profile. This module builds
//! the well-known v4.3 profile programmatically instead of embedding a blob:
//! `desc`/`cprt` (mluc), D50 `wtpt`, Bradford-adapted `rXYZ`/`gXYZ`/`bXYZ`,
//! the sRGB parametric TRC (`para` function type 4) and the `chad` matrix —
//! the same tag set as the canonical sRGB profiles shipping with the OS.
//!
//! Nothing here derives from Adobe's profiles; the numeric values are the
//! sRGB standard's published constants.

/// The complete ICC profile byte stream (little over 500 bytes).
pub(crate) fn srgb_icc_profile() -> Vec<u8> {
    let trc = para_tag4();

    // Required display-profile tags.
    let tags: Vec<([u8; 4], Vec<u8>)> = vec![
        (*b"desc", mluc_tag("sRGB")),
        (*b"cprt", mluc_tag("CC0, no copyright")),
        (*b"wtpt", xyz_tag(D50_XYZ)),
        (*b"rXYZ", xyz_tag(SRGB_R_XYZ)),
        (*b"gXYZ", xyz_tag(SRGB_G_XYZ)),
        (*b"bXYZ", xyz_tag(SRGB_B_XYZ)),
        (*b"rTRC", trc.clone()),
        (*b"gTRC", trc.clone()),
        (*b"bTRC", trc),
        (*b"chad", sf32_tag(&SRGB_CHAD)),
    ];

    // ------------------------------------------------------------- layout
    let header_len = 128usize;
    let table_len = 4 + tags.len() * 12;
    let mut offset = header_len + table_len;
    // Tag data is 4-byte aligned within the file.
    let aligned = |o: usize| (o + 3) & !3;
    offset = aligned(offset);

    let mut out = Vec::with_capacity(offset + tags.iter().map(|t| t.1.len() + 4).sum::<usize>());
    out.resize(header_len, 0);

    // --- header ------------------------------------------------------------
    out[4..8].copy_from_slice(b"ONCE"); // preferred CMM
    out[8..12].copy_from_slice(&0x0430_0000u32.to_be_bytes()); // version 4.3
    out[12..16].copy_from_slice(b"mntr"); // display device class
    out[16..20].copy_from_slice(b"RGB "); // data colorspace
    out[20..24].copy_from_slice(b"XYZ "); // PCS
    // creation date (year, month, day, hour, minute, second)
    for (i, v) in [2024u16, 1, 1, 0, 0, 0].iter().enumerate() {
        out[24 + i * 2..26 + i * 2].copy_from_slice(&v.to_be_bytes());
    }
    out[36..40].copy_from_slice(b"acsp"); // magic
    out[40..44].copy_from_slice(b"APPL"); // platform
    // flags [44..48], manufacturer [48..52], model [52..56], attributes [56..64]
    out[64..68].copy_from_slice(&0u32.to_be_bytes()); // perceptual intent
    // PCS illuminant: D50
    for (i, v) in D50_XYZ.iter().enumerate() {
        out[68 + i * 4..72 + i * 4].copy_from_slice(&s15f16(*v).to_be_bytes());
    }
    out[80..84].copy_from_slice(b"ONCE"); // creator
    // profile ID [84..100] left zero (optional per spec)

    // --- tag table ---------------------------------------------------------
    out.resize(aligned(out.len()), 0);
    out.extend_from_slice(&(tags.len() as u32).to_be_bytes());
    let mut cursor = offset;
    let mut records = Vec::new();
    for (sig, data) in &tags {
        records.push((*sig, cursor as u32, data.len() as u32));
        cursor = aligned(cursor + data.len());
    }
    for (sig, off, size) in &records {
        out.extend_from_slice(sig);
        out.extend_from_slice(&off.to_be_bytes());
        out.extend_from_slice(&size.to_be_bytes());
    }

    // --- tag data ----------------------------------------------------------
    let mut cursor = offset;
    for (_sig, data) in &tags {
        out.resize(aligned(out.len()), 0);
        debug_assert_eq!(out.len(), cursor);
        out.extend_from_slice(data);
        cursor = aligned(cursor + data.len());
    }

    // Profile size goes in the first four bytes.
    let size = (out.len() as u32).to_be_bytes();
    out[0..4].copy_from_slice(&size);
    out
}

/// ICC D50 PCS illuminant.
const D50_XYZ: [f64; 3] = [0.9642, 1.0, 0.8249];

/// sRGB primaries after the standard Bradford adaptation to D50 — the values
/// every shipping sRGB profile carries.
const SRGB_R_XYZ: [f64; 3] = [0.43604, 0.22248, 0.01392];
const SRGB_G_XYZ: [f64; 3] = [0.38512, 0.71690, 0.09706];
const SRGB_B_XYZ: [f64; 3] = [0.14305, 0.06061, 0.71410];

/// Bradford D65 -> D50 chromatic adaptation matrix (`chad`), the standard
/// sRGB-profile values.
#[rustfmt::skip]
const SRGB_CHAD: [f64; 9] = [
    1.0479297,  0.0229468, -0.0501925,
    0.0296278,  0.9904344, -0.0170737,
   -0.0092430,  0.0151413,  0.7518743,
];

/// s15Fixed16Number: sign + 15 integer bits + 16 fraction bits.
fn s15f16(v: f64) -> i32 {
    (v * 65536.0)
        .round()
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

/// `xyzType`: signature + reserved + XYZNumber.
fn xyz_tag(xyz: [f64; 3]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    out.extend_from_slice(b"XYZ ");
    out.extend_from_slice(&[0; 4]);
    for v in xyz {
        out.extend_from_slice(&s15f16(v).to_be_bytes());
    }
    out
}

/// `s15Fixed32ArrayType` (`sf32`): signature + reserved + n numbers.
fn sf32_tag(values: &[f64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + values.len() * 4);
    out.extend_from_slice(b"sf32");
    out.extend_from_slice(&[0; 4]);
    for v in values {
        out.extend_from_slice(&s15f16(*v).to_be_bytes());
    }
    out
}

/// `parametricCurveType` function 4 — the sRGB EOTF:
/// `y = (a*x + b)^g + e` for `x >= d`, else `y = c*x + f`, with the sRGB
/// constants `g=2.4, a=1/1.055, b=0.055/1.055, c=1/12.92, d=0.04045`.
fn para_tag4() -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 4 + 7 * 4);
    out.extend_from_slice(b"para");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&4u16.to_be_bytes()); // function type 4
    out.extend_from_slice(&0u16.to_be_bytes()); // reserved
    for v in [
        2.4,
        1.0 / 1.055,
        0.055 / 1.055,
        1.0 / 12.92,
        0.04045,
        0.0,
        0.0,
    ] {
        out.extend_from_slice(&s15f16(v).to_be_bytes());
    }
    out
}

/// `multiLocalizedUnicodeType` with a single en-US record.
fn mluc_tag(text: &str) -> Vec<u8> {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let text_bytes = (utf16.len() * 2) as u32;
    // header: sig, reserved, count, recSize(12), then one record.
    let text_offset = 8 + 8 + 12; // after header+record
    let mut out = Vec::with_capacity(text_offset + text_bytes as usize);
    out.extend_from_slice(b"mluc");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&1u32.to_be_bytes()); // record count
    out.extend_from_slice(&12u32.to_be_bytes()); // record size
    out.extend_from_slice(b"enUS"); // lang + country
    out.extend_from_slice(&text_bytes.to_be_bytes());
    out.extend_from_slice(&(text_offset as u32).to_be_bytes());
    for code in utf16 {
        out.extend_from_slice(&code.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_is_structurally_valid() {
        let icc = srgb_icc_profile();
        assert!(icc.len() >= 132);
        // Magic + self-consistent size.
        assert_eq!(&icc[36..40], b"acsp");
        let declared = u32::from_be_bytes(icc[0..4].try_into().unwrap()) as usize;
        assert_eq!(declared, icc.len());

        let tag_count = u32::from_be_bytes(icc[128..132].try_into().unwrap()) as usize;
        assert_eq!(tag_count, 10);

        // Every tag record must point inside the profile, 4-byte aligned.
        for i in 0..tag_count {
            let base = 132 + i * 12;
            let off = u32::from_be_bytes(icc[base + 4..base + 8].try_into().unwrap()) as usize;
            let size = u32::from_be_bytes(icc[base + 8..base + 12].try_into().unwrap()) as usize;
            assert!(off.is_multiple_of(4), "tag {i} offset {off} unaligned");
            assert!(off + size <= icc.len(), "tag {i} out of bounds");
        }
    }

    #[test]
    fn primaries_and_trc_carry_the_srgb_constants() {
        let icc = srgb_icc_profile();
        let read_tag = |sig: &[u8; 4]| -> Vec<u8> {
            let count = u32::from_be_bytes(icc[128..132].try_into().unwrap());
            for i in 0..count as usize {
                let base = 132 + i * 12;
                if &icc[base..base + 4] == sig {
                    let off =
                        u32::from_be_bytes(icc[base + 4..base + 8].try_into().unwrap()) as usize;
                    let size =
                        u32::from_be_bytes(icc[base + 8..base + 12].try_into().unwrap()) as usize;
                    return icc[off..off + size].to_vec();
                }
            }
            panic!("tag {sig:?} missing");
        };

        let read_xyz = |tag: &[u8]| -> [f64; 3] {
            assert_eq!(&tag[0..4], b"XYZ ");
            let mut out = [0.0; 3];
            for i in 0..3 {
                let raw = i32::from_be_bytes(tag[8 + i * 4..12 + i * 4].try_into().unwrap());
                out[i] = raw as f64 / 65536.0;
            }
            out
        };

        let wtpt = read_xyz(&read_tag(b"wtpt"));
        assert!((wtpt[0] - 0.9642).abs() < 1e-4);
        assert!((wtpt[1] - 1.0).abs() < 1e-4);
        assert!((wtpt[2] - 0.8249).abs() < 1e-4);

        let r = read_xyz(&read_tag(b"rXYZ"));
        assert!((r[0] - 0.43604).abs() < 1e-4);

        // TRC: para function type 4, g ≈ 2.4, d ≈ 0.04045.
        let trc = read_tag(b"rTRC");
        assert_eq!(&trc[0..4], b"para");
        let func = u16::from_be_bytes(trc[8..10].try_into().unwrap());
        assert_eq!(func, 4);
        let g = i32::from_be_bytes(trc[12..16].try_into().unwrap()) as f64 / 65536.0;
        let d = i32::from_be_bytes(trc[28..32].try_into().unwrap()) as f64 / 65536.0;
        assert!((g - 2.4).abs() < 1e-3);
        assert!((d - 0.04045).abs() < 1e-3);
    }
}
