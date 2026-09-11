//! UI-facing summary of an XMP RGB profile.
//!
//! The frontend must never parse XMP itself, so the profile name shown in the
//! picker comes from the same approved parser used for rendering.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::xmp_profile::loader::load_xmp_rgb_profile_from_path;
use crate::xmp_profile::XmpRgbProfile;

/// Minimal, serializable description of a profile for the profile picker.
///
/// Deliberately excludes LUT values and any binary table data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct XmpProfileSummary {
    pub name: String,
    pub path: String,
    /// Whether the profile advertises a scalable Amount, i.e. whether the
    /// frontend should offer the control at all.
    pub supports_amount: bool,
}

/// Builds the UI summary from an already-parsed profile.
pub(crate) fn summarize_xmp_profile(profile: &XmpRgbProfile, path: &Path) -> XmpProfileSummary {
    XmpProfileSummary {
        name: profile.name.clone(),
        path: normalize_path_string(path),
        supports_amount: profile.supports_amount,
    }
}

fn normalize_path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// Loads the profile at `path` and returns the UI-facing summary.
///
/// Errors are the contextual loader/parser errors, unchanged.
pub(crate) fn inspect_xmp_profile(path: &str) -> Result<XmpProfileSummary, String> {
    let profile_path = PathBuf::from(path);
    let profile = load_xmp_rgb_profile_from_path(&profile_path)?;
    Ok(summarize_xmp_profile(&profile, &profile_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Synthetic 2x2x2 identity RGBTable (same fixture pattern as the parser,
    /// rgb_table and loader tests).
    const IDENTITY_2X2X2_BASE85: &str = "71000rtKmwBRLy1{X$/w9'=(bLC9YEvFE(9%a0@fg0";

    fn unique_temp_path(extension: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);

        std::env::temp_dir().join(format!(
            "once_lab_xmp_summary_{}_{}_{}.{}",
            std::process::id(),
            nanos,
            counter,
            extension
        ))
    }

    /// Profile whose embedded name differs from its file name, so a summary
    /// derived from the path would be visibly wrong.
    fn named_profile_xmp() -> String {
        named_profile_xmp_with_amount_support("True")
    }

    fn named_profile_xmp_with_amount_support(supports_amount: &str) -> String {
        format!(
            r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
      crs:PresetType="Look"
      crs:UUID="TEST123"
      crs:SupportsAmount="{supports_amount}"
      crs:ConvertToGrayscale="False"
      crs:RGBTable="TESTTABLE"
      crs:RGBTableAmount="0.5"
      crs:Table_TESTTABLE="{table}">
      <crs:Name>
        <rdf:Alt>
          <rdf:li xml:lang="x-default">Embedded Profile Name</rdf:li>
        </rdf:Alt>
      </crs:Name>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#,
            table = IDENTITY_2X2X2_BASE85
        )
    }

    #[test]
    fn profile_summary_exposes_amount_support_from_the_parsed_profile() {
        for (declared, expected) in [("True", true), ("False", false)] {
            let path = unique_temp_path("xmp");
            fs::write(&path, named_profile_xmp_with_amount_support(declared)).expect("write temp xmp");

            let summary = inspect_xmp_profile(&path.to_string_lossy()).expect("inspect");

            assert_eq!(
                summary.supports_amount, expected,
                "SupportsAmount=\"{declared}\" must be exposed verbatim"
            );

            fs::remove_file(&path).expect("remove temp xmp");
        }
    }

    #[test]
    fn profile_summary_uses_parsed_xmp_name() {
        let path = unique_temp_path("xmp");
        fs::write(&path, named_profile_xmp()).expect("write temp xmp");

        let profile = load_xmp_rgb_profile_from_path(&path).expect("profile should load");
        let summary = summarize_xmp_profile(&profile, &path);

        // The name must come from the parsed XMP, not from the file name.
        assert_eq!(summary.name, "Embedded Profile Name");
        assert_ne!(
            summary.name,
            path.file_stem().unwrap().to_string_lossy().to_string()
        );
        assert_eq!(summary.path, path.to_string_lossy().to_string());

        fs::remove_file(&path).expect("remove temp xmp");
    }

    #[test]
    fn inspect_command_returns_parsed_name_and_propagates_errors() {
        let path = unique_temp_path("xmp");
        fs::write(&path, named_profile_xmp()).expect("write temp xmp");

        let summary = inspect_xmp_profile(&path.to_string_lossy()).expect("inspect should succeed");
        assert_eq!(summary.name, "Embedded Profile Name");
        assert_eq!(summary.path, path.to_string_lossy().to_string());

        fs::remove_file(&path).expect("remove temp xmp");

        let missing = unique_temp_path("xmp");
        let error = inspect_xmp_profile(&missing.to_string_lossy())
            .expect_err("missing file must be reported");
        assert!(
            error.starts_with("failed to read XMP profile"),
            "unexpected error: {error}"
        );
    }
}
