use flate2::{Decompress, FlushDecompress, Status};

const ADOBE_BASE85_ALPHABET: &[u8; 85] =
    b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.-:+=^!/*?`'|()[]{}@%$#";

const BASE85_LOOKUP: [u8; 256] = build_base85_lookup();

const POW85: [u64; 5] = [1, 85, 85 * 85, 85 * 85 * 85, 85 * 85 * 85 * 85];

const INVALID_BASE85_DIGIT: u8 = 0xFF;

const HEADER_BYTES: usize = 16;
const FOOTER_BYTES: usize = 12;
const AMOUNT_BYTES: usize = 16;
const BYTES_PER_NODE: usize = 6;
const MIN_SIZE: u32 = 2;
const MAX_SIZE: u32 = 32;

/// BigTableTypeEnum::btt_RGBTable.
const RGB_TABLE_TYPE: u32 = 1;

/// dng_rgb_table::kRGBTableVersion.
const RGB_TABLE_VERSION: u32 = 1;

const SUPPORTED_DIMENSIONS: u32 = 3;
const CHANNEL_LEVELS: u32 = 65536;
const CHANNEL_MAX: u32 = 65535;

const MAX_BLOCK_BYTES: usize = HEADER_BYTES
    + (MAX_SIZE as usize * MAX_SIZE as usize * MAX_SIZE as usize * BYTES_PER_NODE)
    + FOOTER_BYTES
    + AMOUNT_BYTES;

const MIN_BLOCK_BYTES: usize = HEADER_BYTES
    + (MIN_SIZE as usize * MIN_SIZE as usize * MIN_SIZE as usize * BYTES_PER_NODE)
    + FOOTER_BYTES
    + AMOUNT_BYTES;

const fn build_base85_lookup() -> [u8; 256] {
    let mut table = [INVALID_BASE85_DIGIT; 256];
    let mut index = 0usize;
    while index < ADOBE_BASE85_ALPHABET.len() {
        table[ADOBE_BASE85_ALPHABET[index] as usize] = index as u8;
        index += 1;
    }
    table
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RgbTable {
    pub size: usize,
    pub values: Vec<[f32; 3]>,
    pub color_space: u32,
    pub gamma: u32,
    pub gamut: u32,
    pub min_amount: f64,
    pub max_amount: f64,
}

pub(crate) fn decode_adobe_rgb_table(encoded: &str) -> Result<RgbTable, String> {
    let compressed = decode_base85(encoded)?;

    if compressed.len() < 4 {
        return Err(
            "embedded RGBTable payload is shorter than its 4-byte length prefix".to_string(),
        );
    }

    let expected_size =
        u32::from_le_bytes([compressed[0], compressed[1], compressed[2], compressed[3]]);
    let expected_size = expected_size as usize;

    if expected_size > MAX_BLOCK_BYTES {
        return Err("embedded RGBTable declared size exceeds supported maximum".to_string());
    }

    if expected_size < MIN_BLOCK_BYTES {
        return Err("embedded RGBTable declared size is below the supported minimum".to_string());
    }

    let stream = &compressed[4..];
    let mut block = vec![0u8; expected_size];

    let mut decoder = Decompress::new(true);
    let status = decoder
        .decompress(stream, &mut block, FlushDecompress::Finish)
        .map_err(|error| format!("failed to decompress embedded RGBTable: {error}"))?;

    if status != Status::StreamEnd {
        return Err("embedded RGBTable zlib stream is incomplete".to_string());
    }

    if decoder.total_out() != expected_size as u64 {
        return Err(format!(
            "embedded RGBTable size mismatch: length prefix declares {expected_size} bytes, decompressed {} bytes",
            decoder.total_out()
        ));
    }

    if decoder.total_in() != stream.len() as u64 {
        return Err(
            "embedded RGBTable payload contains trailing bytes after the zlib stream".to_string(),
        );
    }

    parse_uncompressed_block(&block)
}

fn decode_base85(encoded: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(encoded.len() / 5 * 4 + 4);
    let mut value: u64 = 0;
    let mut phase = 0usize;

    for &byte in encoded.as_bytes() {
        let digit = BASE85_LOOKUP[byte as usize];
        if digit == INVALID_BASE85_DIGIT {
            return Err(format!(
                "invalid Adobe Base85 character '{}' in embedded RGBTable",
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
    let table_type = read_u32(block, 0)?;
    let table_version = read_u32(block, 4)?;
    let dimensions = read_u32(block, 8)?;
    let size_field = read_u32(block, 12)?;

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

    if dimensions != SUPPORTED_DIMENSIONS {
        return Err(format!(
            "unsupported embedded RGBTable dimensions {dimensions}, expected {SUPPORTED_DIMENSIONS}"
        ));
    }

    if !(MIN_SIZE..=MAX_SIZE).contains(&size_field) {
        return Err(format!(
            "unsupported embedded RGBTable size {size_field}, expected {MIN_SIZE}..={MAX_SIZE}"
        ));
    }

    let size = size_field as usize;

    let node_count = size
        .checked_mul(size)
        .and_then(|nodes| nodes.checked_mul(size))
        .ok_or_else(|| "embedded RGBTable node count overflows usize".to_string())?;

    let sample_bytes = node_count
        .checked_mul(BYTES_PER_NODE)
        .ok_or_else(|| "embedded RGBTable sample byte count overflows usize".to_string())?;

    let expected_block_size = HEADER_BYTES
        .checked_add(sample_bytes)
        .and_then(|total| total.checked_add(FOOTER_BYTES))
        .and_then(|total| total.checked_add(AMOUNT_BYTES))
        .ok_or_else(|| "embedded RGBTable block size overflows usize".to_string())?;

    if block.len() != expected_block_size {
        return Err(format!(
            "embedded RGBTable block size mismatch: expected {expected_block_size} bytes, got {} bytes",
            block.len()
        ));
    }

    let mut identity_ramp = Vec::with_capacity(size);
    for index in 0..size {
        let scaled = index
            .checked_mul(CHANNEL_MAX as usize)
            .and_then(|value| value.checked_add(size / 2))
            .ok_or_else(|| "embedded RGBTable identity ramp overflows usize".to_string())?;
        identity_ramp.push((scaled / (size - 1)) as u32);
    }

    let mut values = Vec::with_capacity(node_count);
    let mut offset = HEADER_BYTES;

    for index in 0..node_count {
        let r_index = index / (size * size);
        let g_index = (index / size) % size;
        let b_index = index % size;

        let red_delta = read_u16(block, offset)?;
        let green_delta = read_u16(block, offset + 2)?;
        let blue_delta = read_u16(block, offset + 4)?;
        offset += BYTES_PER_NODE;

        values.push([
            reconstruct_channel(red_delta, identity_ramp[r_index]),
            reconstruct_channel(green_delta, identity_ramp[g_index]),
            reconstruct_channel(blue_delta, identity_ramp[b_index]),
        ]);
    }

    let color_space = read_u32(block, offset)?;
    let gamma = read_u32(block, offset + 4)?;
    let gamut = read_u32(block, offset + 8)?;
    offset += FOOTER_BYTES;

    let min_amount = read_f64(block, offset)?;
    let max_amount = read_f64(block, offset + 8)?;

    Ok(RgbTable {
        size,
        values,
        color_space,
        gamma,
        gamut,
        min_amount,
        max_amount,
    })
}

fn reconstruct_channel(delta: u16, identity: u32) -> f32 {
    let decoded = (u32::from(delta) + identity) % CHANNEL_LEVELS;
    decoded as f32 / CHANNEL_MAX as f32
}

fn read_u16(block: &[u8], offset: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(read_array::<2>(block, offset)?))
}

fn read_u32(block: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(read_array::<4>(block, offset)?))
}

fn read_f64(block: &[u8], offset: usize) -> Result<f64, String> {
    Ok(f64::from_le_bytes(read_array::<8>(block, offset)?))
}

fn read_array<const N: usize>(block: &[u8], offset: usize) -> Result<[u8; N], String> {
    let end = offset
        .checked_add(N)
        .ok_or_else(|| "embedded RGBTable offset overflows usize".to_string())?;

    block
        .get(offset..end)
        .ok_or_else(|| format!("embedded RGBTable block is truncated at offset {offset}"))?
        .try_into()
        .map_err(|_| format!("embedded RGBTable block is truncated at offset {offset}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY_2X2X2_BASE85: &str = "71000rtKmwBRLy1{X$/w9'=(bLC9YEvFE(9%a0@fg0";

    const EXPECTED_IDENTITY: [[f32; 3]; 8] = [
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 1.0, 0.0],
        [0.0, 1.0, 1.0],
        [1.0, 0.0, 0.0],
        [1.0, 0.0, 1.0],
        [1.0, 1.0, 0.0],
        [1.0, 1.0, 1.0],
    ];

    fn assert_channel(actual: f32, expected: f32, context: &str) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "{context}: expected {expected}, got {actual}"
        );
    }

    fn build_block(size: u32, dimensions: u32) -> Vec<u8> {
        let node_count = (size * size * size) as usize;
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

        assert_eq!(table.size, 2);
        assert_eq!(table.values.len(), 8);
        assert_eq!(table.color_space, 0);
        assert_eq!(table.gamma, 1);
        assert_eq!(table.gamut, 0);
        assert_eq!(table.min_amount, 0.0);
        assert_eq!(table.max_amount, 1.5);

        for (index, expected) in EXPECTED_IDENTITY.iter().enumerate() {
            let actual = table.values[index];
            assert_channel(actual[0], expected[0], &format!("node {index} red"));
            assert_channel(actual[1], expected[1], &format!("node {index} green"));
            assert_channel(actual[2], expected[2], &format!("node {index} blue"));
        }
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
}
