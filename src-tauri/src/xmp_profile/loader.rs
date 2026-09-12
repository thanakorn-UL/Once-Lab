//! Filesystem bridge for XMP RGB profile files.
//!
//! Reading and parsing an XMP profile from disk belongs to the `xmp_profile`
//! module; RAW decoding never touches XMP files. The RAW layer receives an
//! already-parsed [`XmpRgbProfile`].

use std::path::Path;

use crate::xmp_profile::XmpRgbProfile;

/// Whether `path` carries an `.xmp` extension, compared case-insensitively.
pub(crate) fn has_xmp_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("xmp"))
}

/// Reads and parses an Adobe XMP RGB profile from `path`.
///
/// The file extension is validated first, so a non-`.xmp` path is rejected
/// without touching the filesystem. The file is read as strict UTF-8 (invalid
/// bytes are an error rather than being replaced), and parsing reuses the
/// existing [`parse_xmp_rgb_profile`](super::parse_xmp_rgb_profile).
pub(crate) fn load_xmp_rgb_profile_from_path(path: &Path) -> Result<XmpRgbProfile, String> {
    if !has_xmp_extension(path) {
        return Err(format!("expected .xmp profile file: {}", path.display()));
    }

    let contents = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read XMP profile '{}': {error}", path.display()))?;

    super::parse_xmp_rgb_profile(&contents)
        .map_err(|error| format!("failed to parse XMP profile '{}': {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Synthetic 2x2x2 identity RGBTable (same fixture pattern as the parser
    /// and rgb_table tests).
    const IDENTITY_2X2X2_BASE85: &str = "71000rtKmwBRLy1{X$/w9'=(bLC9YEvFE(9%a0@fg0";

    fn unique_temp_path(extension: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);

        std::env::temp_dir().join(format!(
            "once_lab_xmp_loader_{}_{}_{}.{}",
            std::process::id(),
            nanos,
            counter,
            extension
        ))
    }

    fn valid_profile_xmp() -> String {
        format!(
            r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:PresetType="Look"
      crs:UUID="TEST123"
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
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#,
            table = IDENTITY_2X2X2_BASE85
        )
    }

    /// Valid UTF-8 XMP that is missing the embedded RGBTable data.
    fn profile_xmp_missing_table() -> String {
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:PresetType="Look"
      crs:UUID="TEST123">
      <crs:Name>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">Test Profile</rdf:li>
        </rdf:Alt>
      </crs:Name>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#
            .to_string()
    }

    #[test]
    fn loads_valid_xmp_profile_from_path() {
        let path = unique_temp_path("xmp");
        fs::write(&path, valid_profile_xmp()).expect("write temp xmp");

        let profile = load_xmp_rgb_profile_from_path(&path).expect("valid profile should load");

        assert_eq!(profile.name, "Test Profile");
        assert_eq!(profile.uuid, "TEST123");
        assert_eq!(profile.rgb_table_amount, Some(0.5));
        assert_eq!(profile.table.size, 2);
        assert_eq!(profile.table.values.len(), 8);

        fs::remove_file(&path).expect("remove temp xmp");
    }

    #[test]
    fn accepts_case_insensitive_xmp_extension() {
        let path = unique_temp_path("XMP");
        fs::write(&path, valid_profile_xmp()).expect("write temp xmp");

        let profile =
            load_xmp_rgb_profile_from_path(&path).expect("uppercase .XMP should be accepted");

        assert_eq!(profile.uuid, "TEST123");

        fs::remove_file(&path).expect("remove temp xmp");
    }

    #[test]
    fn rejects_non_xmp_extension_before_read() {
        // Deliberately nonexistent: extension validation must happen first.
        let path = unique_temp_path("txt");
        let error =
            load_xmp_rgb_profile_from_path(&path).expect_err("non-xmp extension must be rejected");

        assert!(
            error.starts_with("expected .xmp profile file: "),
            "unexpected error: {error}"
        );
        assert!(
            error.contains(&path.display().to_string()),
            "error should contain the path: {error}"
        );
    }

    #[test]
    fn reports_missing_xmp_file() {
        let path = unique_temp_path("xmp");
        let error =
            load_xmp_rgb_profile_from_path(&path).expect_err("missing file must be reported");

        assert!(
            error.starts_with("failed to read XMP profile"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains(&path.display().to_string()),
            "error should contain the path: {error}"
        );
    }

    #[test]
    fn rejects_non_utf8_xmp_file() {
        let path = unique_temp_path("xmp");
        fs::write(&path, [0xFFu8, 0xFE, 0xFD]).expect("write invalid utf-8");

        let error =
            load_xmp_rgb_profile_from_path(&path).expect_err("non-utf8 file must be rejected");

        assert!(
            error.starts_with("failed to read XMP profile"),
            "unexpected error: {error}"
        );

        fs::remove_file(&path).expect("remove temp xmp");
    }

    #[test]
    fn reports_xmp_parse_error_with_path() {
        let path = unique_temp_path("xmp");
        fs::write(&path, profile_xmp_missing_table()).expect("write temp xmp");

        let error =
            load_xmp_rgb_profile_from_path(&path).expect_err("missing RGBTable must be an error");

        assert!(
            error.starts_with("failed to parse XMP profile"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains(&path.display().to_string()),
            "error should contain the path: {error}"
        );
        assert!(
            error.contains("missing crs:RGBTable"),
            "underlying parser error must be preserved: {error}"
        );

        fs::remove_file(&path).expect("remove temp xmp");
    }
}
