//! Read-only discovery of supported Adobe XMP RGB profiles.
//!
//! A `.xmp` extension is not proof of a profile: the same extension covers
//! develop presets, RAW sidecars and unrelated metadata. Every candidate is
//! therefore routed through the approved parser, and anything that does not
//! parse into the supported color profile shape is skipped rather than
//! surfaced to the frontend.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

use crate::xmp_profile::XmpRgbProfile;
use crate::xmp_profile::loader::has_xmp_extension;
use crate::xmp_profile::parser::parse_xmp_rgb_profile;
use crate::xmp_profile::summary::normalize_path_string;

/// Frontend-facing description of a discoverable XMP RGB profile.
///
/// Deliberately excludes the decoded RGBTable, its byte payload and the
/// authored `crs:RGBTableAmount`: the backend stays authoritative and the
/// frontend never parses XMP itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct XmpProfileEntry {
    pub name: String,
    pub group: Option<String>,
    pub uuid: String,
    pub path: String,
    pub supports_amount: bool,
    /// The authored `crs:ConvertToGrayscale` flag, surfaced verbatim. It is not a
    /// render step and does not filter discovery.
    pub convert_to_grayscale: bool,
}

/// Classifies a single path without ever failing on unsupported content.
///
/// * non-`.xmp` path → `Ok(None)`
/// * readable but not a supported color RGB profile → `Ok(None)`
/// * readable supported profile → `Ok(Some(entry))`
/// * unreadable path (missing file, permissions, non-UTF-8) → `Err(context)`
///
/// Only the read step is duplicated from the loader; parsing is delegated to
/// the approved [`parse_xmp_rgb_profile`] so no profile semantics live here.
pub(crate) fn classify_xmp_profile_path(path: &Path) -> Result<Option<XmpProfileEntry>, String> {
    if !has_xmp_extension(path) {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read XMP profile '{}': {error}", path.display()))?;

    match parse_xmp_rgb_profile(&contents) {
        Ok(profile) => Ok(Some(entry_from_profile(&profile, path))),
        // Presets, RAW sidecars and malformed documents all mean the same thing
        // to a library scan: not a supported profile. Grayscale (`Look`)
        // profiles parse like any other, so they are discoverable — the
        // `crs:ConvertToGrayscale` flag is carried, never used to filter.
        Err(error) => {
            log::debug!(
                "Skipping XMP file that is not a supported RGB profile '{}': {error}",
                path.display()
            );
            Ok(None)
        }
    }
}

/// Discovers every supported XMP RGB profile under `roots`.
///
/// Scans are recursive. Symlinked directories are never descended into, so a
/// symlink cycle cannot walk the scan outside the caller-provided roots;
/// symlinked profile files are read like any other file. Individual unreadable
/// or unsupported files are skipped, while a root that is missing or unreadable
/// fails the scan with a contextual error.
///
/// Deduplication is by walked path, so repeated or overlapping roots cannot
/// return the same file twice. Discovery never canonicalizes, so the same file
/// reached through two different spellings of a root (for example `profiles`
/// and `./profiles`) is not recognised as a duplicate.
///
/// An empty `roots` slice discovers nothing; scanning is never implicit.
pub(crate) fn discover_xmp_profiles(roots: &[PathBuf]) -> Result<Vec<XmpProfileEntry>, String> {
    let mut entries: Vec<XmpProfileEntry> = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();

    for root in roots {
        if !root.is_dir() {
            return Err(format!(
                "XMP profile directory does not exist or is not a directory: {}",
                root.display()
            ));
        }

        std::fs::read_dir(root).map_err(|error| {
            format!(
                "failed to read XMP profile directory '{}': {error}",
                root.display()
            )
        })?;

        for walked in WalkDir::new(root) {
            let walked = match walked {
                Ok(walked) => walked,
                Err(error) => {
                    log::warn!("Skipping unreadable path while scanning XMP profiles: {error}");
                    continue;
                }
            };

            let path = walked.path();
            if !has_xmp_extension(path) || !path.is_file() || !visited.insert(path.to_path_buf()) {
                continue;
            }

            match classify_xmp_profile_path(path) {
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => {}
                Err(error) => log::warn!("Skipping unusable XMP file: {error}"),
            }
        }
    }

    entries.sort_by_key(profile_sort_key);
    Ok(entries)
}

/// Sort order: grouped entries first by case-insensitive group, then by
/// case-insensitive name, then by path.
fn profile_sort_key(entry: &XmpProfileEntry) -> (bool, String, String, String) {
    (
        entry.group.is_none(),
        entry.group.as_deref().unwrap_or_default().to_lowercase(),
        entry.name.to_lowercase(),
        entry.path.clone(),
    )
}

fn entry_from_profile(profile: &XmpRgbProfile, path: &Path) -> XmpProfileEntry {
    XmpProfileEntry {
        name: profile.name.clone(),
        group: profile.group.clone(),
        uuid: profile.uuid.clone(),
        path: normalize_path_string(path),
        supports_amount: profile.supports_amount,
        convert_to_grayscale: profile.convert_to_grayscale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Synthetic 2x2x2 identity RGBTable (same fixture pattern as the parser,
    /// rgb_table, loader and summary tests).
    const IDENTITY_2X2X2_BASE85: &str = "71000rtKmwBRLy1{X$/w9'=(bLC9YEvFE(9%a0@fg0";

    fn unique_temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);

        let dir = std::env::temp_dir().join(format!(
            "once_lab_xmp_discovery_{}_{}_{}",
            std::process::id(),
            nanos,
            counter
        ));

        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Writes `contents` to `dir/name`, creating parent directories.
    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }

        fs::write(&path, contents).expect("write temp file");
        path
    }

    /// Supported color profile whose embedded identity is independent of the
    /// file name. `crs:RGBTableAmount` is intentionally absent: discovery must
    /// not depend on authored amount metadata.
    fn profile_xmp(name: &str, group: Option<&str>, uuid: &str, supports_amount: &str) -> String {
        let group_block = group
            .map(|group| {
                format!(
                    r#"
      <crs:Group>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">{group}</rdf:li>
        </rdf:Alt>
      </crs:Group>"#
                )
            })
            .unwrap_or_default();

        format!(
            r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:PresetType="Look"
      crs:UUID="{uuid}"
      crs:SupportsAmount="{supports_amount}"
      crs:ConvertToGrayscale="False"
      crs:RGBTable="TESTTABLE"
      crs:Table_TESTTABLE="{table}">
      <crs:Name>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">{name}</rdf:li>
        </rdf:Alt>
      </crs:Name>{group_block}
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#,
            table = IDENTITY_2X2X2_BASE85
        )
    }

    /// Fe-class fixture: a grayscale (`crs:ConvertToGrayscale="True"`) Look
    /// profile. It must be discovered like any color profile, with the flag
    /// carried through.
    fn grayscale_profile_xmp(name: &str, uuid: &str) -> String {
        format!(
            r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:PresetType="Look"
      crs:UUID="{uuid}"
      crs:SupportsAmount="True"
      crs:ConvertToGrayscale="True"
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

    /// Valid XMP that is a develop preset, not a profile.
    fn preset_xmp() -> String {
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:PresetType="Normal"
      crs:UUID="PRESET123"
      crs:ProcessVersion="11.0">
      <crs:Name>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">Some Develop Preset</rdf:li>
        </rdf:Alt>
      </crs:Name>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#
            .to_string()
    }

    /// Writes raw bytes to `dir/name`, for fixtures that are not valid UTF-8.
    fn write_bytes(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("write temp bytes");
        path
    }

    /// Adobe Base85 (`dng_big_table.cpp` `kEncodeTable`): 5 characters encode a
    /// 4-byte little-endian word, and a final short group encodes `len - 1`
    /// bytes. Mirrors the decoder in `rgb_table.rs`; test-only.
    fn base85_encode(data: &[u8]) -> String {
        use crate::xmp_profile::rgb_table::ADOBE_BASE85_ALPHABET;

        let mut out = String::new();

        let encode_group = |out: &mut String, bytes: &[u8]| {
            let mut value = 0u32;
            for (index, byte) in bytes.iter().enumerate() {
                value += u32::from(*byte) << (8 * index);
            }
            for _ in 0..bytes.len() + 1 {
                out.push(ADOBE_BASE85_ALPHABET[(value % 85) as usize] as char);
                value /= 85;
            }
        };

        let full = data.len() - (data.len() % 4);
        for chunk in data[..full].chunks(4) {
            encode_group(&mut out, chunk);
        }
        if !data[full..].is_empty() {
            encode_group(&mut out, &data[full..]);
        }

        out
    }

    /// Wraps a raw big-table body in the `[u32 LE length][zlib]` + Base85 framing.
    fn encode_big_table(body: &[u8]) -> String {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write;

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body).expect("test zlib compression");

        let mut payload = (body.len() as u32).to_le_bytes().to_vec();
        payload.extend_from_slice(&encoder.finish().expect("test zlib finish"));

        base85_encode(&payload)
    }

    /// A genuinely well-formed, Adobe-legal **1-D** RGBTable big-table payload.
    ///
    /// `dng_rgb_table::GetStream` accepts `dimensions == 1` with
    /// `kMinDivisions1D..=kMaxDivisions1D` (`dng_big_table.cpp:2463-2472`), so this
    /// is a shape Adobe itself writes. Once-Lab supports only 3-D tables, so a
    /// profile carrying one is a real "unsupported but well-formed Adobe profile".
    /// 16 divisions keeps the block inside the 3-D size window so it is rejected
    /// for its SHAPE, not its byte length.
    fn one_dimensional_rgb_table_payload() -> String {
        const DIVISIONS: u32 = 16;
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes()); // btt_RGBTable
        body.extend_from_slice(&1u32.to_le_bytes()); // version 1
        body.extend_from_slice(&1u32.to_le_bytes()); // dimensions = 1
        body.extend_from_slice(&DIVISIONS.to_le_bytes());
        for index in 0..DIVISIONS {
            let sample = (index as u16) * 4000;
            for _ in 0..3 {
                body.extend_from_slice(&sample.to_le_bytes());
            }
        }
        body.extend_from_slice(&0u32.to_le_bytes()); // primaries sRGB
        body.extend_from_slice(&1u32.to_le_bytes()); // gamma sRGB
        body.extend_from_slice(&0u32.to_le_bytes()); // gamut clip
        body.extend_from_slice(&0.0f64.to_le_bytes()); // min amount
        body.extend_from_slice(&1.0f64.to_le_bytes()); // max amount
        encode_big_table(&body)
    }

    /// A 3-D identity RGBTable big-table payload re-encoded in the tests, so a
    /// fixture can swap only the footer metadata (primaries/gamma/gamut). Zero
    /// per-node deltas reconstruct each axis's identity ramp, i.e. an identity
    /// table.
    fn three_dimensional_rgb_table_payload(primaries: u32, gamma: u32, gamut: u32) -> String {
        const SIZE: u32 = 2;
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes()); // btt_RGBTable
        body.extend_from_slice(&1u32.to_le_bytes()); // version 1
        body.extend_from_slice(&3u32.to_le_bytes()); // dimensions = 3
        body.extend_from_slice(&SIZE.to_le_bytes());
        for _ in 0..(SIZE * SIZE * SIZE) {
            body.extend_from_slice(&0u16.to_le_bytes()); // red delta
            body.extend_from_slice(&0u16.to_le_bytes()); // green delta
            body.extend_from_slice(&0u16.to_le_bytes()); // blue delta
        }
        body.extend_from_slice(&primaries.to_le_bytes());
        body.extend_from_slice(&gamma.to_le_bytes());
        body.extend_from_slice(&gamut.to_le_bytes());
        body.extend_from_slice(&0.0f64.to_le_bytes());
        body.extend_from_slice(&1.0f64.to_le_bytes());
        encode_big_table(&body)
    }

    /// A profile-shaped XMP whose ONLY problem is the embedded RGBTable: it uses
    /// `table_payload` instead of the standard identity table.
    fn profile_xmp_with_table(
        name: &str,
        uuid: &str,
        supports_amount: &str,
        table_payload: &str,
    ) -> String {
        format!(
            r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:PresetType="Look"
      crs:UUID="{uuid}"
      crs:SupportsAmount="{supports_amount}"
      crs:ConvertToGrayscale="False"
      crs:RGBTable="TESTTABLE"
      crs:Table_TESTTABLE="{table_payload}">
      <crs:Name>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">{name}</rdf:li>
        </rdf:Alt>
      </crs:Name>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#
        )
    }

    /// Synthetic replicas of the four real Adobe application-metadata `.xmp`
    /// files: each carries an `rdf:Description` and the `crs:` namespace but has
    /// NO `crs:PresetType`. Shapes taken from the installed Adobe tree (never
    /// copied from it). List lengths are shortened; the element/attribute shapes
    /// are faithful.
    fn adobe_app_metadata_documents() -> Vec<(&'static str, String)> {
        vec![
            (
                "RawDefaults.xmp",
                r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000 1.000000, 0000/00/00-00:00:00        ">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:RawDefaults>
    <rdf:Seq>
     <rdf:li
      crs:Defaults="Adobe"
      crs:MasterOnly="True"/>
    </rdf:Seq>
   </crs:RawDefaults>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#
                    .to_string(),
            ),
            (
                "Preferences.xmp",
                r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000 1.000000, 0000/00/00-00:00:00        ">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
   crs:RawDefaultsElements="Adobe"
   crs:DNGSidecarHandling="0"
   crs:NegativeCachePath=""
   crs:NegativeCacheMaximumSize="5.0"
   crs:NegativeCacheLargePreviewSize="2048"
   crs:JPEGHandling="OpenIfHasSettings"
   crs:TIFFHandling="OpenIfHasSettings"/>
 </rdf:RDF>
</x:xmpmeta>"#
                    .to_string(),
            ),
            (
                "FavoriteStyles.xmp",
                r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000 1.000000, 0000/00/00-00:00:00        ">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:HiddenPresetGroups2>
    <rdf:Bag>
     <rdf:li
      crs:ID="00FB827C51C6EC44D394668160AC6E61"
      crs:Hidden="True"/>
     <rdf:li
      crs:ID="0328DAA3F987A293F77FA0E7B0E0626B"
      crs:Hidden="True"/>
    </rdf:Bag>
   </crs:HiddenPresetGroups2>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#
                    .to_string(),
            ),
            (
                "TimeEstimates.xmp",
                r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000 1.000000, 0000/00/00-00:00:00        ">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
   <crs:Recorders rdf:parseType="Resource">
    <crs:RecorderList>
     <rdf:Seq>
      <rdf:li>
       <rdf:Description
        crs:name="adaptive_profile"
        crs:max_entries="5">
       <crs:times>
        <rdf:Seq>
         <rdf:li>14.32020895832102</rdf:li>
        </rdf:Seq>
       </crs:times>
       </rdf:Description>
      </rdf:li>
     </rdf:Seq>
    </crs:RecorderList>
   </crs:Recorders>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#
                    .to_string(),
            ),
        ]
    }

    /// Mirrors the shape a RAW editor writes next to a RAW file: develop
    /// settings containing a `crs:Look` block and embedded RGBTable data, but
    /// no `crs:PresetType`. This must never be mistaken for a profile.
    fn raw_sidecar_xmp() -> String {
        format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:Version="18.5.1"
      crs:ProcessVersion="15.4"
      crs:WhiteBalance="As Shot"
      crs:Exposure2012="0.00"
      crs:CameraProfile="Adobe Standard"
      crs:HasSettings="True">
      <crs:Look>
        <rdf:Description
          crs:Name="Embedded Look"
          crs:Amount="1"
          crs:UUID="LOOKUUID">
          <crs:Parameters>
            <rdf:Description
              crs:Version="18.5.1"
              crs:ConvertToGrayscale="False"
              crs:RGBTable="SIDECARTABLE"
              crs:RGBTableAmount="0.5"
              crs:Table_SIDECARTABLE="{table}">
            </rdf:Description>
          </crs:Parameters>
        </rdf:Description>
      </crs:Look>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#,
            table = IDENTITY_2X2X2_BASE85
        )
    }

    #[test]
    fn discovers_valid_rgb_profile() {
        let dir = unique_temp_dir();
        write_file(
            &dir,
            "sample.xmp",
            &profile_xmp("Sample", Some("Group"), "UUID-A", "True"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan should succeed");

        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0],
            XmpProfileEntry {
                name: "Sample".to_string(),
                group: Some("Group".to_string()),
                uuid: "UUID-A".to_string(),
                path: dir.join("sample.xmp").to_string_lossy().to_string(),
                supports_amount: true,
                convert_to_grayscale: false,
            }
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn discovers_grayscale_profile_without_special_casing() {
        let dir = unique_temp_dir();
        // The file name deliberately gives no hint about the profile, so any
        // filename-based exemption or skip would be visible here.
        write_file(
            &dir,
            "some-arbitrary-name.xmp",
            &grayscale_profile_xmp("Mono ⛏️", "UUID-MONO"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan should succeed");

        assert_eq!(
            entries.len(),
            1,
            "a grayscale (ConvertToGrayscale=True) profile must be discovered, not skipped"
        );
        assert_eq!(entries[0].name, "Mono ⛏️");
        assert_eq!(entries[0].uuid, "UUID-MONO");
        assert!(
            entries[0].convert_to_grayscale,
            "the monochrome flag must survive discovery"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn ignores_non_xmp_files() {
        let dir = unique_temp_dir();
        write_file(
            &dir,
            "sample.xmp",
            &profile_xmp("Sample", None, "UUID-A", "True"),
        );
        write_file(&dir, "notes.txt", "not a profile");
        write_file(&dir, "photo.NEF", "not a profile");
        write_file(&dir, "photo.NEF.rrdata", "{\"tags\":[]}");
        write_file(&dir, "photo.acr", "not a profile");

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan should succeed");

        assert_eq!(entries.len(), 1, "only the .xmp profile may be discovered");
        assert_eq!(entries[0].name, "Sample");

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn extension_is_case_insensitive() {
        let dir = unique_temp_dir();
        write_file(
            &dir,
            "upper.XMP",
            &profile_xmp("Upper", None, "UUID-U", "True"),
        );
        write_file(
            &dir,
            "mixed.Xmp",
            &profile_xmp("Mixed", None, "UUID-M", "True"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan should succeed");

        let mut names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["Mixed", "Upper"]);

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn skips_malformed_xmp_without_failing_scan() {
        let dir = unique_temp_dir();
        write_file(&dir, "broken.xmp", "<x:xmpmeta><rdf:RDF");
        write_file(&dir, "empty.xmp", "");
        write_file(&dir, "binary.xmp", "\u{FFFD}\u{0}not xml");
        write_file(
            &dir,
            "good.xmp",
            &profile_xmp("Good", None, "UUID-G", "True"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan must survive bad files");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Good");

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn skips_non_profile_xmp() {
        let dir = unique_temp_dir();
        write_file(&dir, "preset.xmp", &preset_xmp());
        write_file(&dir, "sidecar.xmp", &raw_sidecar_xmp());
        write_file(
            &dir,
            "good.xmp",
            &profile_xmp("Good", None, "UUID-G", "True"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan should succeed");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Good");

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn returns_multiple_profiles() {
        let dir = unique_temp_dir();
        write_file(&dir, "one.xmp", &profile_xmp("One", None, "UUID-1", "True"));
        write_file(
            &dir,
            "two.xmp",
            &profile_xmp("Two", None, "UUID-2", "False"),
        );
        write_file(
            &dir,
            "three.xmp",
            &profile_xmp("Three", None, "UUID-3", "True"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan should succeed");

        assert_eq!(entries.len(), 3);

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn deterministic_sorting() {
        let dir = unique_temp_dir();
        write_file(
            &dir,
            "f1.xmp",
            &profile_xmp("Bravo", Some("alpha"), "UUID-1", "True"),
        );
        write_file(
            &dir,
            "f2.xmp",
            &profile_xmp("Alpha", Some("alpha"), "UUID-2", "True"),
        );
        write_file(
            &dir,
            "f3.xmp",
            &profile_xmp("Charlie", Some("Beta"), "UUID-3", "True"),
        );
        write_file(
            &dir,
            "f4.xmp",
            &profile_xmp("Delta", None, "UUID-4", "True"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan should succeed");
        let observed: Vec<(&str, Option<&str>)> = entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.group.as_deref()))
            .collect();

        // Case-insensitive group order (alpha before Beta), name order within a
        // group, and ungrouped profiles last.
        assert_eq!(
            observed,
            [
                ("Alpha", Some("alpha")),
                ("Bravo", Some("alpha")),
                ("Charlie", Some("Beta")),
                ("Delta", None),
            ]
        );

        // Repeated scans of unchanged inputs must agree.
        let again = discover_xmp_profiles(&[dir.clone()]).expect("rescan should succeed");
        assert_eq!(entries, again);

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn duplicate_paths_not_returned_twice() {
        let dir = unique_temp_dir();
        write_file(
            &dir,
            "nested/deep.xmp",
            &profile_xmp("Deep", None, "UUID-D", "True"),
        );
        // The same root twice, plus an overlapping parent/child pair.
        let entries = discover_xmp_profiles(&[dir.clone(), dir.clone(), dir.join("nested")])
            .expect("scan should succeed");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Deep");

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    /// Path is the identity at this milestone: two installations of the same
    /// profile share a UUID but are distinct files, and both stay visible.
    #[test]
    fn duplicate_uuids_at_different_paths_are_both_returned() {
        let dir = unique_temp_dir();
        write_file(
            &dir,
            "install-a/Shared.xmp",
            &profile_xmp("Shared A", None, "SHARED-UUID", "True"),
        );
        write_file(
            &dir,
            "install-b/Shared.xmp",
            &profile_xmp("Shared B", None, "SHARED-UUID", "True"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan should succeed");

        assert_eq!(
            entries.len(),
            2,
            "a UUID match must not silently collapse distinct files"
        );
        assert!(entries.iter().all(|entry| entry.uuid == "SHARED-UUID"));
        assert_ne!(entries[0].path, entries[1].path);

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    /// Symlink policy: a symlinked profile file is read, but a symlinked
    /// directory is never descended into, so the scan cannot walk outside the
    /// root it was given.
    #[cfg(unix)]
    #[test]
    fn symlinked_file_is_read_but_symlinked_directory_is_not_descended() {
        use std::os::unix::fs::symlink;

        let dir = unique_temp_dir();
        let outside = unique_temp_dir();

        let external_profile = write_file(
            &outside,
            "External.xmp",
            &profile_xmp("External", None, "UUID-E", "True"),
        );
        write_file(
            &outside,
            "nested/Hidden.xmp",
            &profile_xmp("Hidden", None, "UUID-H", "True"),
        );

        symlink(&external_profile, dir.join("linked.xmp")).expect("symlink to profile");
        symlink(outside.join("nested"), dir.join("linked-dir")).expect("symlink to directory");

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan should succeed");
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();

        assert_eq!(
            names,
            ["External"],
            "the symlinked profile is discoverable, the symlinked directory is not followed"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
        fs::remove_dir_all(&outside).expect("remove temp dir");
    }

    #[test]
    fn missing_root_returns_contextual_error() {
        let dir = unique_temp_dir();
        let missing = dir.join("does-not-exist");

        let error = discover_xmp_profiles(&[missing.clone()])
            .expect_err("a missing root must fail the scan");

        assert!(
            error.contains(&missing.to_string_lossy().to_string()),
            "error should name the offending root: {error}"
        );

        let file_root = write_file(&dir, "not-a-dir.xmp", &preset_xmp());
        let error = discover_xmp_profiles(&[file_root.clone()])
            .expect_err("a file passed as root must fail the scan");
        assert!(
            error.contains(&file_root.to_string_lossy().to_string()),
            "error should name the offending root: {error}"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn profile_entry_uses_embedded_name_group_uuid() {
        let dir = unique_temp_dir();
        let path = write_file(
            &dir,
            "file-name-does-not-match.xmp",
            &profile_xmp(
                "Embedded Name",
                Some("Embedded Group"),
                "EMBEDDED-UUID",
                "False",
            ),
        );

        let entry = classify_xmp_profile_path(&path)
            .expect("classification should succeed")
            .expect("a supported profile must classify as Some");

        assert_eq!(entry.name, "Embedded Name");
        assert_eq!(entry.group, Some("Embedded Group".to_string()));
        assert_eq!(entry.uuid, "EMBEDDED-UUID");
        assert!(!entry.supports_amount);
        assert_eq!(entry.path, path.to_string_lossy().to_string());

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn classify_returns_none_for_non_xmp_path() {
        let dir = unique_temp_dir();
        let path = write_file(&dir, "photo.NEF", "raw data");

        assert_eq!(
            classify_xmp_profile_path(&path),
            Ok(None),
            "non-.xmp paths are never candidates"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn classify_returns_none_for_unsupported_xmp_content() {
        let dir = unique_temp_dir();
        let preset = write_file(&dir, "preset.xmp", &preset_xmp());
        let sidecar = write_file(&dir, "sidecar.xmp", &raw_sidecar_xmp());
        let malformed = write_file(&dir, "broken.xmp", "<x:xmpmeta>");

        for path in [preset, sidecar, malformed] {
            assert_eq!(
                classify_xmp_profile_path(&path),
                Ok(None),
                "{} must be treated as unsupported content",
                path.display()
            );
        }

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn classify_reports_unreadable_xmp_as_error() {
        let dir = unique_temp_dir();
        let missing = dir.join("missing.xmp");

        let error = classify_xmp_profile_path(&missing)
            .expect_err("an unreadable file must be distinguishable from unsupported content");

        assert!(
            error.contains(&missing.to_string_lossy().to_string()),
            "error should contain the path: {error}"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    // ------------------------------------------- corpus-derived classification

    /// The four real Adobe application-metadata files in the installed Camera Raw
    /// tree (`Defaults/RawDefaults.xmp`, `Defaults/Preferences.xmp`,
    /// `Defaults/FavoriteStyles.xmp`, `GPU/TimeEstimates.xmp`) all carry an
    /// `rdf:Description` and the `crs:` namespace but no `crs:PresetType`. None may
    /// be surfaced to the frontend, and none may disturb a scan that also finds a
    /// real profile.
    #[test]
    fn real_adobe_app_metadata_shapes_are_not_surfaced_as_profiles() {
        let dir = unique_temp_dir();

        for (name, contents) in adobe_app_metadata_documents() {
            let path = write_file(&dir, name, &contents);

            assert_eq!(
                classify_xmp_profile_path(&path),
                Ok(None),
                "{name} carries app metadata, not a profile; it must classify as unsupported"
            );
        }

        // Non-vacuity + scan safety: alongside the four app-metadata files, a real
        // profile is still discovered, and only it.
        write_file(
            &dir,
            "Real ⛏️.xmp",
            &profile_xmp("Real ⛏️", Some("Group"), "UUID-REAL", "True"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan must succeed");
        assert_eq!(
            entries.len(),
            1,
            "only the real profile may be returned, got {entries:?}"
        );
        assert_eq!(entries[0].name, "Real ⛏️");

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    /// A genuinely non-UTF-8 file, a truncated profile and a malformed document
    /// must each be skipped without aborting the root scan: the valid profile
    /// beside them is still returned. Non-UTF-8 is NOT valid UTF-8 (unlike the
    /// replacement-character fixture used elsewhere), so it exercises the
    /// `read_to_string` failure path.
    #[test]
    fn scan_survives_non_utf8_truncated_and_malformed_neighbours() {
        let dir = unique_temp_dir();

        // Raw bytes that are not a valid UTF-8 sequence at all.
        let invalid = write_bytes(
            &dir,
            "invalid-utf8.xmp",
            &[0xFF, 0xFE, 0x00, 0x3C, 0x78, 0x3A, 0x80, 0x81],
        );

        // Truncated mid-attribute: the quote is never closed. Built from a real
        // profile document so the cut is provably mid-token.
        let full = profile_xmp("Truncated", None, "UUID-T", "True");
        let anchor = full
            .find("crs:PresetType=\"Look\"")
            .expect("canonical fixture has a PresetType");
        let truncated = &full[..anchor + "crs:PresetType=\"Lo".len()];
        assert!(
            parse_xmp_rgb_profile(truncated).is_err(),
            "the truncated fixture must genuinely be unparseable"
        );
        write_file(&dir, "truncated.xmp", truncated);

        write_file(&dir, "malformed.xmp", "<x:xmpmeta><rdf:RDF");
        write_file(&dir, "empty.xmp", "");
        write_file(
            &dir,
            "good.xmp",
            &profile_xmp("Good", None, "UUID-G", "True"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan must survive bad files");

        assert_eq!(
            entries.len(),
            1,
            "the scan must still return the valid profile, got {entries:?}"
        );
        assert_eq!(entries[0].name, "Good");

        // The non-UTF-8 file is reported as *unreadable* (a contextual error),
        // which the scan downgrades to a skip -- it is never silently treated as
        // a supported profile.
        assert!(
            classify_xmp_profile_path(&invalid).is_err(),
            "a non-UTF-8 .xmp must be reported as unreadable, not parsed"
        );
        assert_eq!(
            classify_xmp_profile_path(&dir.join("truncated.xmp")),
            Ok(None),
            "a truncated document is unsupported content, not a profile"
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    /// A well-formed, Adobe-legal profile shape that Once-Lab does not support (a
    /// 1-D RGBTable, which `dng_rgb_table::GetStream` accepts) must not masquerade
    /// as supported: classification returns `None`, and the neighbouring real
    /// profile is still returned.
    #[test]
    fn unsupported_but_well_formed_adobe_profile_is_not_surfaced() {
        let dir = unique_temp_dir();

        let one_d = profile_xmp_with_table(
            "One-D",
            "UUID-1D",
            "True",
            &one_dimensional_rgb_table_payload(),
        );

        // The rejection is specifically about the TABLE SHAPE, not a byte-length
        // accident: the same payload reaches the dimensions check.
        let error = parse_xmp_rgb_profile(&one_d)
            .expect_err("a 1-D RGBTable is outside Once-Lab's supported table shapes");
        assert!(
            error.contains("unsupported embedded RGBTable dimensions 1"),
            "unexpected error: {error}"
        );

        let path = write_file(&dir, "one-d.xmp", &one_d);
        assert_eq!(
            classify_xmp_profile_path(&path),
            Ok(None),
            "an unsupported (1-D) profile must not be surfaced as supported"
        );

        // Non-vacuity: the exact same document becomes a supported profile once
        // its table is a supported 3-D identity table.
        let three_d = profile_xmp_with_table(
            "Three-D",
            "UUID-3D",
            "True",
            &three_dimensional_rgb_table_payload(0, 1, 0),
        );
        let three_d_path = write_file(&dir, "three-d.xmp", &three_d);
        let entry = classify_xmp_profile_path(&three_d_path)
            .expect("classification should succeed")
            .expect("a supported 3-D table must classify as a profile");
        assert_eq!(entry.name, "Three-D");

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    /// Unknown metadata is never guessed into a supported profile: an unknown
    /// `crs:PresetType`, an unknown boolean spelling and an unknown big-table type
    /// all classify as `None` rather than defaulting to Look / False / RGBTable.
    #[test]
    fn unknown_metadata_is_not_guessed_into_a_supported_profile() {
        let dir = unique_temp_dir();

        // Unknown PresetType value.
        let unknown_preset = profile_xmp("UnknownPreset", None, "UUID-P", "True")
            .replace("crs:PresetType=\"Look\"", "crs:PresetType=\"Look2\"");
        // Unknown boolean spelling (Adobe writes only True/False).
        let unknown_bool = profile_xmp("UnknownBool", None, "UUID-B", "True")
            .replace("crs:SupportsAmount=\"True\"", "crs:SupportsAmount=\"yes\"");
        // Unknown big-table type (2 is not btt_RGBTable == 1). Padded past the
        // minimum block size so it is rejected for its TYPE, not its length.
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes()); // unknown type
        body.extend_from_slice(&[0u8; 96]); // pad into the accepted size window
        let unknown_type =
            profile_xmp_with_table("UnknownType", "UUID-T", "True", &encode_big_table(&body));

        for (name, contents) in [
            ("unknown-preset.xmp", unknown_preset),
            ("unknown-bool.xmp", unknown_bool),
            ("unknown-type.xmp", unknown_type),
        ] {
            let path = write_file(&dir, name, &contents);

            assert_eq!(
                classify_xmp_profile_path(&path),
                Ok(None),
                "{name}: unknown metadata must be skipped, never defaulted"
            );
        }

        // Non-vacuity: a well-formed profile in the same directory is returned,
        // so the assertion above is not simply "nothing is ever discovered".
        write_file(
            &dir,
            "good.xmp",
            &profile_xmp("Good", None, "UUID-G", "True"),
        );
        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan must succeed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Good");

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    /// A valid supported profile is returned (and only through the approved
    /// parser): embedded name/group/uuid win over the file name.
    #[test]
    fn valid_supported_profile_is_returned() {
        let dir = unique_temp_dir();
        write_file(
            &dir,
            "not-the-name.xmp",
            &profile_xmp("Authoritative Name", Some("Authoritative Group"), "UUID-V", "False"),
        );

        let entries = discover_xmp_profiles(&[dir.clone()]).expect("scan must succeed");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Authoritative Name");
        assert_eq!(entries[0].group.as_deref(), Some("Authoritative Group"));
        assert_eq!(entries[0].uuid, "UUID-V");
        assert!(!entries[0].supports_amount);

        fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    /// Discovers the real reference profiles. The fixture directory is
    /// UNTRACKED, so a clean checkout has to skip this test instead of failing
    /// the suite (same guard the render acceptance test uses).
    #[test]
    fn real_reference_discovery() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../raw-filewithxmp-profile");

        if !fixture.is_dir() {
            eprintln!("ACCEPTANCE SKIPPED: {} is missing", fixture.display());
            return;
        }

        // The directory is known to exist, so canonicalizing it is safe from
        // here on. Discovery itself never rewrites the roots it is given.
        let fixture = fixture.canonicalize().expect("acceptance directory");
        let entries = discover_xmp_profiles(&[fixture]).expect("reference scan");

        eprintln!("ACCEPTANCE: discovered {} entries", entries.len());
        for entry in &entries {
            eprintln!(
                "  name={:?} group={:?} uuid={} supports_amount={} convert_to_grayscale={} path={}",
                entry.name,
                entry.group,
                entry.uuid,
                entry.supports_amount,
                entry.convert_to_grayscale,
                entry.path
            );
        }

        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"Au ⛏️"), "Au must be discovered: {names:?}");
        assert!(names.contains(&"Cu ⛏️"), "Cu must be discovered: {names:?}");
        // The Fe-class grayscale profile must now be returned like any other.
        assert!(
            names.contains(&"Fe ⛏"),
            "the grayscale Fe profile must be discovered: {names:?}"
        );
        let fe = entries
            .iter()
            .find(|entry| entry.name == "Fe ⛏")
            .expect("Fe entry");
        assert!(
            fe.convert_to_grayscale,
            "Fe must report its monochrome flag through discovery"
        );

        // The RAW file and the unrelated sidecar artifacts in the same folder
        // must never surface as profiles.
        for entry in &entries {
            assert!(
                has_xmp_extension(Path::new(&entry.path)),
                "discovered entry is not an .xmp file: {}",
                entry.path
            );
            assert!(
                !entry.path.ends_with("TLP_8278.NEF") && !entry.path.ends_with(".rrdata"),
                "non-profile artifact was discovered: {}",
                entry.path
            );
        }
    }
}
