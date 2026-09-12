use std::collections::HashMap;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::xmp_profile::look_table::{LookTable, decode_adobe_look_table};
use crate::xmp_profile::rgb_table::{RgbTable, decode_adobe_rgb_table};

const CRS_PRESET_TYPE: &[u8] = b"crs:PresetType";
const CRS_UUID: &[u8] = b"crs:UUID";
const CRS_PROCESS_VERSION: &[u8] = b"crs:ProcessVersion";
const CRS_SUPPORTS_AMOUNT: &[u8] = b"crs:SupportsAmount";
const CRS_CONVERT_TO_GRAYSCALE: &[u8] = b"crs:ConvertToGrayscale";
const CRS_RGB_TABLE: &[u8] = b"crs:RGBTable";
const CRS_LOOK_TABLE: &[u8] = b"crs:LookTable";
const CRS_RGB_TABLE_AMOUNT: &[u8] = b"crs:RGBTableAmount";
const CRS_TABLE_PREFIX: &[u8] = b"crs:Table_";
const CRS_NAME: &[u8] = b"crs:Name";
const CRS_GROUP: &[u8] = b"crs:Group";
const RDF_LI: &[u8] = b"rdf:li";

const EXPECTED_PRESET_TYPE: &str = "Look";
const TRUE_ATTRIBUTE: &str = "True";
const FALSE_ATTRIBUTE: &str = "False";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct XmpRgbProfile {
    pub name: String,
    pub group: Option<String>,
    pub uuid: String,
    pub process_version: Option<String>,
    pub supports_amount: bool,
    /// The authored `crs:ConvertToGrayscale` develop flag.
    ///
    /// This is a *rendering-unit* flag ("Treatment = Black & White"), driven by
    /// the user-adjustable 8-band `crs:GrayMixer*`, not a camera-profile
    /// property: it has zero occurrences in DNG Spec 1.7.1.0 and zero call sites
    /// in the DNG SDK. Once-Lab therefore carries it faithfully and does NOT
    /// invent a gray conversion for it, so a grayscale profile renders exactly
    /// like the equivalent color profile.
    pub convert_to_grayscale: bool,
    pub rgb_table_id: String,
    pub rgb_table_amount: Option<f32>,
    pub table: RgbTable,
    /// The `crs:LookTable` id, if the profile authors one.
    pub look_table_id: Option<String>,
    /// The decoded `crs:LookTable`, if the profile authors one.
    pub look_table: Option<LookTable>,
}

#[derive(Default)]
struct CollectedAttributes {
    preset_type: Option<String>,
    uuid: Option<String>,
    process_version: Option<String>,
    supports_amount: Option<String>,
    convert_to_grayscale: Option<String>,
    rgb_table: Option<String>,
    look_table: Option<String>,
    rgb_table_amount: Option<String>,
    tables: HashMap<String, String>,
}

enum Section {
    Name,
    Group,
}

pub(crate) fn parse_xmp_rgb_profile(xmp: &str) -> Result<XmpRgbProfile, String> {
    let mut reader = Reader::from_str(xmp);
    let mut collected = CollectedAttributes::default();
    let mut name: Option<String> = None;
    let mut group: Option<String> = None;
    let mut section: Option<Section> = None;
    let mut inside_li = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|error| format!("failed to parse XMP document: {error}"))?;

        match event {
            Event::Start(start) => {
                collect_attributes(&start, &mut collected)?;

                let element = start.name();
                let element = element.as_ref();

                if element == CRS_NAME {
                    section = Some(Section::Name);
                    inside_li = false;
                } else if element == CRS_GROUP {
                    section = Some(Section::Group);
                    inside_li = false;
                } else if section.is_some() && element == RDF_LI {
                    inside_li = true;
                }
            }
            Event::Empty(start) => {
                collect_attributes(&start, &mut collected)?;
            }
            Event::Text(text) => {
                if let Some(current) = section.as_ref()
                    && inside_li
                {
                    let content = text
                        .xml10_content()
                        .map_err(|error| format!("failed to decode XMP text: {error}"))?;
                    let unescaped = quick_xml::escape::unescape(&content)
                        .map_err(|error| format!("failed to unescape XMP text: {error}"))?;

                    match current {
                        Section::Name => append_text(&mut name, unescaped.as_ref()),
                        Section::Group => append_text(&mut group, unescaped.as_ref()),
                    }
                }
            }
            Event::CData(data) => {
                if let Some(current) = section.as_ref()
                    && inside_li
                {
                    let content = data
                        .xml10_content()
                        .map_err(|error| format!("failed to decode XMP CDATA: {error}"))?;

                    match current {
                        Section::Name => append_text(&mut name, content.as_ref()),
                        Section::Group => append_text(&mut group, content.as_ref()),
                    }
                }
            }
            Event::End(end) => {
                let element = end.name();
                let element = element.as_ref();

                if element == RDF_LI {
                    inside_li = false;
                } else if element == CRS_NAME || element == CRS_GROUP {
                    section = None;
                    inside_li = false;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    match collected.preset_type.as_deref() {
        Some(EXPECTED_PRESET_TYPE) => {}
        _ => return Err("unsupported XMP PresetType; expected Look".to_string()),
    }

    let uuid = collected.uuid.unwrap_or_default();
    if uuid.is_empty() {
        return Err("missing crs:UUID".to_string());
    }

    let name = name.unwrap_or_default().trim().to_string();
    if name.is_empty() {
        return Err("missing crs:Name".to_string());
    }

    let supports_amount =
        parse_xmp_bool(collected.supports_amount.as_deref(), "crs:SupportsAmount")?;
    let convert_to_grayscale = parse_xmp_bool(
        collected.convert_to_grayscale.as_deref(),
        "crs:ConvertToGrayscale",
    )?;

    // `crs:ConvertToGrayscale` is carried, never rejected: it is an ACR develop
    // setting ("Treatment = Black & White") with no canonical gray conversion in
    // the DNG SDK, so a grayscale profile renders exactly like the equivalent
    // color profile rather than through an invented conversion.

    let rgb_table_id = collected
        .rgb_table
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "missing crs:RGBTable".to_string())?;

    let encoded = collected
        .tables
        .get(&rgb_table_id)
        .ok_or_else(|| "missing embedded RGBTable data".to_string())?;

    let table = decode_adobe_rgb_table(encoded)?;

    // The LookTable rides the same `crs:Table_<id>` map as the RGBTable. An
    // authored `crs:LookTable` whose data is absent is a broken profile, not a
    // profile without a look, so it is an explicit error naming the id.
    let look_table_id = collected.look_table.filter(|id| !id.is_empty());

    let look_table = match look_table_id.as_deref() {
        Some(id) => {
            let encoded = collected
                .tables
                .get(id)
                .ok_or_else(|| format!("missing embedded LookTable data for crs:LookTable=\"{id}\""))?;

            Some(decode_adobe_look_table(encoded)?)
        }
        None => None,
    };

    let rgb_table_amount = match collected.rgb_table_amount {
        Some(raw) => {
            let amount = raw
                .parse::<f32>()
                .map_err(|_| "invalid crs:RGBTableAmount".to_string())?;

            if !amount.is_finite() {
                return Err("invalid crs:RGBTableAmount".to_string());
            }

            Some(amount)
        }
        None => None,
    };

    let group = group
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    Ok(XmpRgbProfile {
        name,
        group,
        uuid,
        process_version: collected.process_version,
        supports_amount,
        convert_to_grayscale,
        rgb_table_id,
        rgb_table_amount,
        table,
        look_table_id,
        look_table,
    })
}

fn collect_attributes(
    start: &BytesStart<'_>,
    collected: &mut CollectedAttributes,
) -> Result<(), String> {
    for attribute in start.attributes() {
        let attribute =
            attribute.map_err(|error| format!("failed to parse XMP attribute: {error}"))?;

        let key = attribute.key.as_ref();
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| format!("failed to decode XMP attribute value: {error}"))?
            .into_owned();

        if key == CRS_PRESET_TYPE {
            collected.preset_type = Some(value);
        } else if key == CRS_UUID {
            collected.uuid = Some(value);
        } else if key == CRS_PROCESS_VERSION {
            collected.process_version = Some(value);
        } else if key == CRS_SUPPORTS_AMOUNT {
            collected.supports_amount = Some(value);
        } else if key == CRS_CONVERT_TO_GRAYSCALE {
            collected.convert_to_grayscale = Some(value);
        } else if key == CRS_RGB_TABLE {
            collected.rgb_table = Some(value);
        } else if key == CRS_LOOK_TABLE {
            collected.look_table = Some(value);
        } else if key == CRS_RGB_TABLE_AMOUNT {
            collected.rgb_table_amount = Some(value);
        } else if let Some(id) = key.strip_prefix(CRS_TABLE_PREFIX)
            && let Ok(id) = std::str::from_utf8(id)
        {
            collected.tables.insert(id.to_string(), value);
        }
    }

    Ok(())
}

fn append_text(target: &mut Option<String>, value: &str) {
    match target {
        Some(existing) => existing.push_str(value),
        None => *target = Some(value.to_string()),
    }
}

fn parse_xmp_bool(value: Option<&str>, field_name: &str) -> Result<bool, String> {
    match value {
        None => Ok(false),
        Some(TRUE_ATTRIBUTE) => Ok(true),
        Some(FALSE_ATTRIBUTE) => Ok(false),
        Some(_) => Err(format!("invalid {field_name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY_2X2X2_BASE85: &str = "71000rtKmwBRLy1{X$/w9'=(bLC9YEvFE(9%a0@fg0";

    /// Id used for the synthetic LookTable in these fixtures.
    const LOOK_TABLE_ID: &str = "LOOKTABLEID";

    fn encode_group(out: &mut String, bytes: &[u8]) {
        use crate::xmp_profile::rgb_table::ADOBE_BASE85_ALPHABET;

        let mut value = 0u32;
        for (index, byte) in bytes.iter().enumerate() {
            value += u32::from(*byte) << (8 * index);
        }

        for _ in 0..bytes.len() + 1 {
            out.push(ADOBE_BASE85_ALPHABET[(value % 85) as usize] as char);
            value /= 85;
        }
    }

    fn base85_encode(data: &[u8]) -> String {
        let mut out = String::new();
        let full = data.len() - (data.len() % 4);

        for chunk in data[..full].chunks(4) {
            encode_group(&mut out, chunk);
        }

        if !data[full..].is_empty() {
            encode_group(&mut out, &data[full..]);
        }

        out
    }

    fn encode_payload(declared_size: u32, body: &[u8]) -> String {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write;

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body).expect("test zlib compression");

        let mut payload = declared_size.to_le_bytes().to_vec();
        payload.extend_from_slice(&encoder.finish().expect("test zlib finish"));

        base85_encode(&payload)
    }

    /// A minimal version-1 LookTable (1 hue x 2 sat x 1 val = 2 entries).
    fn look_table_base85(entries: &[[f32; 3]], encoding: u32) -> String {
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes()); // btt_LookTable
        body.extend_from_slice(&1u32.to_le_bytes()); // version
        body.extend_from_slice(&1u32.to_le_bytes()); // hueDivisions
        body.extend_from_slice(&2u32.to_le_bytes()); // satDivisions
        body.extend_from_slice(&1u32.to_le_bytes()); // valDivisions
        for entry in entries {
            for component in entry {
                body.extend_from_slice(&component.to_le_bytes());
            }
        }
        body.extend_from_slice(&encoding.to_le_bytes());

        encode_payload(body.len() as u32, &body)
    }

    /// 2 entries in Adobe wire order (value outermost, hue, saturation innermost).
    fn look_table_entries() -> [[f32; 3]; 2] {
        [[0.0, 1.0, 1.0], [30.0, 1.0, 2.0]]
    }

    fn color_profile_xmp() -> String {
        format!(
            r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:PresetType="Look"
      crs:UUID="TEST123"
      crs:ProcessVersion="11.0"
      crs:SupportsAmount="True"
      crs:ConvertToGrayscale="False"
      crs:RGBTable="TESTTABLE"
      crs:RGBTableAmount="0.5"
      crs:Table_TESTTABLE="{table}">
      <crs:Name>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">Test Profile</rdf:li>
        </rdf:Alt>
      </crs:Name>
      <crs:Group>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">Test Group</rdf:li>
        </rdf:Alt>
      </crs:Group>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#,
            table = IDENTITY_2X2X2_BASE85
        )
    }

    fn minimal_xmp(extra_attributes: &str) -> String {
        format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:PresetType="Look"
      crs:UUID="TEST123"
      {extra_attributes}>
      <crs:Name>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">Test Profile</rdf:li>
        </rdf:Alt>
      </crs:Name>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#
        )
    }

    #[test]
    fn parses_color_xmp_profile_with_embedded_rgb_table() {
        let profile = parse_xmp_rgb_profile(&color_profile_xmp())
            .expect("synthetic color XMP profile should parse");

        assert_eq!(profile.name, "Test Profile");
        assert_eq!(profile.group, Some("Test Group".to_string()));
        assert_eq!(profile.uuid, "TEST123");
        assert_eq!(profile.process_version, Some("11.0".to_string()));
        assert!(profile.supports_amount);
        assert!(!profile.convert_to_grayscale);
        assert_eq!(profile.rgb_table_id, "TESTTABLE");
        assert_eq!(profile.rgb_table_amount, Some(0.5));
        assert_eq!(profile.look_table_id, None);
        assert_eq!(profile.look_table, None);
        assert_eq!(profile.table.size, 2);
        assert_eq!(profile.table.values.len(), 8);
        assert_eq!(profile.table.values[0], [0.0, 0.0, 0.0]);
        assert_eq!(profile.table.values[7], [1.0, 1.0, 1.0]);
    }

    #[test]
    fn parses_grayscale_xmp_profile_and_reports_the_flag() {
        // A grayscale profile with BOTH an RGBTable and a LookTable, exactly the
        // Fe-class shape. Previously this was rejected outright; it must now parse
        // and carry the flag verbatim without inventing a gray conversion.
        let xmp = minimal_xmp(&format!(
            r#"crs:SupportsAmount="True"
      crs:ConvertToGrayscale="True"
      crs:RGBTable="TESTTABLE"
      crs:LookTable="{look_id}"
      crs:Table_TESTTABLE="{rgb}"
      crs:Table_{look_id}="{look}""#,
            look_id = LOOK_TABLE_ID,
            rgb = IDENTITY_2X2X2_BASE85,
            look = look_table_base85(&look_table_entries(), 0),
        ));

        let profile =
            parse_xmp_rgb_profile(&xmp).expect("a grayscale profile must now parse, not error");

        assert!(profile.convert_to_grayscale, "the flag must be reported as true");
        assert!(profile.supports_amount);
        assert_eq!(profile.rgb_table_id, "TESTTABLE");
        assert_eq!(profile.rgb_table_amount, None, "Fe declares no authored amount");

        assert_eq!(profile.look_table_id.as_deref(), Some(LOOK_TABLE_ID));
        let look = profile.look_table.as_ref().expect("the LookTable must be decoded");
        assert_eq!(look.hue_divisions, 1);
        assert_eq!(look.sat_divisions, 2);
        assert_eq!(look.val_divisions, 1);
        assert_eq!(look.encoding, 0);
        assert_eq!(look.entries.len(), 2);
        assert_eq!(look.entries[1], [30.0, 1.0, 2.0]);

        // Non-vacuity: the same document with the flag flipped parses identically
        // apart from the flag, proving the flag is the only thing that changed.
        let color = xmp.replace("crs:ConvertToGrayscale=\"True\"", "crs:ConvertToGrayscale=\"False\"");
        let color_profile = parse_xmp_rgb_profile(&color).expect("color variant must parse");
        assert!(!color_profile.convert_to_grayscale);
        assert_eq!(color_profile.look_table.as_ref(), profile.look_table.as_ref());
        assert_eq!(&color_profile.table, &profile.table);
    }

    #[test]
    fn rejects_xmp_when_referenced_look_table_is_missing() {
        let xmp = minimal_xmp(&format!(
            r#"crs:RGBTable="TESTTABLE"
      crs:LookTable="MISSING"
      crs:Table_TESTTABLE="{rgb}""#,
            rgb = IDENTITY_2X2X2_BASE85,
        ));

        let error = parse_xmp_rgb_profile(&xmp)
            .expect_err("an authored LookTable without data must be rejected");
        assert!(
            error.contains("missing embedded LookTable data for crs:LookTable=\"MISSING\""),
            "unexpected error: {error}"
        );

        // Non-vacuity: adding the referenced table data makes the same document
        // parse, so the rejection above is the missing data, not the shape.
        let with_data = minimal_xmp(&format!(
            r#"crs:RGBTable="TESTTABLE"
      crs:LookTable="{look_id}"
      crs:Table_TESTTABLE="{rgb}"
      crs:Table_{look_id}="{look}""#,
            look_id = LOOK_TABLE_ID,
            rgb = IDENTITY_2X2X2_BASE85,
            look = look_table_base85(&look_table_entries(), 0),
        ));
        assert!(parse_xmp_rgb_profile(&with_data).is_ok());
    }

    #[test]
    fn rejects_xmp_when_referenced_rgb_table_is_missing() {
        let xmp = minimal_xmp(r#"crs:RGBTable="MISSING""#);
        assert!(parse_xmp_rgb_profile(&xmp).is_err());
    }

    #[test]
    fn rejects_invalid_supports_amount_boolean() {
        let xmp = minimal_xmp(r#"crs:SupportsAmount="TRUE""#);
        let error =
            parse_xmp_rgb_profile(&xmp).expect_err("non-Adobe boolean spelling must be rejected");

        assert_eq!(error, "invalid crs:SupportsAmount");
    }

    #[test]
    fn rejects_invalid_convert_to_grayscale_boolean() {
        let xmp = minimal_xmp(r#"crs:ConvertToGrayscale="yes""#);
        let error =
            parse_xmp_rgb_profile(&xmp).expect_err("non-Adobe boolean spelling must be rejected");

        assert_eq!(error, "invalid crs:ConvertToGrayscale");
    }
}
