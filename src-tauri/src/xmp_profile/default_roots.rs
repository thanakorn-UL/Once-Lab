//! Read-only detection of Adobe Camera Raw profile folders.
//!
//! Detection only ever reports directories that already exist: it never
//! creates, locks or modifies anything, and it deliberately does not look for
//! profiles. Scanning stays with the approved 4A entry point, so a detected
//! root is just another root handed to `discover_xmp_profiles`.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::xmp_profile::summary::normalize_path_string;

/// Source label of a root Once-Lab detects instead of the user adding it.
pub(crate) const ADOBE_ROOT_SOURCE: &str = "adobe";

/// Frontend-facing description of a profile root Once-Lab knows about without
/// the user having added it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct XmpProfileRootDescriptor {
    pub path: String,
    pub source: String,
}

/// The Adobe Camera Raw profile folder macOS uses, derived from `home` alone.
///
/// Pure path construction: no filesystem access and no hardcoded user name.
/// Milestone 3D confirmed Lightroom reads profiles from this exact folder.
#[cfg(target_os = "macos")]
pub(crate) fn default_root_paths(home: &Path) -> Vec<PathBuf> {
    vec![home.join("Library/Application Support/Adobe/CameraRaw/Settings")]
}

/// No other platform has a verified Adobe profile root: Linux has none, and the
/// Windows location is deliberately left unimplemented rather than guessed. The
/// command still works there and simply reports no default root.
#[cfg(not(target_os = "macos"))]
pub(crate) fn default_root_paths(_home: &Path) -> Vec<PathBuf> {
    Vec::new()
}

/// Every default root that currently exists as a real directory.
///
/// A candidate that does not exist is the normal case (Adobe software may not
/// be installed) and is dropped silently instead of being created or reported.
pub(crate) fn existing_default_xmp_profile_roots(home: &Path) -> Vec<XmpProfileRootDescriptor> {
    default_root_paths(home)
        .into_iter()
        .filter(|candidate| candidate.is_dir())
        .map(|path| XmpProfileRootDescriptor {
            path: normalize_path_string(&path),
            source: ADOBE_ROOT_SOURCE.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);

        let dir = std::env::temp_dir().join(format!(
            "once_lab_xmp_default_roots_{}_{}_{}",
            std::process::id(),
            nanos,
            counter
        ));

        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Path construction depends on the supplied home directory and nothing
    /// else: no environment lookup, no fixed user name.
    #[cfg(target_os = "macos")]
    #[test]
    fn builds_the_camera_raw_settings_path_from_the_supplied_home() {
        for home in [
            PathBuf::from("/Users/example"),
            PathBuf::from("/Volumes/External"),
        ] {
            assert_eq!(
                default_root_paths(&home),
                vec![home.join("Library/Application Support/Adobe/CameraRaw/Settings")],
                "the candidate must be derived from the given home"
            );
        }
    }

    /// The candidate is never created and only reported once it exists.
    #[cfg(target_os = "macos")]
    #[test]
    fn detects_an_existing_adobe_root_without_touching_it() {
        let home = unique_temp_dir();
        let expected = home.join("Library/Application Support/Adobe/CameraRaw/Settings");

        assert!(existing_default_xmp_profile_roots(&home).is_empty());
        assert_eq!(
            fs::read_dir(&home).expect("read temp home").count(),
            0,
            "detection must not create the candidate directory"
        );

        fs::create_dir_all(&expected).expect("create adobe root");
        let marker = expected.join("Reference.xmp");
        fs::write(&marker, "untouched").expect("write marker");

        let roots = existing_default_xmp_profile_roots(&home);

        assert_eq!(
            roots,
            vec![XmpProfileRootDescriptor {
                path: expected.to_string_lossy().to_string(),
                source: "adobe".to_string(),
            }]
        );
        assert_eq!(
            fs::read_to_string(&marker).expect("read marker"),
            "untouched",
            "detection must not read, rewrite or remove profiles"
        );

        fs::remove_dir_all(&home).expect("remove temp dir");
    }

    /// A missing default root is a normal condition, not an error, and still
    /// creates nothing.
    #[test]
    fn missing_default_root_is_ignored() {
        let home = unique_temp_dir();

        assert!(existing_default_xmp_profile_roots(&home).is_empty());
        assert_eq!(
            fs::read_dir(&home).expect("read temp home").count(),
            0,
            "a missing candidate must stay missing"
        );

        fs::remove_dir_all(&home).expect("remove temp dir");
    }

    /// Platform policy: nothing is guessed where no Adobe root is verified.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn no_candidate_is_guessed_on_this_platform() {
        assert!(default_root_paths(Path::new("/home/example")).is_empty());
    }
}
