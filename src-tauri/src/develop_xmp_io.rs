//! Read and write the develop-state profile reference in an Adobe XMP sidecar.
//!
//! The only develop setting this module touches is the `crs:Look` reference: the
//! `crs:Name` / `crs:UUID` / `crs:Amount` triple seen verbatim in real
//! Adobe-authored XMP, plus Once-Lab's own namespaced attributes on the same
//! nested `rdf:Description`. Everything else in the sidecar is preserved
//! byte-for-byte: the document is never re-serialized, only one parser-located
//! byte range is replaced or inserted.
//!
//! XML is parsed namespace-aware with `quick-xml`'s event `Reader`, so a
//! differently-prefixed element bound to the Camera Raw settings namespace still
//! round-trips. No XML writer is used and no dependency is added.

use std::ops::Range;
use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::PrefixDeclaration;
use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::develop_state::{
    DEFAULT_AMOUNT_PERCENT, DEVELOP_STATE_VERSION, DevelopProfileReference,
};
use crate::develop_state_io::{
    DevelopProfileRestore, develop_profile_roots, resolve_reference_against_roots,
};

/// The Once-Lab private namespace for its own develop state. A reserved,
/// never-resolvable TLD (RFC 2606) keeps it provably collision-free; XMP never
/// dereferences a namespace.
pub const ONCE_LAB_NAMESPACE: &str = "http://once-lab.invalid/ns/develop/1.0/";
pub const ONCE_LAB_PREFIX: &str = "onelab";
/// The Camera Raw settings namespace the `crs:` prefix is bound to in every
/// Adobe-authored file.
pub const CRS_NAMESPACE: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";
pub const CRS_PREFIX: &str = "crs";

/// The RDF namespace, resolved rather than matched by the literal `rdf:` prefix.
const RDF_NAMESPACE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// A minimal, valid skeleton for a sidecar that does not exist yet. The `crs`
/// and `onelab` declarations ride on the inserted `crs:Look` element itself, so
/// the main description needs no namespace attributes here.
const SIDECAR_SKELETON: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Once-Lab">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="">
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

/// Where a write landed and what it did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct XmpDevelopWriteOutcome {
    pub sidecar_path: String,
    /// True when the sidecar did not exist and was created.
    pub created: bool,
    /// True when a profile reference was written; false when the empty
    /// (`<crs:Look crs:Name=""/>`) form was written.
    pub wrote_profile: bool,
}

/// What an explicit read found in the sidecar.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct XmpDevelopReadOutcome {
    pub sidecar_path: String,
    /// The resolved profile, in the same shape the metadata restore uses, so the
    /// frontend applies it through the identical projection.
    pub restore: DevelopProfileRestore,
}

// --------------------------------------------------------------- amount (§R12)

/// Formats a profile Amount (in PERCENT) the way Adobe writes `crs:Amount`: a
/// bare integer when integral, otherwise six decimals.
pub(crate) fn format_crs_amount(percent: f32) -> String {
    let fraction = f64::from(percent) / 100.0;

    if fraction.fract() == 0.0 {
        // `as i64` collapses `-0.0` to `0`, matching Adobe's own output.
        format!("{}", fraction as i64)
    } else {
        format!("{fraction:.6}")
    }
}

/// Parses an Adobe `crs:Amount` fraction into a PERCENT, removing binary-float
/// representation noise so `0.65` reads back as exactly `65`, not
/// `65.00000000000001`.
///
/// The authored domain is a fraction in `0..=2` (0 to 200 percent). A
/// non-finite result, or a percent outside `0.0..=200.0`, is a contextual error
/// rather than a silent clamp: a value we cannot act on correctly must be
/// reported, never substituted.
pub(crate) fn parse_crs_amount(value: &str) -> Result<f32, String> {
    let fraction = value
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("invalid crs:Amount value \"{value}\""))?;

    if !fraction.is_finite() {
        return Err(format!("invalid crs:Amount value \"{value}\""));
    }

    let percent = ((fraction * 100.0) * 1000.0).round() / 1000.0;

    if !percent.is_finite() || !(0.0..=200.0).contains(&percent) {
        return Err(format!(
            "crs:Amount value \"{value}\" is outside the supported domain: expected a fraction in 0..=2 (0 to 200 percent)"
        ));
    }

    Ok(percent as f32)
}

// ------------------------------------------------------------------ render (§R1)

/// Renders the `<crs:Look>` element (including its leading indentation).
///
/// `None` produces the empty form seen verbatim in real Adobe-authored XMP;
/// `Some(reference)` writes only the three Adobe attributes seen verbatim in
/// real Adobe-authored XMP, plus Once-Lab's own namespaced attributes.
/// `crs:Stubbed`, `crs:Cluster`, the nested `crs:Group` and any table data are
/// deliberately never written.
pub(crate) fn render_crs_look_element(
    indent: &str,
    profile: Option<&DevelopProfileReference>,
) -> String {
    let Some(reference) = profile else {
        return format!(
            "{indent}<{CRS_PREFIX}:Look xmlns:{CRS_PREFIX}=\"{CRS_NAMESPACE}\" {CRS_PREFIX}:Name=\"\"/>\n"
        );
    };

    let name = reference.name.as_deref().unwrap_or_default();
    let mut description_line = format!(
        "{indent} <rdf:Description {CRS_PREFIX}:Name=\"{}\"",
        escape_xml_attribute(name)
    );

    if let Some(uuid) = reference.uuid.as_deref().filter(|value| !value.is_empty()) {
        description_line.push_str(&format!(
            " {CRS_PREFIX}:UUID=\"{}\"",
            escape_xml_attribute(uuid)
        ));
    }

    description_line.push_str(&format!(
        " {CRS_PREFIX}:Amount=\"{}\"",
        format_crs_amount(reference.amount_percent)
    ));

    let mut private_line = format!(
        "{indent}   xmlns:{ONCE_LAB_PREFIX}=\"{ONCE_LAB_NAMESPACE}\" {ONCE_LAB_PREFIX}:SchemaVersion=\"{DEVELOP_STATE_VERSION}\""
    );

    if let Some(path) = reference.path.as_deref().filter(|value| !value.is_empty()) {
        private_line.push_str(&format!(
            " {ONCE_LAB_PREFIX}:ProfilePath=\"{}\"",
            escape_xml_attribute(path)
        ));
    }

    private_line.push_str("/>");

    format!(
        "{indent}<{CRS_PREFIX}:Look xmlns:{CRS_PREFIX}=\"{CRS_NAMESPACE}\">\n\
{description_line}\n\
{private_line}\n\
{indent}</{CRS_PREFIX}:Look>\n"
    )
}

/// Escapes the four characters that are unsafe in a double-quoted XML attribute
/// value. The reader unescapes them with `quick_xml::escape::unescape`.
fn escape_xml_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(character),
        }
    }

    escaped
}

// ------------------------------------------------------------------ splice (§R5)

/// Byte range of the `crs:Look` element in `document`, if present.
///
/// The element is matched namespace-aware: any prefix bound to [`CRS_NAMESPACE`]
/// with the local name `Look` is accepted, so a differently-prefixed but correct
/// file still round-trips.
///
/// Only the `crs:Look` that is a DIRECT CHILD of the main `rdf:Description` is
/// accepted (the same description [`find_main_description_end`] targets: the
/// first direct child of `rdf:RDF`). A `crs:Look` nested anywhere else — in an
/// earlier sibling property, or inside another property of the main description
/// — is ignored, so a decoy cannot win over the real reference.
///
/// Offsets are absolute: into `document` exactly as passed, including any leading
/// UTF-8 BOM (see [`split_utf8_bom`]).
pub(crate) fn find_crs_look_range(document: &str) -> Result<Option<Range<usize>>, String> {
    let (bom, body) = split_utf8_bom(document);
    let base = bom.len();
    let main = main_description(body)?;

    let mut reader = Reader::from_str(body);
    let mut scope = NsScope::new();
    let mut depth: usize = 0;

    loop {
        let start = reader.buffer_position() as usize;

        let event = reader
            .read_event()
            .map_err(|error| format!("failed to parse XMP document: {error}"))?;

        match event {
            Event::Start(element) => {
                scope.push(collect_declarations(&element)?);
                depth += 1;

                if depth == main.depth + 1
                    && main.content.contains(&start)
                    && element_is(&scope, element.name().as_ref(), CRS_NAMESPACE, b"Look")
                {
                    let look_depth = depth;

                    loop {
                        let inner = reader
                            .read_event()
                            .map_err(|error| format!("failed to parse XMP document: {error}"))?;

                        match inner {
                            Event::Start(inner_element) => {
                                scope.push(collect_declarations(&inner_element)?);
                                depth += 1;
                            }
                            Event::Empty(inner_element) => {
                                scope.push(collect_declarations(&inner_element)?);
                                scope.pop();
                            }
                            Event::End(_) => {
                                scope.pop();

                                if depth == look_depth {
                                    let end = reader.buffer_position() as usize;
                                    return Ok(Some(start + base..end + base));
                                }

                                depth -= 1;
                            }
                            Event::Eof => {
                                return Err(
                                    "unterminated crs:Look element in XMP document".to_string()
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }
            Event::Empty(element) => {
                scope.push(collect_declarations(&element)?);

                if depth == main.depth
                    && main.content.contains(&start)
                    && element_is(&scope, element.name().as_ref(), CRS_NAMESPACE, b"Look")
                {
                    let end = reader.buffer_position() as usize;
                    return Ok(Some(start + base..end + base));
                }

                scope.pop();
            }
            Event::End(_) => {
                scope.pop();
                depth = depth.saturating_sub(1);
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}

/// Splits a leading UTF-8 BOM off `document`, returning `(bom, body)`.
///
/// `quick_xml::Reader::from_str` strips a leading UTF-8 BOM from the slice it is
/// handed, but it advances that slice without advancing its internal offset, so
/// every `buffer_position()` is 3 bytes short of the true offset into the string
/// we hold. Parsing the BOM-stripped body and adding `bom.len()` (0 or 3) back to
/// every offset keeps this module's contract — "offsets are into the document you
/// passed" — for a BOM-prefixed sidecar too (`std::fs::read_to_string` keeps the
/// BOM as `U+FEFF`).
fn split_utf8_bom(document: &str) -> (&str, &str) {
    match document.strip_prefix('\u{FEFF}') {
        Some(body) => ("\u{FEFF}", body),
        None => ("", document),
    }
}

/// The main `rdf:Description` — the first direct child of `rdf:RDF` — as the byte
/// range of its content plus its nesting depth.
struct MainDescription {
    /// Content bytes of the main description, in the BOM-less body's coordinates.
    content: Range<usize>,
    /// Nesting depth of the description element itself. Its direct children sit
    /// at `depth + 1`.
    depth: usize,
}

/// Locates the main `rdf:Description` in a BOM-less `document`.
///
/// A "last `</rdf:Description>`" (or "first") search is WRONG: a populated file
/// carries the Look's own nested stub description, and a sidecar may hold more
/// than one direct-child description.
fn main_description(document: &str) -> Result<MainDescription, String> {
    let mut reader = Reader::from_str(document);
    let mut scope = NsScope::new();
    let mut depth: usize = 0;
    let mut rdf_depth: Option<usize> = None;
    let mut main_description_depth: Option<usize> = None;
    let mut content_start: usize = 0;

    loop {
        let start = reader.buffer_position() as usize;

        let event = reader
            .read_event()
            .map_err(|error| format!("failed to parse XMP document: {error}"))?;

        match event {
            Event::Start(element) => {
                scope.push(collect_declarations(&element)?);
                depth += 1;

                let name = element.name();

                if element_is(&scope, name.as_ref(), RDF_NAMESPACE, b"RDF") {
                    if rdf_depth.is_none() {
                        rdf_depth = Some(depth);
                    }
                } else if let Some(rdf) = rdf_depth
                    && main_description_depth.is_none()
                    && depth == rdf + 1
                    && element_is(&scope, name.as_ref(), RDF_NAMESPACE, b"Description")
                {
                    main_description_depth = Some(depth);
                    content_start = reader.buffer_position() as usize;
                }
            }
            Event::Empty(element) => {
                scope.push(collect_declarations(&element)?);
                scope.pop();
            }
            Event::End(_) => {
                scope.pop();

                if main_description_depth == Some(depth) {
                    return Ok(MainDescription {
                        content: content_start..start,
                        depth,
                    });
                }

                depth = depth.saturating_sub(1);
            }
            Event::Eof => {
                return Err(
                    "XMP document has no rdf:RDF rdf:Description to splice into".to_string()
                );
            }
            _ => {}
        }
    }
}

/// Byte offset of the closing `</rdf:Description>` of the FIRST `rdf:Description`
/// that is a direct child of `rdf:RDF`.
///
/// The offset is absolute: into `document` exactly as passed, including any
/// leading UTF-8 BOM (see [`split_utf8_bom`]).
pub(crate) fn find_main_description_end(document: &str) -> Result<usize, String> {
    let (bom, body) = split_utf8_bom(document);
    let main = main_description(body)?;

    Ok(main.content.end + bom.len())
}

/// The indentation of the line that holds the main description's end tag, i.e.
/// the indentation a `crs:Look` inserted into `document` should use.
pub(crate) fn crs_look_indent(document: &str) -> Result<String, String> {
    let offset = find_main_description_end(document)?;

    Ok(line_indent(document, offset).to_string())
}

/// Replaces the existing `crs:Look` element, or inserts the rendered `element`
/// immediately before the main description's closing tag.
///
/// The document is never re-serialized: every byte outside the spliced range (or
/// outside the zero-width insertion point) is preserved verbatim. The spliced
/// range is the located element plus the leading horizontal whitespace the
/// replace branch swallows (see below); nothing else moves. The caller supplies
/// an `element` already rendered with the indentation from [`crs_look_indent`];
/// alignment is cosmetic.
///
/// The replace branch also swallows the run of spaces and tabs immediately before
/// the replaced element — normally its own leading indentation, but a same-line
/// separating space when the Look shares its line with preceding content. The
/// rendered `element` already carries its indentation, so keeping both would grow
/// the indent on every write; the swap is therefore NOT byte-identical for that
/// leading whitespace run, which is normalized away without dropping any
/// non-whitespace byte. A UTF-8 BOM needs no special handling here — it sits at
/// offset 0, before any splice range, so splicing the ORIGINAL document preserves
/// it automatically.
pub(crate) fn splice_crs_look(document: &str, element: &str) -> Result<String, String> {
    if let Some(range) = find_crs_look_range(document)? {
        let start = range.start - trailing_horizontal_whitespace(&document[..range.start]);

        let mut output = String::with_capacity(document.len());
        output.push_str(&document[..start]);
        output.push_str(element.trim_end());
        output.push_str(&document[range.end..]);

        return Ok(output);
    }

    let offset = find_main_description_end(document)?;

    // Insert at the START of the end tag's line, not at the tag itself: that keeps
    // the closing tag's own indentation intact, so the line's indentation — the
    // source [`crs_look_indent`] draws from — survives the insert. Inserting at
    // the tag would consume that indentation and make an insert followed by a
    // replace non-idempotent.
    let insert_at = offset - line_indent(document, offset).len();

    let mut output = String::with_capacity(document.len() + element.len());
    output.push_str(&document[..insert_at]);
    output.push_str(element);
    output.push_str(&document[insert_at..]);

    Ok(output)
}

/// The length of the run of spaces and tabs ending at the end of `prefix`, or 0
/// when the last byte is anything else (a line break stops the run).
fn trailing_horizontal_whitespace(prefix: &str) -> usize {
    prefix
        .bytes()
        .rev()
        .take_while(|byte| *byte == b' ' || *byte == b'\t')
        .count()
}

/// The leading whitespace of the line containing `offset`, or an empty string
/// when the offset is not preceded only by whitespace on its line.
fn line_indent(document: &str, offset: usize) -> &str {
    let line_start = document[..offset].rfind('\n').map_or(0, |index| index + 1);
    let line = &document[line_start..offset];

    if line
        .chars()
        .all(|character| character == ' ' || character == '\t')
    {
        line
    } else {
        ""
    }
}

// ------------------------------------------------------------------- read (§R13)

/// The Look reference found in a sidecar, before resolution.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawXmpLook {
    pub name: Option<String>,
    pub uuid: Option<String>,
    pub path: Option<String>,
    pub amount_percent: f32,
}

/// Attributes collected from one element, matching on the resolved namespace
/// rather than the literal prefix.
#[derive(Default)]
struct RawAttributes {
    name: Option<String>,
    uuid: Option<String>,
    amount: Option<String>,
    path: Option<String>,
    schema_version: Option<u32>,
}

/// The Look reference found in a sidecar.
///
/// Only the `crs:Look` element's own attributes and those of its first nested
/// `rdf:Description` are read; `crs:Parameters` is never entered, so an authored
/// `crs:RGBTableAmount` can never be mistaken for the user's Amount.
///
/// Only the `crs:Look` that is a DIRECT CHILD of the main `rdf:Description` (the
/// first direct child of `rdf:RDF`) is read, mirroring [`find_crs_look_range`], so
/// a `crs:Look` nested in some other property cannot win over the real reference.
pub(crate) fn read_crs_look(document: &str) -> Result<Option<RawXmpLook>, String> {
    let (_, body) = split_utf8_bom(document);
    let main = main_description(body)?;

    let mut reader = Reader::from_str(body);
    let mut scope = NsScope::new();
    let mut depth: usize = 0;
    let mut in_look = false;
    let mut look_depth: usize = 0;
    let mut look_attributes: Option<RawAttributes> = None;
    let mut description_attributes: Option<RawAttributes> = None;

    loop {
        let start = reader.buffer_position() as usize;

        let event = reader
            .read_event()
            .map_err(|error| format!("failed to parse XMP document: {error}"))?;

        match event {
            Event::Start(element) => {
                scope.push(collect_declarations(&element)?);
                depth += 1;

                if !in_look
                    && depth == main.depth + 1
                    && main.content.contains(&start)
                    && element_is(&scope, element.name().as_ref(), CRS_NAMESPACE, b"Look")
                {
                    in_look = true;
                    look_depth = depth;
                    look_attributes = Some(read_attributes(&scope, &element)?);
                } else if in_look
                    && description_attributes.is_none()
                    && depth == look_depth + 1
                    && element_is(
                        &scope,
                        element.name().as_ref(),
                        RDF_NAMESPACE,
                        b"Description",
                    )
                {
                    description_attributes = Some(read_attributes(&scope, &element)?);
                }
            }
            Event::Empty(element) => {
                let declarations = collect_declarations(&element)?;
                scope.push(declarations);

                if !in_look
                    && depth == main.depth
                    && main.content.contains(&start)
                    && element_is(&scope, element.name().as_ref(), CRS_NAMESPACE, b"Look")
                {
                    // A self-closing Look is the whole element.
                    let look = read_attributes(&scope, &element)?;
                    scope.pop();

                    return finalize_look(Some(look), description_attributes.take());
                }

                if in_look
                    && description_attributes.is_none()
                    && depth == look_depth
                    && element_is(
                        &scope,
                        element.name().as_ref(),
                        RDF_NAMESPACE,
                        b"Description",
                    )
                {
                    description_attributes = Some(read_attributes(&scope, &element)?);
                }

                scope.pop();
            }
            Event::End(_) => {
                scope.pop();

                if in_look && depth == look_depth {
                    return finalize_look(look_attributes.take(), description_attributes.take());
                }

                depth = depth.saturating_sub(1);
            }
            Event::Eof => {
                if in_look {
                    return Err("unterminated crs:Look element in XMP document".to_string());
                }

                return Ok(None);
            }
            _ => {}
        }
    }
}

/// Combines the attributes of the Look element and its nested description into
/// the final reference, applying Adobe's defaults and the private-schema rule.
fn finalize_look(
    look_attributes: Option<RawAttributes>,
    description_attributes: Option<RawAttributes>,
) -> Result<Option<RawXmpLook>, String> {
    let look = look_attributes.unwrap_or_default();
    let description = description_attributes.unwrap_or_default();

    let name = description
        .name
        .or(look.name)
        .filter(|value| !value.is_empty());
    let uuid = description
        .uuid
        .or(look.uuid)
        .filter(|value| !value.is_empty());
    let amount = description.amount.or(look.amount);

    if name.is_none() && uuid.is_none() {
        // Adobe's empty form, `<crs:Look crs:Name=""/>`.
        return Ok(None);
    }

    let amount_percent = match amount {
        Some(value) => parse_crs_amount(&value)?,
        None => DEFAULT_AMOUNT_PERCENT,
    };

    // A record written by a newer schema is still applied for its
    // Adobe-compatible part; the private fields we do not understand are
    // discarded.
    let path = match description.schema_version {
        Some(version) if version > DEVELOP_STATE_VERSION => None,
        _ => description.path.filter(|value| !value.is_empty()),
    };

    Ok(Some(RawXmpLook {
        name,
        uuid,
        path,
        amount_percent,
    }))
}

/// Reads the CRS/Once-Lab attributes of one element, resolving prefixes against
/// the current namespace scope.
fn read_attributes(scope: &NsScope, element: &BytesStart<'_>) -> Result<RawAttributes, String> {
    let mut attributes = RawAttributes::default();

    for attribute in element.attributes() {
        let attribute =
            attribute.map_err(|error| format!("failed to parse XMP attribute: {error}"))?;

        if attribute.key.as_namespace_binding().is_some() {
            continue;
        }

        // An unprefixed attribute is in NO namespace, so it is never ours.
        let (Some(prefix), local) = split_name(attribute.key.as_ref()) else {
            continue;
        };

        let Ok(prefix) = std::str::from_utf8(prefix) else {
            continue;
        };

        let Some(namespace) = scope.resolve(prefix) else {
            continue;
        };

        let value = attribute_value(&attribute)?;

        if namespace == CRS_NAMESPACE {
            match local {
                b"Name" => attributes.name = Some(value),
                b"UUID" => attributes.uuid = Some(value),
                b"Amount" => attributes.amount = Some(value),
                _ => {}
            }
        } else if namespace == ONCE_LAB_NAMESPACE {
            match local {
                b"ProfilePath" => attributes.path = Some(value),
                b"SchemaVersion" => attributes.schema_version = value.trim().parse::<u32>().ok(),
                _ => {}
            }
        }
    }

    Ok(attributes)
}

/// The XML-unescaped value of an attribute.
fn attribute_value(
    attribute: &quick_xml::events::attributes::Attribute<'_>,
) -> Result<String, String> {
    let raw = std::str::from_utf8(&attribute.value)
        .map_err(|error| format!("failed to decode XMP attribute value: {error}"))?;
    let unescaped = quick_xml::escape::unescape(raw)
        .map_err(|error| format!("failed to unescape XMP attribute value: {error}"))?;

    Ok(unescaped.into_owned())
}

// -------------------------------------------------------------- namespace scope

/// A stack of namespace declarations, pushed on `Start` and popped on `End`.
struct NsScope {
    frames: Vec<Vec<(String, String)>>,
}

impl NsScope {
    fn new() -> Self {
        NsScope {
            frames: vec![Vec::new()],
        }
    }

    fn push(&mut self, declarations: Vec<(String, String)>) {
        self.frames.push(declarations);
    }

    fn pop(&mut self) {
        if self.frames.len() > 1 {
            self.frames.pop();
        }
    }

    /// Resolves a prefix against the innermost binding in scope. The empty
    /// prefix is the default namespace.
    fn resolve(&self, prefix: &str) -> Option<&str> {
        self.frames
            .iter()
            .rev()
            .flat_map(|frame| frame.iter().rev())
            .find(|(declared, _)| declared == prefix)
            .map(|(_, uri)| uri.as_str())
    }
}

/// Every `xmlns` / `xmlns:prefix` declaration on an element, as
/// `(prefix, uri)` pairs (the default namespace uses an empty prefix).
fn collect_declarations(element: &BytesStart<'_>) -> Result<Vec<(String, String)>, String> {
    let mut declarations = Vec::new();

    for attribute in element.attributes() {
        let attribute =
            attribute.map_err(|error| format!("failed to parse XMP attribute: {error}"))?;

        let prefix = match attribute.key.as_namespace_binding() {
            Some(PrefixDeclaration::Default) => String::new(),
            Some(PrefixDeclaration::Named(prefix)) => String::from_utf8_lossy(prefix).into_owned(),
            None => continue,
        };

        declarations.push((prefix, attribute_value(&attribute)?));
    }

    Ok(declarations)
}

/// Splits a raw `prefix:local` name, returning `(Some(prefix), local)` or
/// `(None, local)`.
fn split_name(name: &[u8]) -> (Option<&[u8]>, &[u8]) {
    match name.iter().position(|byte| *byte == b':') {
        Some(index) => (Some(&name[..index]), &name[index + 1..]),
        None => (None, name),
    }
}

/// Whether `name` (a raw qualified name) resolves to `namespace` + `local`.
fn element_is(scope: &NsScope, name: &[u8], namespace: &str, local: &[u8]) -> bool {
    let (prefix, local_name) = split_name(name);

    if local_name != local {
        return false;
    }

    let resolved = match prefix {
        Some(prefix) => std::str::from_utf8(prefix)
            .ok()
            .and_then(|p| scope.resolve(p)),
        None => scope.resolve(""),
    };

    resolved == Some(namespace)
}

// ------------------------------------------------------------------- commands

/// Writes the image's current internal develop state into its XMP sidecar.
///
/// Refuses a virtual-copy path: a `?vc=` copy shares its physical image's
/// sidecar, so writing from it would silently overwrite the source's own state.
#[tauri::command]
pub fn write_develop_state_to_xmp(path: String) -> Result<XmpDevelopWriteOutcome, String> {
    if path.contains("?vc=") {
        return Err(format!(
            "refusing to write XMP from the virtual copy '{path}': a virtual copy shares its source image's sidecar"
        ));
    }

    let (image_path, _) = crate::file_management::parse_virtual_path(&path);

    write_develop_state_to_xmp_at(&image_path)
}

/// Reads the develop-state profile reference back out of the image's XMP
/// sidecar and resolves it against the current profile library.
#[tauri::command]
pub fn read_develop_state_from_xmp(
    path: String,
    app_handle: AppHandle,
) -> Result<XmpDevelopReadOutcome, String> {
    let (image_path, _) = crate::file_management::parse_virtual_path(&path);
    let settings = crate::app_settings::load_settings(app_handle.clone())?;

    let home = app_handle.path().home_dir().ok();
    let roots = develop_profile_roots(home.as_deref(), &settings.xmp_profile_roots);

    read_develop_state_from_xmp_at(&image_path, &roots)
}

/// The testable write body, taking an explicit (already de-virtualized) image
/// path.
///
/// A `?vc=` path is refused here too, not only in the command, so the safety
/// property holds for any caller (including 6C5): a virtual copy shares its
/// physical image's sidecar and writing from it would overwrite the source's
/// state.
pub(crate) fn write_develop_state_to_xmp_at(
    image_path: &Path,
) -> Result<XmpDevelopWriteOutcome, String> {
    let image_path_string = image_path.to_string_lossy().to_string();

    if image_path_string.contains("?vc=") {
        return Err(format!(
            "refusing to write XMP from the virtual copy '{image_path_string}': a virtual copy shares its source image's sidecar"
        ));
    }

    let (_, sidecar_path) = crate::file_management::parse_virtual_path(&image_path_string);

    // The internal `.rrdata` state always wins: the write takes whatever the
    // app currently holds, but never reinterprets a newer schema.
    let metadata = crate::exif_processing::load_sidecar(&sidecar_path);
    let state = metadata.develop_state();

    if !state.is_supported() {
        return Err(format!(
            "refusing to write XMP for '{}': the develop state was written by a newer Once-Lab version ({})",
            sidecar_path.display(),
            state.version
        ));
    }

    let profile = state.profile;
    let xmp_path = xmp_write_target(image_path);

    // Never write to a creative profile file: the target must classify as an
    // ordinary (or unsupported) document, and an unreadable target cannot be
    // proven safe.
    if xmp_path.exists() {
        match crate::xmp_profile::discovery::classify_xmp_profile_path(&xmp_path) {
            Ok(None) => {}
            Ok(Some(_)) => {
                return Err(format!(
                    "refusing to overwrite '{}': it is a creative XMP profile, not an image sidecar",
                    xmp_path.display()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "refusing to write '{}': the existing file could not be read to prove it is safe ({error})",
                    xmp_path.display()
                ));
            }
        }
    }

    let created = !xmp_path.exists();
    let document = if created {
        SIDECAR_SKELETON.to_string()
    } else {
        std::fs::read_to_string(&xmp_path).map_err(|error| {
            format!(
                "failed to read XMP sidecar '{}': {error}",
                xmp_path.display()
            )
        })?
    };

    let indent = crs_look_indent(&document)?;
    let element = render_crs_look_element(&indent, profile.as_ref());
    let updated = splice_crs_look(&document, &element)?;

    std::fs::write(&xmp_path, updated).map_err(|error| {
        format!(
            "failed to write XMP sidecar '{}': {error}",
            xmp_path.display()
        )
    })?;

    log::info!(
        "Wrote develop state to XMP sidecar '{}' (created: {created})",
        xmp_path.display()
    );

    Ok(XmpDevelopWriteOutcome {
        sidecar_path: xmp_path.to_string_lossy().to_string(),
        created,
        wrote_profile: profile.is_some(),
    })
}

/// The testable read body, taking an explicit (already de-virtualized) image
/// path and the profile roots to resolve against.
pub(crate) fn read_develop_state_from_xmp_at(
    image_path: &Path,
    roots: &[PathBuf],
) -> Result<XmpDevelopReadOutcome, String> {
    let xmp_path = resolve_xmp_target(image_path)
        .ok_or_else(|| format!("no XMP sidecar found next to '{}'", image_path.display()))?;

    let document = std::fs::read_to_string(&xmp_path).map_err(|error| {
        format!(
            "failed to read XMP sidecar '{}': {error}",
            xmp_path.display()
        )
    })?;

    let restore = match read_crs_look(&document)? {
        None => DevelopProfileRestore::None,
        Some(look) => {
            let reference = DevelopProfileReference {
                uuid: look.uuid,
                path: look.path,
                name: look.name,
                amount_percent: look.amount_percent,
            };

            resolve_reference_against_roots(&reference, roots)
        }
    };

    Ok(XmpDevelopReadOutcome {
        sidecar_path: xmp_path.to_string_lossy().to_string(),
        restore,
    })
}

/// The existing `.xmp` next to `image_path` (lowercase preferred), if any.
fn resolve_xmp_target(image_path: &Path) -> Option<PathBuf> {
    let lowercase = image_path.with_extension("xmp");

    if lowercase.exists() {
        return Some(lowercase);
    }

    let uppercase = image_path.with_extension("XMP");

    if uppercase.exists() {
        return Some(uppercase);
    }

    None
}

/// The `.xmp` a write targets: the existing file (lowercase preferred) or the
/// lowercase name when none exists yet.
fn xmp_write_target(image_path: &Path) -> PathBuf {
    resolve_xmp_target(image_path).unwrap_or_else(|| image_path.with_extension("xmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    /// Synthetic 2x2x2 identity RGBTable (same fixture pattern as the parser,
    /// rgb_table, loader, summary and discovery tests).
    const IDENTITY_2X2X2_BASE85: &str = "71000rtKmwBRLy1{X$/w9'=(bLC9YEvFE(9%a0@fg0";

    fn reference(
        uuid: Option<&str>,
        path: Option<&str>,
        name: Option<&str>,
        amount_percent: f32,
    ) -> DevelopProfileReference {
        DevelopProfileReference {
            uuid: uuid.map(str::to_string),
            path: path.map(str::to_string),
            name: name.map(str::to_string),
            amount_percent,
        }
    }

    /// The module's own minimal skeleton (name namespaces `x`/`rdf` only).
    fn skeleton() -> String {
        SIDECAR_SKELETON.to_string()
    }

    /// A Once-Lab-style skeleton that also declares `xmp`/`dc` but NOT `crs`, so
    /// the inserted Look must declare `crs` itself (6C3-R4).
    fn once_lab_style_skeleton() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RapidRAW">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#
        .to_string()
    }

    /// A populated sidecar with a Look (nested-description form), a
    /// PROJECT-SYNTHETIC `crs:Parameters` block (zero occurrences in real Adobe
    /// output — the test pins only the "we never touch it" property, not Adobe
    /// semantics), and `dc:subject`.
    fn populated_sidecar() -> String {
        r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 5.6-c140">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    crs:Exposure2012="1.25">
   <crs:Look>
    <rdf:Description
     crs:Name="Old Look"
     crs:Amount="0.500000"
     crs:UUID="UUID-OLD-LOOK"/>
   </crs:Look>
   <crs:Parameters>
    <rdf:Description
     crs:RGBTableAmount="0.5"
     crs:Table_PROJECTDATA="PROJECT-TABLE-BLOB"/>
   </crs:Parameters>
   <dc:subject>
    <rdf:Bag>
     <rdf:li>keep</rdf:li>
    </rdf:Bag>
   </dc:subject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#
        .to_string()
    }

    /// A valid, discoverable supported profile document.
    fn valid_profile_xmp(name: &str, uuid: &str) -> String {
        format!(
            r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:PresetType="Look"
      crs:UUID="{uuid}"
      crs:SupportsAmount="True"
      crs:ConvertToGrayscale="False"
      crs:RGBTable="TESTTABLE"
      crs:Table_TESTTABLE="{table}">
      <crs:Name>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">{name}</rdf:li>
        </rdf:Alt>
      </crs:Name>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#,
            table = IDENTITY_2X2X2_BASE85
        )
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);

        let dir = std::env::temp_dir().join(format!(
            "once_lab_develop_xmp_io_{}_{}_{}_{}",
            prefix,
            std::process::id(),
            nanos,
            counter
        ));

        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_image(path: &Path) {
        fs::write(path, b"not-a-real-image").expect("write image placeholder");
    }

    fn sidecar_with_profile(uuid: &str, path: &str, name: &str, amount_percent: f32) -> String {
        serde_json::to_string_pretty(&json!({
            "version": 1,
            "rating": 0,
            "adjustments": null,
            "develop_state": {
                "version": 1,
                "profile": {
                    "uuid": uuid,
                    "path": path,
                    "name": name,
                    "amount_percent": amount_percent,
                },
            },
        }))
        .expect("serialize sidecar")
    }

    /// The leading indentation of the line carrying the `crs:Look` element.
    fn look_line_indent(document: &str) -> String {
        let index = document.find("<crs:Look").expect("a Look");
        let line_start = document[..index]
            .rfind('\n')
            .map_or(0, |position| position + 1);

        document[line_start..index].to_string()
    }

    // -------------------------------------------------------- amount (§1.3)

    #[test]
    fn format_crs_amount_matches_adobe_output() {
        for (percent, expected) in [
            (0.0_f32, "0"),
            (50.0, "0.500000"),
            (65.0, "0.650000"),
            (100.0, "1"),
            (140.0, "1.400000"),
            (200.0, "2"),
        ] {
            assert_eq!(
                format_crs_amount(percent),
                expected,
                "{percent}% must format as {expected}"
            );
        }

        assert_eq!(
            format_crs_amount(-0.0),
            "0",
            "negative zero must render as 0"
        );
    }

    #[test]
    fn parse_crs_amount_round_trips_and_removes_float_noise() {
        assert_eq!(parse_crs_amount("1").unwrap(), 100.0);
        assert_eq!(parse_crs_amount("0.500000").unwrap(), 50.0);
        assert_eq!(
            parse_crs_amount("0.650000").unwrap(),
            65.0,
            "0.65 must read back as exactly 65.0, not 65.00000000000001"
        );
        assert_eq!(parse_crs_amount("1.400000").unwrap(), 140.0);
        assert_eq!(parse_crs_amount("0").unwrap(), 0.0);
        assert_eq!(parse_crs_amount("2").unwrap(), 200.0);
    }

    #[test]
    fn parse_crs_amount_rejects_malformed_values() {
        for value in ["abc", "NaN", "inf", "", "   "] {
            assert!(
                parse_crs_amount(value).is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    #[test]
    fn parse_crs_amount_rejects_a_value_outside_the_supported_domain() {
        for value in ["5", "-1"] {
            let error = parse_crs_amount(value)
                .expect_err("an out-of-domain amount must be a contextual error, never clamped");
            assert!(
                error.contains(value),
                "the error must name the offending value: {error}"
            );
            assert!(
                error.contains("0..=2"),
                "the error must name the accepted fraction range: {error}"
            );
            assert!(
                error.contains("200"),
                "the error must name the accepted percent bound: {error}"
            );
        }
    }

    #[test]
    fn parse_crs_amount_rejects_an_overflowing_value() {
        assert!(
            parse_crs_amount("1e39").is_err(),
            "1e39 must not be silently accepted (its percent overflows the domain)"
        );
    }

    #[test]
    fn parse_crs_amount_accepts_the_domain_boundaries() {
        assert_eq!(parse_crs_amount("0").unwrap(), 0.0);
        assert_eq!(parse_crs_amount("2").unwrap(), 200.0);
    }

    // -------------------------------------------------------- render (§1.4)

    #[test]
    fn render_empty_look_is_adobe_empty_form() {
        let rendered = render_crs_look_element("  ", None);

        assert_eq!(
            rendered,
            format!("  <crs:Look xmlns:crs=\"{CRS_NAMESPACE}\" crs:Name=\"\"/>\n")
        );
        assert!(rendered.contains(CRS_NAMESPACE));
        assert!(rendered.contains("crs:Name=\"\""));
    }

    #[test]
    fn render_profile_look_carries_the_proven_attributes() {
        let reference = reference(
            Some("UUID-ABC"),
            Some("/profiles/alpha.xmp"),
            Some("Alpha"),
            65.0,
        );

        let rendered = render_crs_look_element("\t", Some(&reference));

        assert!(rendered.contains("crs:Name=\"Alpha\""), "{rendered}");
        assert!(rendered.contains("crs:UUID=\"UUID-ABC\""), "{rendered}");
        assert!(rendered.contains("crs:Amount=\"0.650000\""), "{rendered}");
        assert!(
            rendered.contains(&format!("xmlns:onelab=\"{ONCE_LAB_NAMESPACE}\"")),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("onelab:SchemaVersion=\"{DEVELOP_STATE_VERSION}\"")),
            "{rendered}"
        );
        assert!(
            rendered.contains("onelab:ProfilePath=\"/profiles/alpha.xmp\""),
            "{rendered}"
        );
    }

    #[test]
    fn render_profile_look_escapes_xml_metacharacters() {
        let name = "A & B <C> \"D\"";
        let path = "/profiles/&<>\".xmp";
        let reference = reference(Some("UUID-ESC"), Some(path), Some(name), 100.0);

        let rendered = render_crs_look_element("", Some(&reference));

        // The escaped form must not contain a raw metacharacter inside a value.
        assert!(
            rendered.contains("crs:Name=\"A &amp; B &lt;C&gt; &quot;D&quot;\""),
            "{rendered}"
        );

        let document = splice_crs_look(&skeleton(), &rendered).expect("splice");
        let look = read_crs_look(&document)
            .expect("read")
            .expect("the Look must be present");

        assert_eq!(look.name.as_deref(), Some(name), "the name round-trips");
        assert_eq!(look.path.as_deref(), Some(path), "the path round-trips");
        assert_eq!(look.uuid.as_deref(), Some("UUID-ESC"));
    }

    #[test]
    fn render_profile_look_omits_an_empty_path() {
        for path in [None, Some("")] {
            let reference = reference(Some("UUID"), path, Some("Name"), 100.0);
            let rendered = render_crs_look_element("", Some(&reference));

            assert!(
                !rendered.contains("onelab:ProfilePath"),
                "an absent/empty path must not be written: {rendered}"
            );
        }
    }

    #[test]
    fn render_profile_look_never_writes_stubbed_or_parameters() {
        let reference = reference(Some("UUID"), Some("/p.xmp"), Some("Name"), 50.0);
        let rendered = render_crs_look_element("", Some(&reference));

        assert!(
            !rendered.contains("Stubbed"),
            "6C3-R1: no Stubbed: {rendered}"
        );
        assert!(
            !rendered.contains("Parameters"),
            "6C3-R1: no Parameters: {rendered}"
        );
        assert!(
            !rendered.contains("Cluster"),
            "6C3-R1: no Cluster: {rendered}"
        );
    }

    // -------------------------------------------------------- splice (§1.5)

    #[test]
    fn splice_replaces_an_existing_look_and_preserves_every_other_byte() {
        let before = populated_sidecar();
        let range = find_crs_look_range(&before)
            .expect("parse")
            .expect("the fixture has a Look");

        let reference = reference(
            Some("UUID-NEW"),
            Some("/profiles/new.xmp"),
            Some("New"),
            100.0,
        );
        let element = render_crs_look_element("   ", Some(&reference));

        let after = splice_crs_look(&before, &element).expect("splice");

        // The replace also consumes the element's EXISTING leading indentation
        // (the rendered element carries its own), so the untouched prefix ends
        // where the whitespace before the element begins.
        let replaced_start = range.start - trailing_horizontal_whitespace(&before[..range.start]);

        // Byte equality of the two untouched regions, not merely "contains".
        assert!(
            after.starts_with(&before[..replaced_start]),
            "the prefix must be byte-identical"
        );
        assert!(
            after.ends_with(&before[range.end..]),
            "the suffix must be byte-identical"
        );

        assert_eq!(
            after,
            format!(
                "{}{}{}",
                &before[..replaced_start],
                element.trim_end(),
                &before[range.end..]
            ),
            "every byte outside the replaced range must be preserved"
        );

        // The untouched regions survive verbatim.
        assert!(after.contains("crs:Exposure2012=\"1.25\""));
        assert!(after.contains("crs:RGBTableAmount=\"0.5\""));
        assert!(after.contains("crs:Table_PROJECTDATA=\"PROJECT-TABLE-BLOB\""));
        assert!(after.contains("<dc:subject>"));
        assert!(after.contains("UUID-NEW"));

        // 6C3-R6: the old Look is replaced wholesale.
        assert!(!after.contains("UUID-OLD-LOOK"), "{after}");
        assert_eq!(after.matches("<crs:Look").count(), 1, "{after}");
        assert_eq!(after.matches("</crs:Look>").count(), 1, "{after}");
    }

    #[test]
    fn splice_inserts_a_look_when_none_exists() {
        let before = once_lab_style_skeleton();
        let reference = reference(
            Some("UUID-IN"),
            Some("/profiles/in.xmp"),
            Some("Inserted"),
            100.0,
        );

        let indent = crs_look_indent(&before).expect("indent");
        let element = render_crs_look_element(&indent, Some(&reference));

        let after = splice_crs_look(&before, &element).expect("splice");

        // The result parses and the Look is inside the main description.
        assert!(find_crs_look_range(&after).expect("parse").is_some());
        assert!(read_crs_look(&after).expect("read").is_some());

        let description_start = after.find("<rdf:Description").expect("description");
        let look_start = after.find("<crs:Look").expect("look");
        let description_end = after.find("</rdf:Description>").expect("description end");

        assert!(
            description_start < look_start && look_start < description_end,
            "the inserted Look must live inside the main description: {after}"
        );
    }

    #[test]
    fn splice_preserves_the_xpacket_processing_instruction_and_xmptk() {
        let before = populated_sidecar();
        let range = find_crs_look_range(&before).expect("parse").expect("look");

        let element = render_crs_look_element(
            "   ",
            Some(&reference(Some("UUID"), None, Some("N"), 100.0)),
        );
        let after = splice_crs_look(&before, &element).expect("splice");

        assert!(after.starts_with("<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>"));
        assert!(after.contains("x:xmptk=\"Adobe XMP Core 5.6-c140\""));

        // Byte equality of the untouched regions.
        assert_eq!(&after[..range.start], &before[..range.start]);
        assert_eq!(
            &after[after.len() - (before.len() - range.end)..],
            &before[range.end..]
        );
    }

    #[test]
    fn splice_insertion_preserves_every_other_byte() {
        let before = once_lab_style_skeleton();
        let element = render_crs_look_element(
            "  ",
            Some(&reference(
                Some("UUID-IN"),
                Some("/profiles/in.xmp"),
                Some("Inserted"),
                100.0,
            )),
        );

        let offset = find_main_description_end(&before).expect("main description end");
        let insert_at = offset - line_indent(&before, offset).len();
        let after = splice_crs_look(&before, &element).expect("splice");

        // Byte-equality of the untouched prefix and suffix around the insertion
        // point, not merely structure.
        assert_eq!(
            &after[..insert_at],
            &before[..insert_at],
            "the prefix must be byte-identical"
        );
        assert_eq!(
            &after[insert_at + element.len()..],
            &before[insert_at..],
            "the suffix must be byte-identical"
        );
        assert_eq!(
            after,
            format!(
                "{}{}{}",
                &before[..insert_at],
                element,
                &before[insert_at..]
            )
        );
    }

    // ---------------------------------------------------------- bom (F1)

    /// The exact `<crs:Look>` ELEMENT `populated_sidecar` carries (as returned by
    /// [`find_crs_look_range`] — starting at the `<`, so NOT including the
    /// element's own three-space indentation).
    const POPULATED_OLD_LOOK: &str = "<crs:Look>\n    <rdf:Description\n     crs:Name=\"Old Look\"\n     crs:Amount=\"0.500000\"\n     crs:UUID=\"UUID-OLD-LOOK\"/>\n   </crs:Look>";

    /// The same element together with its leading indentation, i.e. the exact
    /// bytes a replace of it consumes.
    fn populated_old_look_block() -> String {
        format!("   {POPULATED_OLD_LOOK}")
    }

    #[test]
    fn splice_preserves_a_utf8_bom_when_replacing() {
        let reference = reference(
            Some("UUID-NEW"),
            Some("/profiles/new.xmp"),
            Some("New"),
            100.0,
        );
        let element = render_crs_look_element("   ", Some(&reference));

        let body = populated_sidecar();
        let document = format!("\u{FEFF}{body}");

        let range = find_crs_look_range(&document)
            .expect("parse")
            .expect("the fixture has a Look");

        // The reported range is ABSOLUTE (into the BOM-prefixed document), so it
        // slices the real element out. Mutation: dropping the `+ base` in
        // `find_crs_look_range` makes this slice miss by 3 bytes.
        assert_eq!(&document[range.clone()], POPULATED_OLD_LOOK);

        let after = splice_crs_look(&document, &element).expect("splice");

        // Assert the EXACT expected document (BOM + body with the old Look block
        // swapped for the rendered element), so any orphaned/echoed byte fails.
        let expected = format!(
            "\u{FEFF}{}",
            body.replacen(&populated_old_look_block(), element.trim_end(), 1)
        );
        assert_eq!(after, expected);

        assert!(after.starts_with('\u{FEFF}'), "the BOM must survive");
        assert!(!after.contains("UUID-OLD-LOOK"), "{after}");
        assert_eq!(after.matches("<crs:Look").count(), 1, "{after}");
        assert_eq!(after.matches("</crs:Look>").count(), 1, "{after}");
    }

    #[test]
    fn splice_preserves_a_utf8_bom_when_inserting() {
        let reference = reference(
            Some("UUID-IN"),
            Some("/profiles/in.xmp"),
            Some("Inserted"),
            100.0,
        );

        let body = once_lab_style_skeleton();
        let document = format!("\u{FEFF}{body}");

        let indent = crs_look_indent(&document).expect("indent");
        assert_eq!(
            indent, "  ",
            "the insert uses the end tag line's indentation"
        );

        let element = render_crs_look_element(&indent, Some(&reference));
        let after = splice_crs_look(&document, &element).expect("splice");

        let expected = format!(
            "\u{FEFF}{}",
            body.replacen(
                "  </rdf:Description>",
                &format!("{element}  </rdf:Description>"),
                1
            )
        );
        assert_eq!(after, expected);

        assert!(after.starts_with('\u{FEFF}'), "the BOM must survive");
        assert!(
            find_crs_look_range(&after).expect("parse").is_some(),
            "the result must be well-formed"
        );
        assert_eq!(after.matches("<crs:Look").count(), 1, "{after}");
        assert!(read_crs_look(&after).expect("read").is_some());
    }

    #[test]
    fn splice_without_a_bom_is_unchanged_by_the_bom_fix() {
        // Replace: the BOM-less output equals the pre-fix output exactly (the old
        // block swapped for the rendered element), and prefixing the input with a
        // BOM yields exactly that output with a BOM in front — proving the fix is
        // a no-op for BOM-less documents.
        let replace_element = render_crs_look_element(
            "   ",
            Some(&reference(
                Some("UUID-NEW"),
                Some("/profiles/new.xmp"),
                Some("New"),
                100.0,
            )),
        );
        let body = populated_sidecar();

        let replaced = splice_crs_look(&body, &replace_element).expect("splice");
        assert_eq!(
            replaced,
            body.replacen(&populated_old_look_block(), replace_element.trim_end(), 1)
        );

        let replaced_with_bom =
            splice_crs_look(&format!("\u{FEFF}{body}"), &replace_element).expect("splice");
        assert_eq!(replaced_with_bom, format!("\u{FEFF}{replaced}"));

        // Insert: the same property for the insert branch.
        let insert_element = render_crs_look_element(
            "  ",
            Some(&reference(
                Some("UUID-IN"),
                Some("/profiles/in.xmp"),
                Some("Inserted"),
                100.0,
            )),
        );
        let skeleton = once_lab_style_skeleton();

        let inserted = splice_crs_look(&skeleton, &insert_element).expect("splice");
        assert_eq!(
            inserted,
            skeleton.replacen(
                "  </rdf:Description>",
                &format!("{insert_element}  </rdf:Description>"),
                1
            )
        );

        let inserted_with_bom =
            splice_crs_look(&format!("\u{FEFF}{skeleton}"), &insert_element).expect("splice");
        assert_eq!(inserted_with_bom, format!("\u{FEFF}{inserted}"));
    }

    // -------------------------------------------------------- decoy (F3)

    #[test]
    fn a_decoy_look_outside_the_main_description_is_ignored() {
        // A property that is a direct child of `rdf:RDF`, BEFORE the main
        // description, carries its own `crs:Look` (DECOY). In document order that
        // decoy comes first, so a naive "first crs:Look" wins it.
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <crs:Parameters xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <rdf:Description crs:Name="DECOY" crs:UUID="UUID-DECOY" crs:Amount="1">
    <crs:Look crs:Name="DECOY"/>
   </rdf:Description>
  </crs:Parameters>
  <rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Look>
    <rdf:Description crs:Name="Real" crs:UUID="UUID-REAL" crs:Amount="1"/>
   </crs:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

        let range = find_crs_look_range(document)
            .expect("parse")
            .expect("the real Look must be found");
        let slice = &document[range.clone()];
        assert!(slice.contains("UUID-REAL"), "{slice}");
        assert!(!slice.contains("DECOY"), "{slice}");

        let look = read_crs_look(document)
            .expect("parse")
            .expect("the real Look must be read");
        assert_eq!(look.name.as_deref(), Some("Real"));
        assert_eq!(look.uuid.as_deref(), Some("UUID-REAL"));

        let element = render_crs_look_element(
            "   ",
            Some(&reference(Some("UUID-NEW"), None, Some("New"), 100.0)),
        );
        let after = splice_crs_look(document, &element).expect("splice");

        assert!(after.contains("UUID-NEW"), "{after}");
        assert!(!after.contains("UUID-REAL"), "{after}");
        // The decoy's bytes are untouched.
        assert!(after.contains("<crs:Look crs:Name=\"DECOY\"/>"), "{after}");
        assert!(after.contains("UUID-DECOY"), "{after}");
    }

    #[test]
    fn a_decoy_look_inside_the_main_description_is_ignored() {
        // Another property of the MAIN description carries a nested `crs:Look`
        // (DECOY) and appears before the real one. The look's range IS inside the
        // main description, so only the direct-child rule rejects it.
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Parameters>
    <rdf:Description crs:Name="DECOY" crs:UUID="UUID-DECOY" crs:Amount="1">
     <crs:Look crs:Name="DECOY"/>
    </rdf:Description>
   </crs:Parameters>
   <crs:Look>
    <rdf:Description crs:Name="Real" crs:UUID="UUID-REAL" crs:Amount="1"/>
   </crs:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

        let range = find_crs_look_range(document)
            .expect("parse")
            .expect("the real Look must be found");
        let slice = &document[range.clone()];
        assert!(slice.contains("UUID-REAL"), "{slice}");
        assert!(!slice.contains("DECOY"), "{slice}");

        let look = read_crs_look(document)
            .expect("parse")
            .expect("the real Look must be read");
        assert_eq!(look.name.as_deref(), Some("Real"));
        assert_eq!(look.uuid.as_deref(), Some("UUID-REAL"));

        let element = render_crs_look_element(
            "   ",
            Some(&reference(Some("UUID-NEW"), None, Some("New"), 100.0)),
        );
        let after = splice_crs_look(document, &element).expect("splice");

        assert!(after.contains("UUID-NEW"), "{after}");
        assert!(!after.contains("UUID-REAL"), "{after}");
        assert!(after.contains("<crs:Look crs:Name=\"DECOY\"/>"), "{after}");
        assert!(after.contains("UUID-DECOY"), "{after}");
    }

    // ------------------------------------------------ sibling description (6C3)

    /// A two-description sidecar: the FIRST direct-child `rdf:Description` (the
    /// main one) has NO Look; the SECOND (a sibling, also a direct child of
    /// `rdf:RDF`) carries a CRS-namespace Look at the SAME nesting depth. The
    /// sibling binds the Camera Raw settings namespace to a non-`crs` prefix, so
    /// the test proves the rejection is by main-description RANGE and not merely
    /// by its prefix.
    fn sibling_description_document() -> &'static str {
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
  </rdf:Description>
  <rdf:Description rdf:about="" xmlns:cam="http://ns.adobe.com/camera-raw-settings/1.0/">
   <cam:Look>
    <rdf:Description cam:Name="Sibling" cam:UUID="UUID-SIBLING" cam:Amount="1"/>
   </cam:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#
    }

    #[test]
    fn read_crs_look_ignores_a_look_inside_a_sibling_description() {
        // The sibling's Look is a direct child of the second `rdf:Description`, so
        // it sits at the SAME depth (`main.depth + 1`) as the main description's
        // own direct children: only the main-description range check rejects it.
        let document = sibling_description_document();

        assert_eq!(
            read_crs_look(document).expect("parse"),
            None,
            "a Look inside a SIBLING rdf:Description must not be read as the main description's"
        );
    }

    #[test]
    fn read_and_write_agree_on_a_sibling_description_look() {
        // The SAME document: read and write must agree that the main description
        // has no Look, so `splice_crs_look` INSERTS one rather than treating the
        // sibling's Look as the main description's.
        let document = sibling_description_document();

        assert_eq!(
            find_crs_look_range(document).expect("parse"),
            None,
            "the sibling description's Look is not the main description's Look"
        );
        assert_eq!(read_crs_look(document).expect("parse"), None);

        let element = render_crs_look_element(
            "   ",
            Some(&reference(Some("UUID-NEW"), None, Some("New"), 100.0)),
        );
        let after = splice_crs_look(document, &element).expect("splice");

        // Exactly ONE `crs:Look` element afterwards — the inserted one, the only
        // Look in the main description...
        assert_eq!(after.matches("<crs:Look").count(), 1, "{after}");
        assert_eq!(after.matches("</crs:Look>").count(), 1, "{after}");
        assert!(after.contains("UUID-NEW"), "{after}");
        // ...while the sibling's own Look (a non-`crs` prefix bound to the same
        // namespace) is byte-untouched.
        assert_eq!(after.matches("<cam:Look>").count(), 1, "{after}");
        assert_eq!(after.matches("</cam:Look>").count(), 1, "{after}");
        assert!(after.contains("UUID-SIBLING"), "{after}");
    }

    #[test]
    fn find_crs_look_range_returns_none_when_absent() {
        assert_eq!(find_crs_look_range(&skeleton()).expect("parse"), None);
        assert_eq!(
            find_crs_look_range(&once_lab_style_skeleton()).expect("parse"),
            None
        );
    }

    #[test]
    fn find_crs_look_range_handles_the_self_closing_form() {
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Look crs:Name=""/>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

        let range = find_crs_look_range(document)
            .expect("parse")
            .expect("the self-closing Look must be found");

        assert_eq!(&document[range], "<crs:Look crs:Name=\"\"/>");
    }

    #[test]
    fn find_crs_look_range_handles_the_nested_description_form() {
        let document = populated_sidecar();
        let range = find_crs_look_range(&document)
            .expect("parse")
            .expect("the nested-description Look must be found");

        let slice = &document[range];

        assert!(slice.starts_with("<crs:Look>"), "{slice}");
        assert!(slice.ends_with("</crs:Look>"), "{slice}");
        assert!(slice.contains("UUID-OLD-LOOK"), "{slice}");
    }

    #[test]
    fn find_main_description_end_finds_the_direct_child_of_rdf_rdf() {
        // Hazards: the Look's OWN stub description has an explicit end tag (a
        // naive "first </rdf:Description>" lands on it), and a second direct
        // child follows the main description (a naive "last </rdf:Description>"
        // lands on that).
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Look>
    <rdf:Description crs:Name="Old" crs:UUID="U" crs:Amount="1">
    </rdf:Description>
   </crs:Look>
  </rdf:Description>
  <rdf:Description rdf:about="">
   <dc:title>second</dc:title>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

        let offset = find_main_description_end(document).expect("find the main description end");

        let first = document.find("</rdf:Description>").expect("first end tag");
        let last = document.rfind("</rdf:Description>").expect("last end tag");

        assert_ne!(offset, first, "the Look stub's end tag must not be chosen");
        assert_ne!(
            offset, last,
            "a trailing sibling description must not be chosen"
        );
        assert!(
            offset > first && offset < last,
            "offset {offset} in {first}..{last}"
        );

        assert_eq!(&document[offset..offset + 18], "</rdf:Description>");

        // Non-vacuity: the chosen end tag is the one that closes the description
        // containing the Look.
        let look_start = document.find("<crs:Look").expect("look");
        let look_end = document.find("</crs:Look>").expect("look end");
        assert!(look_start < look_end && look_end < offset);
    }

    #[test]
    fn splice_reports_a_contextual_error_on_a_malformed_document() {
        let malformed =
            "<x:xmpmeta><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"";

        let error = splice_crs_look(malformed, "  <crs:Look/>")
            .expect_err("a malformed document must be reported, not silently handled");

        assert!(!error.is_empty());
    }

    // --------------------------------------------------------- read (§1.6)

    #[test]
    fn read_crs_look_returns_none_when_absent() {
        assert_eq!(read_crs_look(&skeleton()).expect("parse"), None);
        assert_eq!(
            read_crs_look(&once_lab_style_skeleton()).expect("parse"),
            None
        );
    }

    #[test]
    fn read_crs_look_returns_none_for_the_empty_form() {
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Look crs:Name=""/>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        assert_eq!(read_crs_look(document).expect("parse"), None);
    }

    #[test]
    fn read_crs_look_reads_the_look_amount_and_never_the_authored_rgbtableamount() {
        // PROJECT-SYNTHETIC: `crs:Parameters` with `crs:RGBTableAmount` inside a
        // Look has zero occurrences in real Adobe output. This test pins only
        // that the user's Amount comes from the Look's own `crs:Amount` and never
        // from the authored table amount (6C3-R13).
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Look>
    <rdf:Description crs:Name="Look" crs:UUID="U" crs:Amount="1"/>
   </crs:Look>
   <crs:Parameters>
    <rdf:Description crs:RGBTableAmount="0.5"/>
   </crs:Parameters>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let look = read_crs_look(document)
            .expect("parse")
            .expect("the Look must be present");

        assert_eq!(
            look.amount_percent, 100.0,
            "the Look's crs:Amount=1 must win, never RGBTableAmount=0.5"
        );
    }

    #[test]
    fn read_crs_look_reads_the_look_amount_with_parameters_before_the_look() {
        // PROJECT-SYNTHETIC `crs:Parameters` block placed BEFORE the Look: the
        // reader must still take the Look's own `crs:Amount` (6C3-R13).
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Parameters>
    <rdf:Description crs:RGBTableAmount="0.5" crs:Amount="0.5"/>
   </crs:Parameters>
   <crs:Look>
    <rdf:Description crs:Name="Look" crs:UUID="U" crs:Amount="1"/>
   </crs:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let look = read_crs_look(document)
            .expect("parse")
            .expect("the Look must be present");

        assert_eq!(
            look.amount_percent, 100.0,
            "the Look's crs:Amount=1 must win over an earlier RGBTableAmount=0.5"
        );
    }

    #[test]
    fn read_crs_look_reads_the_look_amount_with_parameters_inside_the_look() {
        // PROJECT-SYNTHETIC `crs:Parameters` nested INSIDE the Look (its stub
        // description): the authored `crs:RGBTableAmount` must never be read as
        // the user's Amount (6C3-R13).
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Look>
    <rdf:Description crs:Name="Look" crs:UUID="U" crs:Amount="1">
     <crs:Parameters>
      <rdf:Description crs:RGBTableAmount="0.5" crs:Amount="0.5"/>
     </crs:Parameters>
    </rdf:Description>
   </crs:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let look = read_crs_look(document)
            .expect("parse")
            .expect("the Look must be present");

        assert_eq!(
            look.amount_percent, 100.0,
            "a nested RGBTableAmount=0.5 must never be read as the Amount"
        );
    }

    #[test]
    fn read_crs_look_rejects_an_out_of_domain_amount() {
        // `crs:Amount="5"` is 500 percent, outside this application's 0-200
        // percent domain: an explicit read must report it, not silently use it.
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Look>
    <rdf:Description crs:Name="Too Much" crs:UUID="U" crs:Amount="5"/>
   </crs:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let error = read_crs_look(document)
            .expect_err("a 500 percent amount is outside the supported domain");
        assert!(error.contains("0..=2"), "{error}");
        assert!(error.contains("200"), "{error}");
    }

    #[test]
    fn read_crs_look_defaults_a_missing_amount_to_100() {
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Look>
    <rdf:Description crs:Name="No Amount" crs:UUID="U"/>
   </crs:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        assert_eq!(
            read_crs_look(document)
                .expect("parse")
                .expect("present")
                .amount_percent,
            100.0
        );
    }

    #[test]
    fn read_crs_look_rejects_an_unparseable_amount() {
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Look>
    <rdf:Description crs:Name="Bad" crs:UUID="U" crs:Amount="abc"/>
   </crs:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        assert!(
            read_crs_look(document).is_err(),
            "an unparseable crs:Amount is a contextual error, never guessed"
        );
    }

    #[test]
    fn read_crs_look_reads_the_once_lab_private_fields() {
        let reference = reference(
            Some("UUID-P"),
            Some("/profiles/private.xmp"),
            Some("Private"),
            42.5,
        );

        let element = render_crs_look_element("    ", Some(&reference));
        let document = splice_crs_look(&skeleton(), &element).expect("splice");

        let look = read_crs_look(&document)
            .expect("parse")
            .expect("the Look must be present");

        assert_eq!(look.name.as_deref(), Some("Private"));
        assert_eq!(look.uuid.as_deref(), Some("UUID-P"));
        assert_eq!(look.path.as_deref(), Some("/profiles/private.xmp"));
        assert_eq!(look.amount_percent, 42.5);
    }

    #[test]
    fn read_crs_look_ignores_private_fields_from_a_newer_schema() {
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description>
   <crs:Look xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
    <rdf:Description crs:Name="Future" crs:UUID="UUID-FUTURE" crs:Amount="1"
      xmlns:onelab="http://once-lab.invalid/ns/develop/1.0/"
      onelab:SchemaVersion="2" onelab:ProfilePath="/profiles/future.xmp"/>
   </crs:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let look = read_crs_look(document)
            .expect("parse")
            .expect("the Adobe part must still be read");

        assert_eq!(look.name.as_deref(), Some("Future"));
        assert_eq!(look.uuid.as_deref(), Some("UUID-FUTURE"));
        assert_eq!(look.amount_percent, 100.0);
        assert_eq!(
            look.path, None,
            "private fields from a newer schema must be discarded"
        );
    }

    #[test]
    fn write_then_read_round_trips_a_profile() {
        let reference = reference(
            Some("UUID-RT"),
            Some("/profiles/rt.xmp"),
            Some("Round Trip"),
            137.5,
        );

        let element = render_crs_look_element(" ", Some(&reference));
        let document = splice_crs_look(&skeleton(), &element).expect("splice");

        let look = read_crs_look(&document)
            .expect("parse")
            .expect("the Look must be present");

        assert_eq!(look.uuid.as_deref(), Some("UUID-RT"));
        assert_eq!(look.path.as_deref(), Some("/profiles/rt.xmp"));
        assert_eq!(look.name.as_deref(), Some("Round Trip"));
        assert_eq!(look.amount_percent, 137.5);
    }

    #[test]
    fn a_non_crs_prefix_bound_to_the_crs_namespace_is_still_read() {
        let document = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description xmlns:cam="http://ns.adobe.com/camera-raw-settings/1.0/">
   <cam:Look>
    <rdf:Description cam:Name="Cam" cam:UUID="UUID-CAM" cam:Amount="0.5"/>
   </cam:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        let look = read_crs_look(document)
            .expect("parse")
            .expect("the differently-prefixed Look must be read");

        assert_eq!(look.name.as_deref(), Some("Cam"));
        assert_eq!(look.uuid.as_deref(), Some("UUID-CAM"));
        assert_eq!(look.amount_percent, 50.0);
        assert!(
            find_crs_look_range(document).expect("parse").is_some(),
            "find_crs_look_range must be namespace-aware too"
        );
    }

    // ------------------------------------------------------ commands (§1.7)

    #[test]
    fn write_creates_a_missing_sidecar_with_a_valid_document() {
        let dir = unique_temp_dir("create");
        let image = dir.join("photo.NEF");
        write_image(&image);

        let outcome = write_develop_state_to_xmp_at(&image).expect("write must succeed");

        assert!(
            outcome.created,
            "the sidecar did not exist, so it was created"
        );
        assert!(
            !outcome.wrote_profile,
            "no internal profile -> the empty form"
        );

        let xmp = dir.join("photo.xmp");
        assert!(xmp.exists(), "the sidecar must exist at {xmp:?}");

        let document = fs::read_to_string(&xmp).expect("read");
        assert!(find_crs_look_range(&document).expect("parse").is_some());
        assert_eq!(read_crs_look(&document).expect("read"), None);

        let second = write_develop_state_to_xmp_at(&image).expect("second write");
        assert!(!second.created, "the sidecar already existed");

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn write_refuses_a_profile_file() {
        let dir = unique_temp_dir("refuse_profile");
        let image = dir.join("photo.NEF");
        write_image(&image);

        let xmp = dir.join("photo.xmp");
        fs::write(&xmp, valid_profile_xmp("Pro", "UUID-PRO")).expect("write profile");
        let before = fs::read(&xmp).expect("read profile");

        let error = write_develop_state_to_xmp_at(&image)
            .expect_err("a creative profile file must be refused");

        assert!(error.contains("profile"), "{error}");
        assert_eq!(
            fs::read(&xmp).expect("re-read profile"),
            before,
            "the profile file must be byte-unchanged"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn write_refuses_an_unreadable_target() {
        // A target that exists but cannot be read as UTF-8 cannot be proven safe,
        // so the classification `Err` arm refuses the write. This exercises the
        // guard portably, without depending on filesystem permissions.
        let dir = unique_temp_dir("unreadable");
        let image = dir.join("photo.NEF");
        write_image(&image);

        let xmp = dir.join("photo.xmp");
        fs::write(&xmp, [0xff_u8, 0xfe, 0x00, 0x01]).expect("write unreadable target");
        let before = fs::read(&xmp).expect("read target");

        let error = write_develop_state_to_xmp_at(&image)
            .expect_err("an unreadable target must be refused");

        assert!(!error.is_empty());
        assert_eq!(
            fs::read(&xmp).expect("re-read target"),
            before,
            "the refused target must be byte-unchanged"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn write_clears_with_the_empty_look_form() {
        let dir = unique_temp_dir("clear");
        let image = dir.join("photo.NEF");
        write_image(&image);

        let sidecar = dir.join("photo.NEF.rrdata");
        fs::write(
            &sidecar,
            sidecar_with_profile("UUID-X", "/profiles/x.xmp", "X", 50.0),
        )
        .expect("write sidecar");

        let first = write_develop_state_to_xmp_at(&image).expect("write");
        assert!(first.wrote_profile);

        let xmp = dir.join("photo.xmp");
        let document = fs::read_to_string(&xmp).expect("read");
        assert!(document.contains("crs:UUID=\"UUID-X\""), "{document}");

        // Clear the internal state and write again.
        fs::write(
            &sidecar,
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "rating": 0,
                "adjustments": null,
                "develop_state": { "version": 1 },
            }))
            .expect("serialize"),
        )
        .expect("write cleared sidecar");

        let cleared = write_develop_state_to_xmp_at(&image).expect("clear write");
        assert!(!cleared.wrote_profile);

        let document = fs::read_to_string(&xmp).expect("read");
        assert!(document.contains("crs:Name=\"\""), "{document}");
        assert!(!document.contains("crs:UUID"), "{document}");
        assert_eq!(read_crs_look(&document).expect("read"), None);

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn write_does_not_touch_the_rrdata_or_the_image() {
        let dir = unique_temp_dir("untouched");
        let image = dir.join("photo.NEF");
        write_image(&image);

        let sidecar = dir.join("photo.NEF.rrdata");
        fs::write(
            &sidecar,
            sidecar_with_profile("UUID-X", "/profiles/x.xmp", "X", 100.0),
        )
        .expect("write sidecar");

        let image_before = fs::read(&image).expect("read image");
        let sidecar_before = fs::read(&sidecar).expect("read sidecar");
        let image_mtime = fs::metadata(&image)
            .expect("image meta")
            .modified()
            .expect("mtime");
        let sidecar_mtime = fs::metadata(&sidecar)
            .expect("sidecar meta")
            .modified()
            .expect("mtime");

        write_develop_state_to_xmp_at(&image).expect("write");

        assert_eq!(fs::read(&image).expect("re-read image"), image_before);
        assert_eq!(fs::read(&sidecar).expect("re-read sidecar"), sidecar_before);
        assert_eq!(
            fs::metadata(&image)
                .expect("image meta")
                .modified()
                .expect("mtime"),
            image_mtime,
            "the image mtime must be unchanged"
        );
        assert_eq!(
            fs::metadata(&sidecar)
                .expect("sidecar meta")
                .modified()
                .expect("mtime"),
            sidecar_mtime,
            "the .rrdata mtime must be unchanged"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn read_reports_a_contextual_error_for_a_missing_sidecar() {
        let dir = unique_temp_dir("missing_read");
        let image = dir.join("photo.NEF");
        write_image(&image);

        let error = read_develop_state_from_xmp_at(&image, &[])
            .expect_err("an explicit read of a missing sidecar is a user-visible failure");

        assert!(
            error.contains("photo.NEF"),
            "the error must name the path: {error}"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn write_leaves_rating_label_and_tags_alone() {
        let dir = unique_temp_dir("rating_label_tags");
        let image = dir.join("photo.NEF");
        write_image(&image);

        let rating = "<xmp:Rating>4</xmp:Rating>";
        let label = "<xmp:Label>Red</xmp:Label>";
        let subject = "<dc:subject><rdf:Bag><rdf:li>Travel</rdf:li><rdf:li>Sunset</rdf:li></rdf:Bag></dc:subject>";

        let document = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 5.6-c140">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
   {rating}
   {label}
   {subject}
   <crs:Look>
    <rdf:Description crs:Name="Old" crs:UUID="UUID-OLD" crs:Amount="1"/>
   </crs:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#
        );

        let xmp = dir.join("photo.xmp");
        fs::write(&xmp, &document).expect("write sidecar");

        write_develop_state_to_xmp_at(&image).expect("write");

        let after = fs::read_to_string(&xmp).expect("read");

        assert!(
            after.contains(rating),
            "rating must be byte-identical: {after}"
        );
        assert!(
            after.contains(label),
            "label must be byte-identical: {after}"
        );
        assert!(
            after.contains(subject),
            "tags must be byte-identical: {after}"
        );
        assert!(
            after.contains("crs:Name=\"\""),
            "the develop Look was cleared: {after}"
        );
        assert!(!after.contains("UUID-OLD"), "{after}");

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn the_written_sidecar_is_not_classified_as_a_profile() {
        let dir = unique_temp_dir("classification");
        let image = dir.join("photo.NEF");
        write_image(&image);

        write_develop_state_to_xmp_at(&image).expect("write");

        let xmp = dir.join("photo.xmp");
        assert!(
            matches!(
                crate::xmp_profile::discovery::classify_xmp_profile_path(&xmp),
                Ok(None)
            ),
            "our written sidecar must never enter the profile library"
        );

        // Non-vacuity: a real profile at the same extension IS classified.
        let profile = dir.join("real_profile.xmp");
        fs::write(&profile, valid_profile_xmp("Real", "UUID-REAL")).expect("write profile");
        assert!(
            matches!(
                crate::xmp_profile::discovery::classify_xmp_profile_path(&profile),
                Ok(Some(_))
            ),
            "the fixture must actually be a discoverable profile"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn write_refuses_a_virtual_copy_path() {
        let error = write_develop_state_to_xmp("/some/photo.NEF?vc=1234".to_string())
            .expect_err("a virtual copy must be refused");

        assert!(error.contains("virtual copy"), "{error}");
    }

    #[test]
    fn write_develop_state_to_xmp_at_also_refuses_a_virtual_copy_path() {
        // The helper de-virtualizes and writes the physical sidecar, so the guard
        // must live here too — not only in the command — for any future caller.
        let error = write_develop_state_to_xmp_at(Path::new("/some/photo.NEF?vc=1234"))
            .expect_err("the write body must refuse a virtual copy path too");

        assert!(error.contains("virtual copy"), "{error}");
    }

    #[test]
    fn write_refuses_a_newer_develop_state_version() {
        let dir = unique_temp_dir("newer_version");
        let image = dir.join("photo.NEF");
        write_image(&image);

        let sidecar = dir.join("photo.NEF.rrdata");
        fs::write(
            &sidecar,
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "rating": 0,
                "adjustments": null,
                "develop_state": { "version": 2 },
            }))
            .expect("serialize"),
        )
        .expect("write newer sidecar");

        let error = write_develop_state_to_xmp_at(&image)
            .expect_err("a newer develop-state version must never be reinterpreted");

        assert!(error.contains("newer"), "{error}");
        assert!(
            error.contains("(2)"),
            "the error must name the unsupported version: {error}"
        );
        assert!(
            !dir.join("photo.xmp").exists(),
            "the refused write must not create a sidecar"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn writing_twice_is_idempotent_for_a_replace() {
        let dir = unique_temp_dir("idempotent_replace");
        let image = dir.join("photo.NEF");
        write_image(&image);

        let sidecar = dir.join("photo.NEF.rrdata");
        fs::write(
            &sidecar,
            sidecar_with_profile("UUID-X", "/profiles/x.xmp", "X", 42.0),
        )
        .expect("write sidecar");

        // The target already has a Look, so the first write REPLACES it.
        let xmp = dir.join("photo.xmp");
        fs::write(&xmp, populated_sidecar()).expect("write existing sidecar");

        write_develop_state_to_xmp_at(&image).expect("first write");
        let first = fs::read(&xmp).expect("read");
        let first_indent = look_line_indent(std::str::from_utf8(&first).expect("utf8"));

        write_develop_state_to_xmp_at(&image).expect("second write");
        let second = fs::read(&xmp).expect("read");

        assert_eq!(
            first, second,
            "writing the same profile twice must be byte-identical"
        );
        assert_eq!(
            look_line_indent(std::str::from_utf8(&second).expect("utf8")),
            first_indent,
            "the rendered Look indentation must not grow between writes"
        );
        assert!(
            std::str::from_utf8(&second)
                .expect("utf8")
                .contains("UUID-X"),
            "the profile must be present"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn writing_twice_is_idempotent_for_an_insert() {
        let dir = unique_temp_dir("idempotent_insert");
        let image = dir.join("photo.NEF");
        write_image(&image);

        let sidecar = dir.join("photo.NEF.rrdata");
        fs::write(
            &sidecar,
            sidecar_with_profile("UUID-Y", "/profiles/y.xmp", "Y", 65.0),
        )
        .expect("write sidecar");

        // No sidecar yet: the first write INSERTS, the second REPLACES.
        let xmp = dir.join("photo.xmp");

        write_develop_state_to_xmp_at(&image).expect("first write");
        let first = fs::read_to_string(&xmp).expect("read");
        let first_indent = look_line_indent(&first);

        write_develop_state_to_xmp_at(&image).expect("second write");
        let second = fs::read_to_string(&xmp).expect("read");

        assert_eq!(first, second, "insert then replace must be byte-identical");
        assert_eq!(
            look_line_indent(&second),
            first_indent,
            "the rendered Look indentation must not grow between writes"
        );
        assert!(second.contains("UUID-Y"), "{second}");
        assert_eq!(second.matches("<crs:Look").count(), 1, "{second}");

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }
}
