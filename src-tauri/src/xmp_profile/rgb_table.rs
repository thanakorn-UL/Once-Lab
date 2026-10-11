use flate2::{Decompress, FlushDecompress, Status};

/// Adobe's Z85-like Base85 alphabet, verbatim from `dng_big_table.cpp`
/// (`kEncodeTable`). `pub(super)` so the LookTable test fixtures can reuse it.
pub(super) const ADOBE_BASE85_ALPHABET: &[u8; 85] =
    b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.-:+=^!/*?`'|()[]{}@%$#";

const BASE85_LOOKUP: [u8; 256] = build_base85_lookup();

const POW85: [u64; 5] = [1, 85, 85 * 85, 85 * 85 * 85, 85 * 85 * 85 * 85];

const INVALID_BASE85_DIGIT: u8 = 0xFF;

const HEADER_BYTES: usize = 16;
const FOOTER_BYTES: usize = 12;
const AMOUNT_BYTES: usize = 16;
/// The optional trailing `u32` big-table flags word
/// (`dng_big_table.cpp:2606-2609`: read only when exactly 4 bytes remain).
const FLAGS_BYTES: usize = 4;
const BYTES_PER_NODE: usize = 6;

/// `dng_rgb_table::kMinDivisions3D`.
const MIN_SIZE_3D: u32 = 2;
/// Adobe's read/write/import ceiling for a 3-D table:
/// `dng_rgb_table::kMaxDivisions3D_InMemory` (`dng_big_table.h:623`). The other
/// constant, `kMaxDivisions3D` (32), is only the default *resample* resolution
/// used when baking a table -- it is NOT the maximum a table may carry.
/// `dng_rgb_table::GetStream` accepts 3-D divisions up to this value
/// (`dng_big_table.cpp:2477-2490`, "Raised from 32 so common 64-cube LUTs
/// persist"), and `PutStream`/`Set`/`.cube` import enforce the same limit, so a
/// legal Adobe 64-division `crs:RGBTable` decodes here. 65 is rejected.
const MAX_SIZE_3D: u32 = 64;

/// `dng_rgb_table::kMinDivisions1D` / `kMaxDivisions1D` (`dng_big_table.h:604-605`,
/// read path `dng_big_table.cpp:2463-2472`).
const MIN_SIZE_1D: u32 = 2;
const MAX_SIZE_1D: u32 = 4096;

/// BigTableTypeEnum::btt_RGBTable.
const RGB_TABLE_TYPE: u32 = 1;

/// Label woven into every RGBTable error string. Kept as a named constant so the
/// shared framing helper can be called with it and the historical
/// `embedded RGBTable ...` messages stay byte-identical.
const RGB_TABLE_LABEL: &str = "RGBTable";

/// dng_rgb_table::kRGBTableVersion.
const RGB_TABLE_VERSION: u32 = 1;

/// `dng_rgb_table` legal `fDimensions` values (`dng_big_table.cpp:2463-2498`).
const DIMENSIONS_1D: u32 = 1;
const DIMENSIONS_3D: u32 = 3;

/// In-memory sample stride: `fSamples` is `node_count` nodes of 4 u16
/// (R, G, B, and a zero pad word), `dng_big_table.cpp:2515-2564`.
const SAMPLE_STRIDE: usize = 4;

const CHANNEL_MAX: u32 = 65535;

/// Largest legal decompressed block: a 64-division 3-D table, plus the optional
/// flags word.
const MAX_BLOCK_BYTES: usize = HEADER_BYTES
    + (MAX_SIZE_3D as usize * MAX_SIZE_3D as usize * MAX_SIZE_3D as usize * BYTES_PER_NODE)
    + FOOTER_BYTES
    + AMOUNT_BYTES
    + FLAGS_BYTES;

/// Smallest legal decompressed block: a 2-division 1-D table.
const MIN_BLOCK_BYTES: usize =
    HEADER_BYTES + (MIN_SIZE_1D as usize * BYTES_PER_NODE) + FOOTER_BYTES + AMOUNT_BYTES;

const fn build_base85_lookup() -> [u8; 256] {
    let mut table = [INVALID_BASE85_DIGIT; 256];
    let mut index = 0usize;
    while index < ADOBE_BASE85_ALPHABET.len() {
        table[ADOBE_BASE85_ALPHABET[index] as usize] = index as u8;
        index += 1;
    }
    table
}

/// A decoded Adobe `dng_rgb_table`.
///
/// The samples are kept in Adobe's own in-memory layout: absolute `u16` values
/// at a stride of 4 per node (`RGB0`, `dng_big_table.cpp:2515-2564`), NOT
/// pre-normalized to `f32`. Preserving `u16` keeps the delta codec lossless
/// mod 2^16 and matches what the SDK's tetrahedral kernel consumes
/// (`RefRGBtoRGBTable3D`, `dng_reference.cpp:3728-3748`, weights x raw u16
/// then `(1/65535)`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RgbTable {
    /// `fDimensions`: 1 or 3 (`dng_big_table.cpp:2463-2498`).
    pub dimensions: u32,
    /// `fDivisions`: the per-axis node count (`size` historically).
    pub size: usize,
    /// Absolute `u16` samples, `node_count * SAMPLE_STRIDE` long; node `i` is
    /// `samples[4*i .. 4*i+3]` (the 4th word is Adobe's zero pad).
    pub samples: Vec<u16>,
    pub color_space: u32,
    pub gamma: u32,
    pub gamut: u32,
    pub min_amount: f64,
    pub max_amount: f64,
    /// The generic big-table flags word, when the stream carried one
    /// (`dng_big_table.cpp:2606-2609`). Adobe defines only bit 0
    /// ("embed never"), which affects file writing, never rendering.
    pub flags: Option<u32>,
}

impl RgbTable {
    /// Builds a table from normalized `f32` node values, quantizing to `u16`
    /// exactly the way an Adobe encoder would (`round(x * 65535)` clamped).
    /// Test-fixture convenience; production tables arrive as `u16` on the wire.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_f32_nodes(
        dimensions: u32,
        size: usize,
        values: &[[f32; 3]],
        color_space: u32,
        gamma: u32,
        gamut: u32,
        min_amount: f64,
        max_amount: f64,
    ) -> RgbTable {
        let mut samples = Vec::with_capacity(values.len() * SAMPLE_STRIDE);
        for node in values {
            for channel in node {
                let quantized = (f64::from(*channel) * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16;
                samples.push(quantized);
            }
            samples.push(0);
        }
        RgbTable {
            dimensions,
            size,
            samples,
            color_space,
            gamma,
            gamut,
            min_amount,
            max_amount,
            flags: None,
        }
    }

    /// Number of stored nodes (`divisions` for 1-D, `divisions^3` for 3-D).
    pub(crate) fn node_count(&self) -> usize {
        self.samples.len() / SAMPLE_STRIDE
    }

    /// The RGB triplet of node `index`, normalized to `[0,1]` `f32`
    /// (`u16 * (1/65535)`, the same conversion the SDK kernel applies).
    #[cfg(test)]
    pub(crate) fn node(&self, index: usize) -> [f32; 3] {
        let base = index * SAMPLE_STRIDE;
        [
            self.samples[base] as f32 / 65535.0,
            self.samples[base + 1] as f32 / 65535.0,
            self.samples[base + 2] as f32 / 65535.0,
        ]
    }
}

pub(crate) fn decode_adobe_rgb_table(encoded: &str) -> Result<RgbTable, String> {
    let block = decode_big_table_payload(encoded, "RGBTable", MIN_BLOCK_BYTES, MAX_BLOCK_BYTES)?;

    parse_uncompressed_block(&block)
}

/// Decodes one Adobe "big table" payload into its exact decompressed byte block.
///
/// `crs:RGBTable` and `crs:LookTable` share byte-identical *outer* framing: the
/// XMP attribute value is Adobe's Z85-like Base85 (5 chars -> 4 little-endian
/// bytes) of a `[u32 LE declared length][zlib stream]` blob. Only the bytes
/// inside the zlib stream differ, so the Base85, the length prefix, the zlib
/// inflate and the exact-length checks live here once and are parameterised.
///
/// `label` is woven into every error string so a caller keeps stable, greppable
/// messages (RGBTable passes `"RGBTable"` and therefore keeps its historical
/// `embedded RGBTable ...` strings byte-for-byte). `min_bytes`/`max_bytes` are
/// the caller's accepted decompressed sizes; both are checked BEFORE the output
/// buffer is allocated or the stream is inflated, so a hostile length prefix can
/// never drive a large allocation.
pub(super) fn decode_big_table_payload(
    encoded: &str,
    label: &str,
    min_bytes: usize,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    let compressed = decode_base85(encoded, label)?;

    if compressed.len() < 4 {
        return Err(format!(
            "embedded {label} payload is shorter than its 4-byte length prefix"
        ));
    }

    let expected_size =
        u32::from_le_bytes([compressed[0], compressed[1], compressed[2], compressed[3]]);
    let expected_size = expected_size as usize;

    if expected_size > max_bytes {
        return Err(format!(
            "embedded {label} declared size exceeds supported maximum"
        ));
    }

    if expected_size < min_bytes {
        return Err(format!(
            "embedded {label} declared size is below the supported minimum"
        ));
    }

    let stream = &compressed[4..];
    let mut block = vec![0u8; expected_size];

    let mut decoder = Decompress::new(true);
    let status = decoder
        .decompress(stream, &mut block, FlushDecompress::Finish)
        .map_err(|error| format!("failed to decompress embedded {label}: {error}"))?;

    if status != Status::StreamEnd {
        return Err(format!("embedded {label} zlib stream is incomplete"));
    }

    if decoder.total_out() != expected_size as u64 {
        return Err(format!(
            "embedded {label} size mismatch: length prefix declares {expected_size} bytes, decompressed {} bytes",
            decoder.total_out()
        ));
    }

    if decoder.total_in() != stream.len() as u64 {
        return Err(format!(
            "embedded {label} payload contains trailing bytes after the zlib stream"
        ));
    }

    Ok(block)
}

fn decode_base85(encoded: &str, label: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(encoded.len() / 5 * 4 + 4);
    let mut value: u64 = 0;
    let mut phase = 0usize;

    for &byte in encoded.as_bytes() {
        let digit = BASE85_LOOKUP[byte as usize];
        if digit == INVALID_BASE85_DIGIT {
            return Err(format!(
                "invalid Adobe Base85 character '{}' in embedded {label}",
                char::from(byte)
            ));
        }

        value += u64::from(digit) * POW85[phase];
        phase += 1;

        if phase == 5 {
            let word = u32::try_from(value)
                .map_err(|_| "Adobe Base85 group does not fit into 32 bits".to_string())?;
            out.extend_from_slice(&word.to_le_bytes());
            value = 0;
            phase = 0;
        }
    }

    if phase == 1 {
        return Err(
            "Adobe Base85 payload ends with an incomplete single-character group".to_string(),
        );
    }

    if phase > 1 {
        let word = u32::try_from(value)
            .map_err(|_| "Adobe Base85 group does not fit into 32 bits".to_string())?;
        out.extend_from_slice(&word.to_le_bytes()[..phase - 1]);
    }

    Ok(out)
}

fn parse_uncompressed_block(block: &[u8]) -> Result<RgbTable, String> {
    let table_type = read_u32(block, 0, RGB_TABLE_LABEL)?;
    let table_version = read_u32(block, 4, RGB_TABLE_LABEL)?;
    let dimensions = read_u32(block, 8, RGB_TABLE_LABEL)?;
    let size_field = read_u32(block, 12, RGB_TABLE_LABEL)?;

    if table_type != RGB_TABLE_TYPE {
        return Err(format!(
            "unsupported embedded RGBTable type {table_type}, expected {RGB_TABLE_TYPE}"
        ));
    }

    if table_version != RGB_TABLE_VERSION {
        return Err(format!(
            "unsupported embedded RGBTable version {table_version}, expected {RGB_TABLE_VERSION}"
        ));
    }

    // Division bounds depend on dimensionality (`dng_rgb_table::GetStream`,
    // `dng_big_table.cpp:2463-2498`): 1-D tables run `kMinDivisions1D..=
    // kMaxDivisions1D` (2..=4096), 3-D tables `kMinDivisions3D..=
    // kMaxDivisions3D_InMemory` (2..=64). Any other `fDimensions` is rejected.
    let (min_size, max_size) = match dimensions {
        DIMENSIONS_1D => (MIN_SIZE_1D, MAX_SIZE_1D),
        DIMENSIONS_3D => (MIN_SIZE_3D, MAX_SIZE_3D),
        _ => {
            return Err(format!(
                "unsupported embedded RGBTable dimensions {dimensions}, expected {DIMENSIONS_1D} or {DIMENSIONS_3D}"
            ));
        }
    };

    if !(min_size..=max_size).contains(&size_field) {
        return Err(format!(
            "unsupported embedded RGBTable size {size_field}, expected {min_size}..={max_size}"
        ));
    }

    let size = size_field as usize;

    let node_count = match dimensions {
        DIMENSIONS_1D => size,
        _ => size
            .checked_mul(size)
            .and_then(|nodes| nodes.checked_mul(size))
            .ok_or_else(|| "embedded RGBTable node count overflows usize".to_string())?,
    };

    let sample_bytes = node_count
        .checked_mul(BYTES_PER_NODE)
        .ok_or_else(|| "embedded RGBTable sample byte count overflows usize".to_string())?;

    let expected_block_size = HEADER_BYTES
        .checked_add(sample_bytes)
        .and_then(|total| total.checked_add(FOOTER_BYTES))
        .and_then(|total| total.checked_add(AMOUNT_BYTES))
        .ok_or_else(|| "embedded RGBTable block size overflows usize".to_string())?;

    // The stream is either exactly the table body, or the body plus the
    // optional `u32` flags word (`dng_big_table.cpp:2606-2609`: the SDK reads
    // the word whenever 4 bytes remain). Anything else is corrupt.
    let flags_present = if block.len() == expected_block_size {
        false
    } else if block.len() == expected_block_size + FLAGS_BYTES {
        true
    } else {
        return Err(format!(
            "embedded RGBTable block size mismatch: expected {expected_block_size} or {} bytes, got {} bytes",
            expected_block_size + FLAGS_BYTES,
            block.len()
        ));
    };

    // `nopValue` identity ramp (`dng_big_table.cpp:2500-2510`):
    // `(index * 0xFFFF + (divisions >> 1)) / (divisions - 1)`, integer math.
    let mut identity_ramp = Vec::with_capacity(size);
    for index in 0..size {
        let scaled = index
            .checked_mul(CHANNEL_MAX as usize)
            .and_then(|value| value.checked_add(size / 2))
            .ok_or_else(|| "embedded RGBTable identity ramp overflows usize".to_string())?;
        identity_ramp.push((scaled / (size - 1)) as u32);
    }

    // Delta reconstruction is LOSSLESS mod 2^16: `sample = ramp + delta` in
    // `u16`, so negative deltas wrap exactly like the SDK's `uint16` add
    // (`dng_big_table.cpp:2522-2524, 2552-2554`). Samples stay `u16`; nothing
    // is normalized to f32 here.
    let mut samples = Vec::with_capacity(node_count * SAMPLE_STRIDE);
    let mut offset = HEADER_BYTES;

    for index in 0..node_count {
        let (r_index, g_index, b_index) = match dimensions {
            DIMENSIONS_1D => (index, index, index),
            _ => (index / (size * size), (index / size) % size, index % size),
        };

        let red_delta = read_u16(block, offset, RGB_TABLE_LABEL)?;
        let green_delta = read_u16(block, offset + 2, RGB_TABLE_LABEL)?;
        let blue_delta = read_u16(block, offset + 4, RGB_TABLE_LABEL)?;
        offset += BYTES_PER_NODE;

        samples.push(identity_ramp[r_index] as u16);
        samples.push(identity_ramp[g_index] as u16);
        samples.push(identity_ramp[b_index] as u16);
        let base = samples.len() - 3;
        samples[base] = samples[base].wrapping_add(red_delta);
        samples[base + 1] = samples[base + 1].wrapping_add(green_delta);
        samples[base + 2] = samples[base + 2].wrapping_add(blue_delta);
        samples.push(0);
    }

    let color_space = read_u32(block, offset, RGB_TABLE_LABEL)?;
    let gamma = read_u32(block, offset + 4, RGB_TABLE_LABEL)?;
    let gamut = read_u32(block, offset + 8, RGB_TABLE_LABEL)?;
    offset += FOOTER_BYTES;

    let min_amount = read_f64(block, offset, RGB_TABLE_LABEL)?;
    let max_amount = read_f64(block, offset + 8, RGB_TABLE_LABEL)?;

    let flags = if flags_present {
        Some(read_u32(block, offset + AMOUNT_BYTES, RGB_TABLE_LABEL)?)
    } else {
        None
    };

    Ok(RgbTable {
        dimensions,
        size,
        samples,
        color_space,
        gamma,
        gamut,
        min_amount,
        max_amount,
        flags,
    })
}

/// Little-endian integer/float readers shared with the LookTable decoder.
///
/// `label` only affects the error text; it lets both decoders report truncation
/// against their own table name.
pub(super) fn read_u16(block: &[u8], offset: usize, label: &str) -> Result<u16, String> {
    Ok(u16::from_le_bytes(read_array::<2>(block, offset, label)?))
}

pub(super) fn read_u32(block: &[u8], offset: usize, label: &str) -> Result<u32, String> {
    Ok(u32::from_le_bytes(read_array::<4>(block, offset, label)?))
}

pub(super) fn read_f32(block: &[u8], offset: usize, label: &str) -> Result<f32, String> {
    Ok(f32::from_le_bytes(read_array::<4>(block, offset, label)?))
}

pub(super) fn read_f64(block: &[u8], offset: usize, label: &str) -> Result<f64, String> {
    Ok(f64::from_le_bytes(read_array::<8>(block, offset, label)?))
}

pub(super) fn read_array<const N: usize>(
    block: &[u8],
    offset: usize,
    label: &str,
) -> Result<[u8; N], String> {
    let end = offset
        .checked_add(N)
        .ok_or_else(|| format!("embedded {label} offset overflows usize"))?;

    block
        .get(offset..end)
        .ok_or_else(|| format!("embedded {label} block is truncated at offset {offset}"))?
        .try_into()
        .map_err(|_| format!("embedded {label} block is truncated at offset {offset}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY_2X2X2_BASE85: &str = "71000rtKmwBRLy1{X$/w9'=(bLC9YEvFE(9%a0@fg0";

    /// The identity corners as stored `u16` (RGB0 stride-4) samples.
    const EXPECTED_IDENTITY: [[u16; 3]; 8] = [
        [0, 0, 0],
        [0, 0, 65535],
        [0, 65535, 0],
        [0, 65535, 65535],
        [65535, 0, 0],
        [65535, 0, 65535],
        [65535, 65535, 0],
        [65535, 65535, 65535],
    ];

    fn build_block(size: u32, dimensions: u32) -> Vec<u8> {
        let node_count = if dimensions == 1 {
            size as usize
        } else {
            (size * size * size) as usize
        };
        let mut block = Vec::new();
        block.extend_from_slice(&1u32.to_le_bytes());
        block.extend_from_slice(&1u32.to_le_bytes());
        block.extend_from_slice(&dimensions.to_le_bytes());
        block.extend_from_slice(&size.to_le_bytes());
        for _ in 0..node_count {
            block.extend_from_slice(&0u16.to_le_bytes());
            block.extend_from_slice(&0u16.to_le_bytes());
            block.extend_from_slice(&0u16.to_le_bytes());
        }
        block.extend_from_slice(&0u32.to_le_bytes());
        block.extend_from_slice(&1u32.to_le_bytes());
        block.extend_from_slice(&0u32.to_le_bytes());
        block.extend_from_slice(&0f64.to_le_bytes());
        block.extend_from_slice(&1.5f64.to_le_bytes());
        block
    }

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

    #[test]
    fn decodes_identity_rgb_table_2x2x2() {
        let table = decode_adobe_rgb_table(IDENTITY_2X2X2_BASE85)
            .expect("fixture should decode into a 2x2x2 identity table");

        assert_eq!(table.dimensions, 3);
        assert_eq!(table.size, 2);
        assert_eq!(table.samples.len(), 8 * 4);
        assert_eq!(table.color_space, 0);
        assert_eq!(table.gamma, 1);
        assert_eq!(table.gamut, 0);
        assert_eq!(table.min_amount, 0.0);
        assert_eq!(table.max_amount, 1.5);
        assert_eq!(table.flags, None);

        for (index, expected) in EXPECTED_IDENTITY.iter().enumerate() {
            let actual = &table.samples[index * 4..index * 4 + 3];
            assert_eq!(
                actual, expected,
                "node {index} must reconstruct the exact u16 identity ramp"
            );
        }
    }

    #[test]
    fn decodes_one_dimensional_rgb_table() {
        // A legal Adobe 1-D table (`dimensions == 1`, divisions 2..=4096):
        // `dng_rgb_table::GetStream` reads `divisions` samples of 3 u16 deltas
        // each against the SAME per-index identity ramp.
        const DIVISIONS: u32 = 16;
        let block = build_block(DIVISIONS, 1);
        let table = parse_uncompressed_block(&block).expect("1-D table must decode");

        assert_eq!(table.dimensions, 1);
        assert_eq!(table.size, DIVISIONS as usize);
        assert_eq!(table.samples.len(), DIVISIONS as usize * 4);
        // Zero deltas reconstruct the identity ramp on every channel.
        for index in 0..DIVISIONS as usize {
            let ramp =
                ((index * 65535 + (DIVISIONS as usize / 2)) / (DIVISIONS as usize - 1)) as u16;
            assert_eq!(
                &table.samples[index * 4..index * 4 + 3],
                &[ramp, ramp, ramp],
                "1-D node {index}"
            );
        }
    }

    #[test]
    fn rejects_one_dimensional_divisions_out_of_range() {
        // 1-D bounds are 2..=4096 (`kMinDivisions1D`/`kMaxDivisions1D`), NOT the
        // 3-D 2..=64 window: a 100-division 1-D table is legal, a 4097-division
        // one is not.
        let block = build_block(100, 1);
        parse_uncompressed_block(&block).expect("100-division 1-D table must decode");

        let block = build_block(4097, 1);
        assert!(
            parse_uncompressed_block(&block).is_err(),
            "4097 1-D divisions must be rejected"
        );
    }

    #[test]
    fn rejects_unsupported_dimensions() {
        // Only 1 and 3 are legal `fDimensions` (`dng_big_table.cpp:2463-2498`).
        for dimensions in [0u32, 2, 4, u32::MAX] {
            let block = build_block(2, dimensions);
            let error =
                parse_uncompressed_block(&block).expect_err("illegal dimensions must be rejected");
            assert!(
                error.contains("unsupported embedded RGBTable dimensions"),
                "dimensions {dimensions}: unexpected error: {error}"
            );
        }
    }

    #[test]
    fn delta_decoding_wraps_modulo_2_to_the_16() {
        // A "negative" stored delta wraps mod 2^16 exactly like the SDK's
        // `uint16` add: sample = (ramp + delta) mod 65536. With 4 divisions,
        // ramp[0] = (0 * 65535 + 2) / 3 = 0, so delta 0xFFFF yields 65535.
        let mut block = build_block(4, 3);
        // First node's red delta sits at offset HEADER_BYTES.
        block[16] = 0xFF;
        block[17] = 0xFF;
        let table = parse_uncompressed_block(&block).expect("decode");
        assert_eq!(table.samples[0], 65535, "negative delta must wrap mod 2^16");
    }

    #[test]
    fn decodes_optional_flags_word() {
        // `dng_big_table.cpp:2606-2609`: when exactly 4 bytes remain after the
        // amounts, the stream carries the optional u32 flags word.
        let mut block = build_block(2, 3);
        block.extend_from_slice(&0x07u32.to_le_bytes());
        let table = parse_uncompressed_block(&block).expect("flags block must decode");
        assert_eq!(table.flags, Some(7));

        // And three trailing bytes is still malformed -- neither a bare block
        // nor a flags-terminated one.
        let mut block = build_block(2, 3);
        block.extend_from_slice(&[0xAA; 3]);
        assert!(parse_uncompressed_block(&block).is_err());
    }

    #[test]
    fn rejects_invalid_base85_character() {
        let encoded = "71000~tKmwBRLy1{X$/w9'=(bLC9YEvFE(9%a0@fg0";
        assert!(decode_adobe_rgb_table(encoded).is_err());
    }

    #[test]
    fn rejects_single_character_final_base85_group() {
        assert!(decode_adobe_rgb_table("012345").is_err());
    }

    #[test]
    fn rejects_invalid_rgb_table_dimensions() {
        let block = build_block(2, 2);
        assert!(parse_uncompressed_block(&block).is_err());
    }

    #[test]
    fn rejects_rgb_table_length_mismatch() {
        let mut block = build_block(2, 3);
        block.truncate(block.len() - 1);
        assert!(parse_uncompressed_block(&block).is_err());
    }

    #[test]
    fn rejects_trailing_bytes_after_zlib_stream() {
        let block = build_block(2, 3);

        let mut payload = (block.len() as u32).to_le_bytes().to_vec();
        payload.extend_from_slice(&zlib_compress(&block));
        payload.push(0x00);

        let encoded = base85_encode(&payload);
        assert!(decode_adobe_rgb_table(&encoded).is_err());
    }

    #[test]
    fn rejects_declared_rgb_table_size_above_supported_maximum() {
        let block = build_block(2, 3);
        let encoded = encode_payload(u32::MAX, &block);

        let error = decode_adobe_rgb_table(&encoded)
            .expect_err("declared size above the supported maximum must be rejected");

        assert!(
            error.contains("embedded RGBTable declared size exceeds supported maximum"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_declared_rgb_table_size_below_supported_minimum() {
        let encoded = encode_payload(1, &[0u8; 1]);

        let error = decode_adobe_rgb_table(&encoded)
            .expect_err("declared size below the supported minimum must be rejected");

        assert!(
            error.contains("embedded RGBTable declared size is below the supported minimum"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn decodes_33_and_64_division_rgb_tables() {
        // 33 sat just above the old 32 ceiling; 64 is Adobe's real read/write
        // ceiling (`kMaxDivisions3D_InMemory`, `dng_big_table.h:623`). Both must
        // now decode, with no fallback and an exact node count.
        for size in [33u32, 64u32] {
            let block = build_block(size, 3);
            let table = parse_uncompressed_block(&block)
                .unwrap_or_else(|error| panic!("size {size} must decode: {error}"));
            assert_eq!(table.size, size as usize, "decoded size");
            assert_eq!(
                table.node_count(),
                (size as usize) * (size as usize) * (size as usize),
                "decoded node count for size {size}"
            );
        }
    }

    #[test]
    fn rejects_65_division_rgb_table() {
        // One past Adobe's ceiling: still rejected, by SHAPE (it is a 3-D table,
        // so the block-length window would admit its byte size).
        let block = build_block(65, 3);
        let error = parse_uncompressed_block(&block).expect_err("65 divisions must be rejected");
        assert!(
            error.contains("unsupported embedded RGBTable size 65"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn decodes_64_division_rgb_table_over_the_wire() {
        // End-to-end, exactly what the parser/discovery see: Adobe base85 + a
        // 4-byte length prefix + zlib. The declared size (~1.5 MB) must clear the
        // pre-allocation guard and the decompressed block must match it exactly.
        let size = 64u32;
        let block = build_block(size, 3);
        let encoded = encode_payload(block.len() as u32, &block);

        let table = decode_adobe_rgb_table(&encoded).expect("a 64-division wire table must decode");
        assert_eq!(table.size, 64);
        assert_eq!(table.node_count(), 64 * 64 * 64);

        // Zero deltas reconstruct each axis's identity ramp, so the corner nodes
        // are exact...
        assert_eq!(table.node(0), [0.0, 0.0, 0.0], "first corner");
        assert_eq!(table.node(64 * 64 * 64 - 1), [1.0, 1.0, 1.0], "last corner");
        // ...and an interior node indexes back to its own coordinate (within one
        // 16-bit reconstruction step, ~1.5e-5, of the ideal ramp), which fails if
        // the node ordering or the size stride were wrong.
        const RAMP_TOLERANCE: f32 = 1e-4;
        let (r, g, b) = (10usize, 20usize, 30usize);
        let node = table.node(r * 64 * 64 + g * 64 + b);
        let scale = 63.0f32;
        for (channel, value, expected) in [
            (0usize, node[0], r as f32 / scale),
            (1, node[1], g as f32 / scale),
            (2, node[2], b as f32 / scale),
        ] {
            assert!(
                (value - expected).abs() < RAMP_TOLERANCE,
                "interior node {r},{g},{b} channel {channel}: {value} vs {expected}"
            );
        }
    }
}
