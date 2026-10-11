//! Minimal, read-only TIFF container reader for Adobe `.dcp` camera profiles.
//!
//! A DCP file is a classic TIFF container whose IFD-0 entries carry the camera
//! profile tags (color matrices, HueSatMap data, tone curve, ...). This reader
//! deliberately supports only what a camera profile needs:
//!
//! * classic TIFF magic `0x2A` in either byte order (BigTIFF `0x2B` is
//!   rejected explicitly - Adobe does not ship it, so treating it as TIFF
//!   would silently mis-read every offset);
//! * the linked IFD chain, bounded;
//! * typed tag access with endian conversion and strict bounds checks.
//!
//! Everything is bounds-checked against the byte slice; a malformed file can
//! never cause an out-of-range read or an unbounded allocation.

/// TIFF type constant `BYTE`.
const TYPE_BYTE: u16 = 1;
/// TIFF type constant `ASCII`.
const TYPE_ASCII: u16 = 2;
/// TIFF type constant `SHORT`.
const TYPE_SHORT: u16 = 3;
/// TIFF type constant `LONG`.
const TYPE_LONG: u16 = 4;
/// TIFF type constant `RATIONAL`.
const TYPE_RATIONAL: u16 = 5;
/// TIFF type constant `SBYTE`.
const TYPE_SBYTE: u16 = 6;
/// TIFF type constant `UNDEFINED`.
const TYPE_UNDEFINED: u16 = 7;
/// TIFF type constant `SSHORT`.
const TYPE_SSHORT: u16 = 8;
/// TIFF type constant `SLONG`.
const TYPE_SLONG: u16 = 9;
/// TIFF type constant `SRATIONAL`.
const TYPE_SRATIONAL: u16 = 10;
/// TIFF type constant `FLOAT`.
const TYPE_FLOAT: u16 = 11;
/// TIFF type constant `DOUBLE`.
const TYPE_DOUBLE: u16 = 12;

/// One TIFF field type's byte size, or `None` for an unknown type id.
fn type_size(field_type: u16) -> Option<u64> {
    match field_type {
        TYPE_BYTE | TYPE_ASCII | TYPE_SBYTE | TYPE_UNDEFINED => Some(1),
        TYPE_SHORT | TYPE_SSHORT => Some(2),
        TYPE_LONG | TYPE_SLONG | TYPE_FLOAT => Some(4),
        TYPE_RATIONAL | TYPE_SRATIONAL | TYPE_DOUBLE => Some(8),
        _ => None,
    }
}

/// A single parsed IFD entry. `data` is the entry's value bytes in file order,
/// already resolved (inline or via the offset field).
#[derive(Debug)]
struct IfdEntry<'a> {
    field_type: u16,
    data: &'a [u8],
}

/// An indexed image file directory (the DCP profile tags live in IFD 0).
#[derive(Debug)]
pub(crate) struct Ifd<'a> {
    little_endian: bool,
    entries: Vec<(u16, IfdEntry<'a>)>,
}

/// A fully parsed TIFF container: header plus the bounded IFD chain.
#[derive(Debug)]
pub(crate) struct Tiff<'a> {
    pub ifds: Vec<Ifd<'a>>,
}

/// Largest DCP we will attempt to read. Real camera profiles are well under
/// 1 MiB; this cap only bounds hostile inputs.
const MAX_DCP_BYTES: usize = 64 * 1024 * 1024;
/// Largest tag-element count allowed for a single entry.
const MAX_TAG_COUNT: u64 = 16 * 1024 * 1024;
/// Largest IFD chain length.
const MAX_IFDS: usize = 8;
/// Largest entry count in a single IFD.
const MAX_IFD_ENTRIES: u64 = 4096;

impl<'a> Tiff<'a> {
    /// Parses a classic TIFF container. Returns a human-readable error on any
    /// structural problem; never panics.
    pub(crate) fn parse(data: &'a [u8]) -> Result<Self, String> {
        if data.len() > MAX_DCP_BYTES {
            return Err(format!(
                "DCP file too large ({} bytes, max {})",
                data.len(),
                MAX_DCP_BYTES
            ));
        }
        if data.len() < 8 {
            return Err("file too short to be a TIFF/DCP container".to_string());
        }

        let little_endian = match &data[0..2] {
            b"II" => true,
            b"MM" => false,
            _ => return Err("not a TIFF/DCP container (bad byte-order mark)".to_string()),
        };

        let read_u16 = |offset: usize| -> Option<u16> {
            let b: [u8; 2] = data.get(offset..offset + 2)?.try_into().ok()?;
            Some(if little_endian {
                u16::from_le_bytes(b)
            } else {
                u16::from_be_bytes(b)
            })
        };
        let read_u32 = |offset: usize| -> Option<u32> {
            let b: [u8; 4] = data.get(offset..offset + 4)?.try_into().ok()?;
            Some(if little_endian {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            })
        };

        match read_u16(2) {
            // Classic TIFF (0x2A) or Adobe's DCP container magic (0x4352,
            // little-endian "CR"): a TIFF structure with profile tags in IFD 0.
            Some(0x2A) | Some(0x4352) => {}
            Some(0x2B) => {
                return Err("BigTIFF containers are not supported camera profiles".to_string());
            }
            _ => return Err("not a TIFF/DCP container (bad magic)".to_string()),
        }

        let mut next_ifd = read_u32(4)
            .filter(|offset| *offset != 0)
            .ok_or_else(|| "DCP container has no first IFD".to_string())?
            as usize;

        let mut ifds = Vec::new();
        let mut visited = std::collections::HashSet::new();

        while next_ifd != 0 {
            if !visited.insert(next_ifd) {
                return Err("cyclic IFD chain in DCP container".to_string());
            }
            if ifds.len() >= MAX_IFDS {
                return Err(format!("DCP exceeds {MAX_IFDS} IFDs"));
            }

            let entry_count =
                read_u16(next_ifd).ok_or_else(|| "truncated IFD entry count".to_string())? as u64;
            if entry_count > MAX_IFD_ENTRIES {
                return Err(format!("IFD entry count {entry_count} exceeds limit"));
            }

            let entries_start = next_ifd
                .checked_add(2)
                .ok_or_else(|| "IFD offset overflow".to_string())?;
            let table_bytes = entry_count
                .checked_mul(12)
                .ok_or_else(|| "IFD size overflow".to_string())?
                as usize;
            let next_offset_pos = entries_start
                .checked_add(table_bytes)
                .ok_or_else(|| "IFD size overflow".to_string())?;
            if next_offset_pos
                .checked_add(4)
                .is_none_or(|end| end > data.len())
            {
                return Err("truncated IFD entry table".to_string());
            }

            let mut entries = Vec::with_capacity(entry_count as usize);
            for index in 0..entry_count as usize {
                let pos = entries_start + index * 12;
                let (Some(tag), Some(field_type), Some(count)) =
                    (read_u16(pos), read_u16(pos + 2), read_u32(pos + 4))
                else {
                    return Err("truncated IFD entry header".to_string());
                };

                let Some(elem_size) = type_size(field_type) else {
                    // Unknown types are skipped, not fatal: Adobe adds new tags
                    // over time, and an unread tag is safer than a misread one.
                    continue;
                };
                let total = u64::from(count)
                    .checked_mul(elem_size)
                    .ok_or_else(|| "tag size overflow".to_string())?;
                if total > MAX_TAG_COUNT * elem_size {
                    return Err(format!("tag {tag} count {count} exceeds limit"));
                }

                let value_bytes = if total <= 4 {
                    // Inline values are stored left-justified in the 4-byte
                    // value field, in FILE byte order regardless of endianness.
                    data.get(pos + 8..pos + 8 + total as usize)
                } else {
                    let offset = read_u32(pos + 8)
                        .ok_or_else(|| "truncated tag offset".to_string())?
                        as usize;
                    offset
                        .checked_add(total as usize)
                        .and_then(|end| data.get(offset..end))
                }
                .ok_or_else(|| format!("tag {tag} data ({count} elements) out of bounds"))?;

                entries.push((
                    tag,
                    IfdEntry {
                        field_type,
                        data: value_bytes,
                    },
                ));
            }

            ifds.push(Ifd {
                little_endian,
                entries,
            });

            next_ifd = read_u32(next_offset_pos).unwrap_or(0) as usize;
        }

        Ok(Self { ifds })
    }
}

impl<'a> Ifd<'a> {
    fn entry(&self, tag: u16) -> Option<&IfdEntry<'a>> {
        self.entries
            .iter()
            .find(|(entry_tag, _)| *entry_tag == tag)
            .map(|(_, entry)| entry)
    }

    fn read_u16(&self, bytes: &[u8]) -> u16 {
        let b: [u8; 2] = bytes[..2].try_into().expect("checked length");
        if self.little_endian {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }
    }

    fn read_u32(&self, bytes: &[u8]) -> u32 {
        let b: [u8; 4] = bytes[..4].try_into().expect("checked length");
        if self.little_endian {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    }

    fn read_i32(&self, bytes: &[u8]) -> i32 {
        let b: [u8; 4] = bytes[..4].try_into().expect("checked length");
        if self.little_endian {
            i32::from_le_bytes(b)
        } else {
            i32::from_be_bytes(b)
        }
    }

    fn read_f32(&self, bytes: &[u8]) -> f32 {
        let b: [u8; 4] = bytes[..4].try_into().expect("checked length");
        if self.little_endian {
            f32::from_le_bytes(b)
        } else {
            f32::from_be_bytes(b)
        }
    }

    fn read_f64(&self, bytes: &[u8]) -> f64 {
        let b: [u8; 8] = bytes[..8].try_into().expect("checked length");
        if self.little_endian {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        }
    }

    /// Whether `tag` is present in the IFD (type-agnostic presence check).
    pub(crate) fn contains(&self, tag: u16) -> bool {
        self.entry(tag).is_some()
    }

    /// Reads a NUL-terminated ASCII tag. Trailing NULs are stripped.
    pub(crate) fn ascii(&self, tag: u16) -> Result<Option<String>, String> {
        let Some(entry) = self.entry(tag) else {
            return Ok(None);
        };
        if entry.field_type != TYPE_ASCII {
            return Err(format!(
                "tag {tag}: expected ASCII, got type {}",
                entry.field_type
            ));
        }
        let text = entry
            .data
            .split(|b| *b == 0)
            .next()
            .unwrap_or_default()
            .to_vec();
        Ok(Some(String::from_utf8_lossy(&text).into_owned()))
    }

    /// Reads a SHORT or LONG tag as a list of u32s (either type accepted).
    pub(crate) fn u32_list(&self, tag: u16) -> Result<Option<Vec<u32>>, String> {
        let Some(entry) = self.entry(tag) else {
            return Ok(None);
        };
        let values = match entry.field_type {
            TYPE_SHORT => entry
                .data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u32::from(self.read_u16(b)))
                .collect(),
            TYPE_LONG => entry
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| self.read_u32(b))
                .collect(),
            other => {
                return Err(format!("tag {tag}: expected SHORT/LONG, got type {other}"));
            }
        };
        Ok(Some(values))
    }

    /// Reads a scalar SHORT/LONG tag.
    pub(crate) fn u32_scalar(&self, tag: u16) -> Result<Option<u32>, String> {
        Ok(self
            .u32_list(tag)?
            .and_then(|values| values.first().copied()))
    }

    /// Reads a RATIONAL, SRATIONAL, FLOAT, SHORT or LONG tag as f64s, so matrix
    /// tags can be consumed without caring which numeric encoding Adobe chose.
    pub(crate) fn f64_list(&self, tag: u16) -> Result<Option<Vec<f64>>, String> {
        let Some(entry) = self.entry(tag) else {
            return Ok(None);
        };
        let values: Vec<f64> = match entry.field_type {
            TYPE_SHORT => entry
                .data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| f64::from(self.read_u16(b)))
                .collect(),
            TYPE_LONG => entry
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f64::from(self.read_u32(b)))
                .collect(),
            TYPE_RATIONAL => entry
                .data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|b| {
                    let num = self.read_u32(&b[..4]) as f64;
                    let den = self.read_u32(&b[4..]) as f64;
                    if den == 0.0 { 0.0 } else { num / den }
                })
                .collect(),
            TYPE_SRATIONAL => entry
                .data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|b| {
                    let num = self.read_i32(&b[..4]) as f64;
                    let den = self.read_i32(&b[4..]) as f64;
                    if den == 0.0 { 0.0 } else { num / den }
                })
                .collect(),
            TYPE_FLOAT => entry
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f64::from(self.read_f32(b)))
                .collect(),
            TYPE_DOUBLE => entry
                .data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|b| self.read_f64(b))
                .collect(),
            other => {
                return Err(format!(
                    "tag {tag}: expected numeric type, got type {other}"
                ));
            }
        };
        Ok(Some(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal little-endian TIFF container with the given tags.
    /// Each spec is `(tag, field_type, count, value_bytes)`; values longer
    /// than 4 bytes are appended after the IFD.
    fn build_le_tiff(entries: &[(u16, u16, u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&0x2Au16.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8

        let count = entries.len() as u16;
        out.extend_from_slice(&count.to_le_bytes());

        let table_len = 2 + 12 * entries.len() + 4;
        let mut data_area = Vec::new();
        for (tag, ty, n, bytes) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&n.to_le_bytes());
            if bytes.len() <= 4 {
                out.extend_from_slice(bytes);
                out.resize(out.len() + (4 - bytes.len()), 0);
            } else {
                let offset = (8 + table_len + data_area.len()) as u32;
                out.extend_from_slice(&offset.to_le_bytes());
                data_area.extend_from_slice(bytes);
            }
        }
        out.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        out.extend_from_slice(&data_area);
        out
    }

    #[test]
    fn parses_little_endian_container() {
        let bytes = build_le_tiff(&[
            (50708, TYPE_ASCII, 13, b"Canon EOS R5\0".to_vec()),
            (50778, TYPE_SHORT, 1, 21u16.to_le_bytes().to_vec()),
        ]);
        let tiff = Tiff::parse(&bytes).expect("valid container parses");
        assert_eq!(tiff.ifds.len(), 1);
        let ifd = &tiff.ifds[0];
        assert_eq!(ifd.ascii(50708).unwrap().as_deref(), Some("Canon EOS R5"));
        assert_eq!(ifd.u32_scalar(50778).unwrap(), Some(21));
    }

    #[test]
    fn parses_big_endian_container() {
        let mut out = Vec::new();
        out.extend_from_slice(b"MM");
        out.extend_from_slice(&0x2Au16.to_be_bytes());
        out.extend_from_slice(&8u32.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&50721u16.to_be_bytes());
        out.extend_from_slice(&TYPE_RATIONAL.to_be_bytes());
        out.extend_from_slice(&9u32.to_be_bytes());
        out.extend_from_slice(&26u32.to_be_bytes()); // data offset after table
        out.extend_from_slice(&0u32.to_be_bytes());
        for i in 1..=9i32 {
            out.extend_from_slice(&i.to_be_bytes());
            out.extend_from_slice(&1i32.to_be_bytes());
        }
        let tiff = Tiff::parse(&out).expect("BE container parses");
        let values = tiff.ifds[0].f64_list(50721).unwrap().unwrap();
        assert_eq!(values, (1..=9).map(|v| v as f64).collect::<Vec<_>>());
    }

    #[test]
    fn rejects_garbage() {
        assert!(Tiff::parse(b"").is_err());
        assert!(Tiff::parse(b"II\x2a\x00").is_err());
        assert!(Tiff::parse(b"ZZ\x2a\x00\x08\x00\x00\x00").is_err());
        assert!(Tiff::parse(b"II\x2b\x00\x08\x00\x00\x00").is_err());
    }

    #[test]
    fn rejects_out_of_bounds_tag_data() {
        // Tag claims an offset far beyond the buffer end.
        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&0x2Au16.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&50982u16.to_le_bytes());
        out.extend_from_slice(&TYPE_FLOAT.to_le_bytes());
        out.extend_from_slice(&100u32.to_le_bytes());
        out.extend_from_slice(&0xFFFF0000u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        assert!(Tiff::parse(&out).is_err());
    }

    #[test]
    fn short_and_long_uniformly_read() {
        let bytes = build_le_tiff(&[
            (50937, TYPE_LONG, 3, {
                let mut v = Vec::new();
                for n in [36u32, 8, 4] {
                    v.extend_from_slice(&n.to_le_bytes());
                }
                v
            }),
            (50778, TYPE_SHORT, 1, 17u16.to_le_bytes().to_vec()),
        ]);
        let tiff = Tiff::parse(&bytes).unwrap();
        assert_eq!(tiff.ifds[0].u32_list(50937).unwrap(), Some(vec![36, 8, 4]));
        assert_eq!(tiff.ifds[0].u32_scalar(50778).unwrap(), Some(17));
    }
}
