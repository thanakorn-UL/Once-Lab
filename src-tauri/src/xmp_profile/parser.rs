use std::collections::HashMap;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::xmp_profile::rgb_table::{RgbTable, decode_adobe_rgb_table};

const CRS_PRESET_TYPE: &[u8] = b"crs:PresetType";
const CRS_UUID: &[u8] = b"crs:UUID";
const CRS_PROCESS_VERSION: &[u8] = b"crs:ProcessVersion";
const CRS_SUPPORTS_AMOUNT: &[u8] = b"crs:SupportsAmount";
const CRS_CONVERT_TO_GRAYSCALE: &[u8] = b"crs:ConvertToGrayscale";
const CRS_RGB_TABLE: &[u8] = b"crs:RGBTable";
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
    pub convert_to_grayscale: bool,
    pub rgb_table_id: String,
    pub rgb_table_amount: Option<f32>,
    pub table: RgbTable,
}

#[derive(Default)]
struct CollectedAttributes {
    preset_type: Option<String>,
    uuid: Option<String>,
    process_version: Option<String>,
    supports_amount: Option<String>,
    convert_to_grayscale: Option<String>,
    rgb_table: Option<String>,
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

    if convert_to_grayscale {
        return Err("grayscale XMP profile is not supported in this milestone".to_string());
    }

    let rgb_table_id = collected
        .rgb_table
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "missing crs:RGBTable".to_string())?;

    let encoded = collected
        .tables
        .get(&rgb_table_id)
        .ok_or_else(|| "missing embedded RGBTable data".to_string())?;

    let table = decode_adobe_rgb_table(encoded)?;

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
        assert_eq!(profile.table.size, 2);
        assert_eq!(profile.table.values.len(), 8);
        assert_eq!(profile.table.values[0], [0.0, 0.0, 0.0]);
        assert_eq!(profile.table.values[7], [1.0, 1.0, 1.0]);
    }

    #[test]
    fn rejects_grayscale_xmp_profile_for_now() {
        let xmp = minimal_xmp(r#"crs:ConvertToGrayscale="True""#);
        let error = parse_xmp_rgb_profile(&xmp).expect_err("grayscale profile must be rejected");

        assert!(
            error.contains("grayscale XMP profile is not supported in this milestone"),
            "unexpected error: {error}"
        );
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
