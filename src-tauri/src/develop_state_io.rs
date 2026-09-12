//! Develop-state IO: resolve and persist an image's saved profile reference.
//!
//! `develop_state.rs` stays pure, so every filesystem access for the develop
//! state — reading the sidecar, probing the profile roots and writing the
//! sidecar back — lives here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::develop_state::{
    DEVELOP_STATE_VERSION, DevelopProfileReference, DevelopState, DiscoveredProfile,
    ProfileResolution, resolve_profile_reference,
};
use crate::xmp_profile::load_xmp_rgb_profile_from_path;
use crate::xmp_profile::summary::{XmpProfileSummary, summarize_xmp_profile};

/// Outcome of restoring an image's saved profile reference.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DevelopProfileRestore {
    /// The image has no saved profile, or its record is from a newer schema
    /// that this build must not interpret.
    None,
    /// The reference resolved against the current profile library.
    Resolved {
        reference: DevelopProfileReference,
        summary: XmpProfileSummary,
    },
    /// The reference could not be resolved. Returned unchanged so it can
    /// resolve later if the profile is installed or restored.
    Missing { reference: DevelopProfileReference },
}

/// Resolves the profile reference saved in `sidecar_path` against `roots`.
///
/// A root that is missing, unreadable or not a directory is skipped: an
/// unavailable saved root must never fail a restore. The saved path itself is
/// probed last, so a profile outside every root still resolves by exact path.
pub(crate) fn resolve_develop_profile_from_sidecar(
    sidecar_path: &Path,
    roots: &[PathBuf],
) -> DevelopProfileRestore {
    let metadata = crate::exif_processing::load_sidecar(sidecar_path);
    let state = metadata.develop_state();

    if !state.is_supported() {
        return DevelopProfileRestore::None;
    }

    let Some(reference) = state.profile else {
        return DevelopProfileRestore::None;
    };

    let candidates = collect_profile_candidates(roots, &reference);

    match resolve_profile_reference(&reference, &candidates) {
        ProfileResolution::Resolved { profile, .. } => {
            let profile_path = Path::new(&profile.path);

            match load_xmp_rgb_profile_from_path(profile_path) {
                Ok(loaded) => DevelopProfileRestore::Resolved {
                    reference,
                    summary: summarize_xmp_profile(&loaded, profile_path),
                },
                Err(error) => {
                    log::warn!(
                        "Saved XMP profile became unreadable '{}': {error}",
                        profile_path.display()
                    );
                    DevelopProfileRestore::Missing { reference }
                }
            }
        }
        // Only the profile identified by the saved uuid/path resolves; a
        // different profile sitting at the saved path is never substituted.
        ProfileResolution::Missing { reference } => DevelopProfileRestore::Missing { reference },
    }
}

/// Every profile the resolution may match, in discovery order.
fn collect_profile_candidates(
    roots: &[PathBuf],
    reference: &DevelopProfileReference,
) -> Vec<DiscoveredProfile> {
    let mut candidates: Vec<DiscoveredProfile> = Vec::new();

    for root in roots {
        if !root.is_dir() {
            continue;
        }

        match crate::xmp_profile::discovery::discover_xmp_profiles(std::slice::from_ref(root)) {
            Ok(entries) => {
                for entry in entries {
                    // Overlapping roots can reach the same file twice, so a path
                    // already collected is skipped: first occurrence wins, which
                    // keeps resolution deterministic.
                    if candidates
                        .iter()
                        .any(|candidate| candidate.path == entry.path)
                    {
                        continue;
                    }

                    candidates.push(DiscoveredProfile {
                        uuid: entry.uuid,
                        path: entry.path,
                        name: entry.name,
                    });
                }
            }
            Err(error) => log::warn!(
                "Skipping unusable XMP profile root '{}': {error}",
                root.display()
            ),
        }
    }

    if let Some(saved_path) = reference.path.as_deref().filter(|value| !value.is_empty()) {
        let path = Path::new(saved_path);

        if path.is_file()
            && !candidates
                .iter()
                .any(|candidate| candidate.path == saved_path)
            && let Ok(profile) = load_xmp_rgb_profile_from_path(path)
        {
            candidates.push(DiscoveredProfile {
                uuid: profile.uuid,
                path: saved_path.to_string(),
                name: profile.name,
            });
        }
    }

    candidates
}

/// The roots a restore scans: the detected Adobe roots first, then the user's
/// saved roots, deduplicated by exact path string.
pub(crate) fn develop_profile_roots(home: Option<&Path>, user_roots: &[String]) -> Vec<PathBuf> {
    let detected = home
        .map(crate::xmp_profile::default_roots::existing_default_xmp_profile_roots)
        .unwrap_or_default();

    let mut seen: HashSet<String> = HashSet::new();
    let mut roots: Vec<PathBuf> = Vec::new();

    for root in detected
        .into_iter()
        .map(|descriptor| descriptor.path)
        .chain(user_roots.iter().cloned())
    {
        if seen.insert(root.clone()) {
            roots.push(PathBuf::from(root));
        }
    }

    roots
}

/// Persists `profile` as the image's develop state, keeping every other field of
/// the sidecar byte-for-byte.
///
/// A record written by a newer build is never overwritten: it is left exactly
/// as it is and reported as an error.
pub(crate) fn write_develop_profile_to_sidecar(
    sidecar_path: &Path,
    profile: Option<DevelopProfileReference>,
) -> Result<(), String> {
    let mut metadata = crate::exif_processing::load_sidecar(sidecar_path);
    let existing = metadata.develop_state();

    if metadata.develop_state.is_some() && !existing.is_supported() {
        return Err(format!(
            "refusing to overwrite a develop state written by a newer Once-Lab version ({})",
            existing.version
        ));
    }

    let saved_profile = profile.is_some();

    metadata.set_develop_state(DevelopState {
        version: DEVELOP_STATE_VERSION,
        profile,
        unknown: existing.unknown,
    });

    let json_string = serde_json::to_string_pretty(&metadata).map_err(|e| e.to_string())?;
    std::fs::write(sidecar_path, json_string).map_err(|e| e.to_string())?;

    log::info!(
        "Develop profile {} for sidecar '{}'",
        if saved_profile { "saved" } else { "cleared" },
        sidecar_path.display()
    );

    Ok(())
}

/// Restores the profile the image was last developed with.
///
/// The candidate list is built from the user's saved roots plus the detected
/// Adobe roots, and the resolution itself stays with the pure identity model.
#[tauri::command]
pub fn resolve_image_develop_profile(
    path: String,
    app_handle: AppHandle,
) -> Result<DevelopProfileRestore, String> {
    let (_, sidecar_path) = crate::file_management::parse_virtual_path(&path);
    let settings = crate::app_settings::load_settings(app_handle.clone())?;

    let home = app_handle.path().home_dir().ok();
    let roots = develop_profile_roots(home.as_deref(), &settings.xmp_profile_roots);

    Ok(resolve_develop_profile_from_sidecar(&sidecar_path, &roots))
}

/// Saves (or clears, with `profile: None`) the profile the image is developed
/// with. The profile reference is retained verbatim, including when the profile
/// is currently unavailable.
#[tauri::command]
pub fn save_image_develop_profile(
    path: String,
    profile: Option<DevelopProfileReference>,
) -> Result<(), String> {
    let (_, sidecar_path) = crate::file_management::parse_virtual_path(&path);

    write_develop_profile_to_sidecar(&sidecar_path, profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::{Value, json};

    /// Synthetic 2x2x2 identity RGBTable (same fixture pattern as the parser,
    /// rgb_table, loader, summary and discovery tests).
    const IDENTITY_2X2X2_BASE85: &str = "71000rtKmwBRLy1{X$/w9'=(bLC9YEvFE(9%a0@fg0";

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);

        let dir = std::env::temp_dir().join(format!(
            "once_lab_develop_state_io_{}_{}_{}_{}",
            prefix,
            std::process::id(),
            nanos,
            counter
        ));

        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A supported color profile document whose embedded identity is independent
    /// of the file name.
    fn profile_xmp(name: &str, uuid: &str, supports_amount: &str) -> String {
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
      </crs:Name>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>"#,
            table = IDENTITY_2X2X2_BASE85
        )
    }

    fn write_profile(dir: &Path, file_name: &str, name: &str, uuid: &str) -> PathBuf {
        let path = dir.join(file_name);
        fs::create_dir_all(dir).expect("create profile dir");
        fs::write(&path, profile_xmp(name, uuid, "True")).expect("write profile");
        path
    }

    fn write_sidecar(dir: &Path, file_name: &str, document: &Value) -> PathBuf {
        let path = dir.join(file_name);
        fs::create_dir_all(dir).expect("create sidecar dir");
        fs::write(
            &path,
            serde_json::to_string_pretty(document).expect("serialize sidecar"),
        )
        .expect("write sidecar");
        path
    }

    fn read_sidecar(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).expect("read sidecar"))
            .expect("parse sidecar")
    }

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

    fn sidecar_with_profile(profile: Value) -> Value {
        json!({
            "version": 1,
            "rating": 0,
            "adjustments": null,
            "develop_state": { "version": 1, "profile": profile },
        })
    }

    /// A legacy sidecar: every key a pre-develop-state build wrote, and no
    /// `develop_state`.
    fn legacy_document() -> Value {
        json!({
            "version": 1,
            "rating": 4,
            "adjustments": { "exposure": 0.5 },
            "tags": ["keep"],
            "exif": { "Make": "Once" },
        })
    }

    #[test]
    fn no_saved_profile_resolves_to_none() {
        let dir = unique_temp_dir("none");
        let roots = unique_temp_dir("none_roots");

        // No sidecar at all.
        let absent = dir.join("absent.NEF.rrdata");
        assert_eq!(
            resolve_develop_profile_from_sidecar(&absent, std::slice::from_ref(&roots)),
            DevelopProfileRestore::None
        );

        // A legacy sidecar: parsed, but with no develop state record.
        let legacy = write_sidecar(&dir, "legacy.NEF.rrdata", &legacy_document());
        assert!(
            crate::exif_processing::load_sidecar(&legacy)
                .develop_state
                .is_none()
        );
        assert_eq!(
            resolve_develop_profile_from_sidecar(&legacy, std::slice::from_ref(&roots)),
            DevelopProfileRestore::None
        );

        // A develop state record that carries no profile: a cleared image.
        let cleared = write_sidecar(
            &dir,
            "cleared.NEF.rrdata",
            &json!({ "version": 1, "rating": 0, "adjustments": null, "develop_state": { "version": 1 } }),
        );
        assert_eq!(
            resolve_develop_profile_from_sidecar(&cleared, std::slice::from_ref(&roots)),
            DevelopProfileRestore::None
        );

        fs::remove_dir_all(&dir).expect("remove temp dir");
        fs::remove_dir_all(&roots).expect("remove temp dir");
    }

    #[test]
    fn resolved_profile_is_returned_with_its_amount_and_support_flag() {
        let root = unique_temp_dir("resolved_root");
        let profile_path = write_profile(&root, "Alpha.xmp", "Alpha Profile", "UUID-ALPHA");
        let profile_path_string = profile_path.to_string_lossy().to_string();

        let saved = reference(
            Some("UUID-ALPHA"),
            Some(&profile_path_string),
            Some("Alpha Profile"),
            50.0,
        );

        let document = sidecar_with_profile(serde_json::to_value(&saved).unwrap());

        let sidecar = write_sidecar(&root, "photo.NEF.rrdata", &document);

        let restore = resolve_develop_profile_from_sidecar(&sidecar, std::slice::from_ref(&root));

        let DevelopProfileRestore::Resolved { reference, summary } = restore else {
            panic!("the saved profile is installed in the scanned root: {restore:?}");
        };

        assert_eq!(summary.name, "Alpha Profile");
        assert_eq!(summary.uuid, "UUID-ALPHA");
        assert_eq!(summary.path, profile_path_string);
        assert!(summary.supports_amount);
        assert_eq!(reference, saved);
        assert_eq!(reference.amount_percent, 50.0);

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn saved_path_outside_the_roots_still_resolves() {
        let root = unique_temp_dir("outside_root");
        // A root with a real scan, so the probe is what has to resolve here.
        write_profile(&root, "Other.xmp", "Other Profile", "UUID-OTHER");

        let outside = unique_temp_dir("outside_root_external");
        let outside_profile = write_profile(&outside, "Detached.xmp", "Detached", "UUID-DETACHED");
        let outside_string = outside_profile.to_string_lossy().to_string();

        let sidecar = write_sidecar(
            &root,
            "photo.NEF.rrdata",
            &sidecar_with_profile(
                json!({ "uuid": "UUID-DETACHED", "path": outside_string, "amount_percent": 100.0 }),
            ),
        );

        let restore = resolve_develop_profile_from_sidecar(&sidecar, std::slice::from_ref(&root));

        let DevelopProfileRestore::Resolved { summary, .. } = restore else {
            panic!("the profile at the saved path must resolve: {restore:?}");
        };
        assert_eq!(summary.uuid, "UUID-DETACHED");
        assert_eq!(summary.path, outside_string);

        fs::remove_dir_all(&root).expect("remove temp dir");
        fs::remove_dir_all(&outside).expect("remove temp dir");
    }

    #[test]
    fn profile_missing_from_an_existing_root_is_missing_and_retains_the_reference() {
        let root = unique_temp_dir("missing_root");
        write_profile(&root, "Present.xmp", "Present", "UUID-PRESENT");

        let saved = reference(
            Some("UUID-GONE"),
            Some("/profiles/gone.xmp"),
            Some("Gone"),
            75.0,
        );

        let sidecar = write_sidecar(
            &root,
            "photo.NEF.rrdata",
            &sidecar_with_profile(serde_json::to_value(&saved).unwrap()),
        );

        let restore = resolve_develop_profile_from_sidecar(&sidecar, std::slice::from_ref(&root));

        assert_eq!(
            restore,
            DevelopProfileRestore::Missing {
                reference: saved.clone(),
            },
            "a missing profile must not be substituted by the root's other profile"
        );

        let DevelopProfileRestore::Missing { reference } = restore else {
            unreachable!("asserted above");
        };
        assert_eq!(
            reference, saved,
            "the saved reference is returned unchanged"
        );
        assert_eq!(reference.amount_percent, 75.0);

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn an_unavailable_root_does_not_fail_the_resolve() {
        let root = unique_temp_dir("available_root");
        let profile_path = write_profile(&root, "Alpha.xmp", "Alpha Profile", "UUID-ALPHA");
        let profile_path_string = profile_path.to_string_lossy().to_string();

        let unavailable = root.join("not-a-directory");
        assert!(
            !unavailable.exists(),
            "the fixture root must be unavailable"
        );
        assert!(
            crate::xmp_profile::discovery::discover_xmp_profiles(std::slice::from_ref(
                &unavailable
            ))
            .is_err(),
            "the fixture root must be one discovery genuinely refuses"
        );

        // The first unavailable case: a root that does not exist. This half
        // validates the `is_dir()` skip, NOT the discovery error path: the root is
        // dropped before discovery is ever called, so it would pass even if the
        // `Err` arm did nothing. The reference carries no path, so the good root's
        // scan is the only thing that can resolve it: a candidate list that gave up
        // on the first unusable root could not fall back to anything.
        let sidecar = write_sidecar(
            &root,
            "photo.NEF.rrdata",
            &sidecar_with_profile(json!({ "uuid": "UUID-ALPHA", "amount_percent": 100.0 })),
        );

        // The unavailable root comes FIRST, so a scan that gives up on the
        // first bad root never reaches the profile at all.
        let roots = vec![unavailable, root.clone()];
        let restore = resolve_develop_profile_from_sidecar(&sidecar, &roots);

        let DevelopProfileRestore::Resolved { summary, .. } = restore else {
            panic!("an unavailable root must be skipped, not fatal: {restore:?}");
        };
        assert_eq!(summary.uuid, "UUID-ALPHA");
        assert_eq!(summary.path, profile_path_string);

        // The other unavailable case, and the half that actually exercises the
        // per-root discovery `Err` arm: a root that EXISTS (so the `is_dir()` guard
        // passes) but cannot be read. Discovery fails for it and the failure is
        // ignored per root too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let unreadable = root.join("unreadable");
            fs::create_dir_all(&unreadable).expect("create unreadable root");
            let unreadable_string = unreadable.to_string_lossy().to_string();
            fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
                .expect("make the unreadable root unreadable");

            // A root user bypasses the permission check, so the precondition can
            // legitimately not hold: skip instead of failing the suite.
            if crate::xmp_profile::discovery::discover_xmp_profiles(std::slice::from_ref(
                &unreadable,
            ))
            .is_ok()
            {
                eprintln!(
                    "SKIPPED: '{}' stayed readable (running as root?); the per-root discovery error path cannot be exercised",
                    unreadable_string
                );
                fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700))
                    .expect("restore permissions");
                fs::remove_dir_all(&root).expect("remove temp dir");
                return;
            }

            let restore =
                resolve_develop_profile_from_sidecar(&sidecar, &[unreadable.clone(), root.clone()]);
            assert!(
                matches!(restore, DevelopProfileRestore::Resolved { .. }),
                "an unreadable root must be skipped, not fatal ('{unreadable_string}'): {restore:?}"
            );

            fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700))
                .expect("restore permissions");
        }

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn a_newer_develop_state_version_is_never_applied() {
        let root = unique_temp_dir("newer_root");
        let profile_path = write_profile(&root, "Alpha.xmp", "Alpha Profile", "UUID-ALPHA");
        let profile_path_string = profile_path.to_string_lossy().to_string();
        let profile =
            json!({ "uuid": "UUID-ALPHA", "path": profile_path_string, "amount_percent": 100.0 });

        let newer = write_sidecar(
            &root,
            "newer.NEF.rrdata",
            &json!({
                "version": 1,
                "rating": 0,
                "adjustments": null,
                "develop_state": { "version": 2, "profile": profile },
            }),
        );

        assert_eq!(
            resolve_develop_profile_from_sidecar(&newer, std::slice::from_ref(&root)),
            DevelopProfileRestore::None,
            "a record this build must not interpret is never applied"
        );

        // Non-vacuity: the identical save at version 1 does resolve, so the
        // `None` above is the version check and not a broken fixture.
        let current = write_sidecar(&root, "current.NEF.rrdata", &sidecar_with_profile(profile));
        assert!(
            matches!(
                resolve_develop_profile_from_sidecar(&current, std::slice::from_ref(&root)),
                DevelopProfileRestore::Resolved { .. }
            ),
            "the same profile at the current version must resolve"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn never_substitutes_a_different_profile_at_the_saved_path() {
        let root = unique_temp_dir("substitute_root");
        let in_root = write_profile(&root, "InRoot.xmp", "In Root", "UUID-SAVED");
        let in_root_string = in_root.to_string_lossy().to_string();

        let elsewhere = unique_temp_dir("substitute_elsewhere");
        let impostor = write_profile(&elsewhere, "AtSavedPath.xmp", "Impostor", "UUID-IMPOSTOR");
        let impostor_string = impostor.to_string_lossy().to_string();

        // The saved uuid lives in a root; the saved PATH holds a different uuid.
        let sidecar = write_sidecar(
            &root,
            "photo.NEF.rrdata",
            &sidecar_with_profile(
                json!({ "uuid": "UUID-SAVED", "path": impostor_string, "amount_percent": 100.0 }),
            ),
        );

        let restore = resolve_develop_profile_from_sidecar(&sidecar, std::slice::from_ref(&root));

        let DevelopProfileRestore::Resolved { summary, .. } = restore else {
            panic!("the saved uuid must resolve: {restore:?}");
        };
        assert_eq!(
            summary.uuid, "UUID-SAVED",
            "a different profile sitting at the saved path must never be substituted"
        );
        assert_eq!(summary.path, in_root_string);

        // And with no uuid match, the impostor at the saved path is NOT used.
        let unmatched = write_sidecar(
            &root,
            "unmatched.NEF.rrdata",
            &sidecar_with_profile(
                json!({ "uuid": "UUID-ABSENT", "path": impostor_string, "amount_percent": 100.0 }),
            ),
        );

        let restore = resolve_develop_profile_from_sidecar(&unmatched, std::slice::from_ref(&root));

        assert!(
            matches!(restore, DevelopProfileRestore::Missing { .. }),
            "an unmatched uuid at the saved path is Missing, not the impostor: {restore:?}"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
        fs::remove_dir_all(&elsewhere).expect("remove temp dir");
    }

    #[test]
    fn write_then_read_round_trips_the_profile() {
        let root = unique_temp_dir("roundtrip_root");
        let profile_path = write_profile(&root, "Alpha.xmp", "Alpha Profile", "UUID-ALPHA");
        let profile_path_string = profile_path.to_string_lossy().to_string();

        let sidecar = root.join("photo.NEF.rrdata");
        let written = reference(
            Some("UUID-ALPHA"),
            Some(&profile_path_string),
            Some("Alpha Profile"),
            137.5,
        );

        write_develop_profile_to_sidecar(&sidecar, Some(written.clone()))
            .expect("write develop state");

        let stored = read_sidecar(&sidecar)["develop_state"]["profile"].clone();
        assert_eq!(stored["uuid"], json!("UUID-ALPHA"));
        assert_eq!(stored["path"], json!(profile_path_string));
        assert_eq!(stored["name"], json!("Alpha Profile"));
        assert_eq!(stored["amount_percent"], json!(137.5));

        let metadata: crate::image_processing::ImageMetadata =
            crate::exif_processing::load_sidecar(&sidecar);
        assert_eq!(metadata.develop_state().profile, Some(written.clone()));

        let restore = resolve_develop_profile_from_sidecar(&sidecar, std::slice::from_ref(&root));

        let DevelopProfileRestore::Resolved { reference, summary } = restore else {
            panic!("the written profile must resolve: {restore:?}");
        };
        assert_eq!(reference, written, "the reference survives the round trip");
        assert_eq!(summary.name, "Alpha Profile");
        assert_eq!(summary.uuid, "UUID-ALPHA");
        assert_eq!(summary.path, profile_path_string);

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn writing_no_profile_clears_it() {
        let root = unique_temp_dir("clear_root");
        let profile_path = write_profile(&root, "Alpha.xmp", "Alpha Profile", "UUID-ALPHA");
        let profile_path_string = profile_path.to_string_lossy().to_string();

        let sidecar = root.join("photo.NEF.rrdata");
        write_develop_profile_to_sidecar(
            &sidecar,
            Some(reference(
                Some("UUID-ALPHA"),
                Some(&profile_path_string),
                Some("Alpha Profile"),
                100.0,
            )),
        )
        .expect("write develop state");

        assert!(read_sidecar(&sidecar)["develop_state"]["profile"].is_object());

        write_develop_profile_to_sidecar(&sidecar, None).expect("clear develop state");

        let stored = read_sidecar(&sidecar);
        assert!(
            stored["develop_state"].get("profile").is_none(),
            "a cleared profile is omitted entirely, never a stray value: {stored}"
        );
        assert!(
            crate::exif_processing::load_sidecar(&sidecar)
                .develop_state()
                .profile
                .is_none()
        );
        assert_eq!(
            resolve_develop_profile_from_sidecar(&sidecar, std::slice::from_ref(&root)),
            DevelopProfileRestore::None
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn unknown_keys_of_a_supported_record_are_preserved_across_a_write() {
        let root = unique_temp_dir("unknown_root");
        let profile_path = write_profile(&root, "Alpha.xmp", "Alpha Profile", "UUID-ALPHA");
        let profile_path_string = profile_path.to_string_lossy().to_string();

        let sidecar = write_sidecar(
            &root,
            "photo.NEF.rrdata",
            &json!({
                "version": 1,
                "rating": 0,
                "adjustments": null,
                "develop_state": { "version": 1, "futureField": 7 },
            }),
        );

        // Precondition: the hand-written record parses as a supported one with
        // the unknown key parked.
        let parked = crate::exif_processing::load_sidecar(&sidecar).develop_state();
        assert!(parked.is_supported());
        assert_eq!(parked.unknown.get("futureField"), Some(&json!(7)));

        write_develop_profile_to_sidecar(
            &sidecar,
            Some(reference(
                Some("UUID-ALPHA"),
                Some(&profile_path_string),
                Some("Alpha Profile"),
                100.0,
            )),
        )
        .expect("write develop state");

        let stored = read_sidecar(&sidecar);
        assert_eq!(
            stored["develop_state"]["futureField"],
            json!(7),
            "an unknown key of a supported record survives the write: {stored}"
        );
        assert!(stored["develop_state"]["profile"].is_object());
        assert_eq!(stored["develop_state"]["version"], json!(1));

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn writing_refuses_to_overwrite_a_newer_develop_state() {
        let root = unique_temp_dir("refuse_root");
        let sidecar = write_sidecar(
            &root,
            "photo.NEF.rrdata",
            &json!({
                "version": 1,
                "rating": 0,
                "adjustments": null,
                "develop_state": {
                    "version": 2,
                    "profile": { "uuid": "UUID-FUTURE", "amount_percent": 100.0 },
                },
            }),
        );

        let before = fs::read(&sidecar).expect("read sidecar");

        let error = write_develop_profile_to_sidecar(
            &sidecar,
            Some(reference(Some("UUID-ALPHA"), None, None, 100.0)),
        )
        .expect_err("a newer develop state must never be overwritten");

        assert!(
            error.contains('2') && error.contains("newer"),
            "the error must name the version: {error}"
        );
        assert_eq!(
            fs::read(&sidecar).expect("re-read sidecar"),
            before,
            "the refusal must leave the file byte-unchanged"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn writing_touches_only_develop_state() {
        let root = unique_temp_dir("only_develop_root");
        let profile_path = write_profile(&root, "Alpha.xmp", "Alpha Profile", "UUID-ALPHA");
        let profile_path_string = profile_path.to_string_lossy().to_string();

        let sidecar = write_sidecar(&root, "photo.NEF.rrdata", &legacy_document());

        let before = read_sidecar(&sidecar);
        assert!(before.get("develop_state").is_none());

        write_develop_profile_to_sidecar(
            &sidecar,
            Some(reference(
                Some("UUID-ALPHA"),
                Some(&profile_path_string),
                Some("Alpha Profile"),
                100.0,
            )),
        )
        .expect("write develop state");

        let mut after = read_sidecar(&sidecar);
        after
            .as_object_mut()
            .expect("sidecar is an object")
            .remove("develop_state");

        assert_eq!(
            after, before,
            "rating, tags, exif and adjustments must be untouched"
        );
        assert_eq!(after["rating"], json!(4));
        assert_eq!(after["adjustments"], json!({ "exposure": 0.5 }));
        assert_eq!(after["tags"], json!(["keep"]));
        assert_eq!(after["exif"], json!({ "Make": "Once" }));

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn writing_preserves_legacy_sidecars() {
        let root = unique_temp_dir("legacy_root");
        let profile_path = write_profile(&root, "Alpha.xmp", "Alpha Profile", "UUID-ALPHA");
        let profile_path_string = profile_path.to_string_lossy().to_string();

        let sidecar = write_sidecar(&root, "photo.NEF.rrdata", &legacy_document());

        write_develop_profile_to_sidecar(
            &sidecar,
            Some(reference(
                Some("UUID-ALPHA"),
                Some(&profile_path_string),
                Some("Alpha Profile"),
                100.0,
            )),
        )
        .expect("write develop state");

        let stored = read_sidecar(&sidecar);
        let mut keys: Vec<&str> = stored
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "adjustments",
                "develop_state",
                "exif",
                "rating",
                "tags",
                "version"
            ],
            "a legacy document gains ONLY develop_state: {stored}"
        );
        assert_eq!(
            stored["develop_state"],
            json!({
                "version": 1,
                "profile": {
                    "uuid": "UUID-ALPHA",
                    "path": profile_path_string,
                    "name": "Alpha Profile",
                    "amount_percent": 100.0,
                },
            }),
            "the added key holds exactly the develop record"
        );

        // Clearing the same legacy document still adds the key, with the
        // profile omitted rather than written as null.
        write_develop_profile_to_sidecar(&sidecar, None).expect("clear develop state");

        let cleared = read_sidecar(&sidecar);
        assert_eq!(
            cleared["develop_state"],
            json!({ "version": 1 }),
            "a cleared legacy document gains an empty develop record: {cleared}"
        );
        assert_eq!(cleared["rating"], json!(4));
        assert_eq!(cleared["adjustments"], json!({ "exposure": 0.5 }));

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    /// The Adobe roots come first, then the user's roots, deduplicated by exact
    /// path string.
    #[test]
    fn develop_profile_roots_are_adobe_first_then_deduped_user_roots() {
        let unrelated = unique_temp_dir("roots_unrelated");

        // Assertion 1 (macOS only): the DETECTED Adobe root comes FIRST even though the
        // Adobe string is passed as a user root placed AFTER the unrelated one, and it
        // appears only ONCE (deduped against that same user root).
        // On a non-macOS host `default_root_paths` returns nothing, so there is no
        // detected root to order ahead of the user roots.
        #[cfg(target_os = "macos")]
        {
            let home = unique_temp_dir("roots_home");
            let adobe = home.join("Library/Application Support/Adobe/CameraRaw/Settings");
            fs::create_dir_all(&adobe).expect("create adobe root");

            let adobe_string = adobe.to_string_lossy().to_string();
            let unrelated_string = unrelated.to_string_lossy().to_string();

            // The unrelated user root comes FIRST, so a user-roots-first
            // implementation would yield [unrelated, adobe] and fail below.
            let roots = develop_profile_roots(
                Some(&home),
                &[unrelated_string.clone(), adobe_string.clone()],
            );

            assert_eq!(
                roots,
                vec![
                    PathBuf::from(&adobe_string),
                    PathBuf::from(&unrelated_string)
                ],
                "the detected Adobe root comes first and is deduped against the same user root"
            );

            fs::remove_dir_all(&home).expect("remove temp dir");
        }

        // Assertion 2 (every platform): with no detected roots, the user roots keep
        // their order with exact-string duplicates removed.
        let p = unique_temp_dir("roots_p");
        let p_string = p.to_string_lossy().to_string();
        let unrelated_string = unrelated.to_string_lossy().to_string();

        let roots = develop_profile_roots(
            None,
            &[p_string.clone(), p_string.clone(), unrelated_string.clone()],
        );

        assert_eq!(
            roots,
            vec![PathBuf::from(&p_string), PathBuf::from(&unrelated_string)],
            "duplicate user roots collapse to one, order preserved"
        );

        fs::remove_dir_all(&p).expect("remove temp dir");
        fs::remove_dir_all(&unrelated).expect("remove temp dir");
    }

    /// Overlapping roots reach the same file through two separate scans, so the
    /// cross-root dedup is what keeps the candidate list single.
    #[test]
    fn overlapping_roots_do_not_duplicate_candidates() {
        // `collect_profile_candidates` stays private on purpose: this test is a child
        // module, so it reaches the private helper directly and the public surface is
        // not widened.
        let parent = unique_temp_dir("overlap_parent");
        let child = parent.join("nested");
        let profile_path = write_profile(&child, "Overlap.xmp", "Overlap", "UUID-OVERLAP");
        let profile_path_string = profile_path.to_string_lossy().to_string();

        // `parent` scans recursively into `child`, and `child` is scanned again on
        // its own: both reach the one profile file.
        let roots = vec![parent.clone(), child.clone()];

        let saved = reference(Some("UUID-OVERLAP"), None, Some("Overlap"), 100.0);

        let candidates = collect_profile_candidates(&roots, &saved);
        assert_eq!(
            candidates.len(),
            1,
            "the overlapping roots must not append the same file twice: {candidates:?}"
        );
        assert_eq!(candidates[0].path, profile_path_string);
        assert_eq!(candidates[0].uuid, "UUID-OVERLAP");

        let sidecar = write_sidecar(
            &parent,
            "photo.NEF.rrdata",
            &sidecar_with_profile(serde_json::to_value(&saved).unwrap()),
        );

        let restore = resolve_develop_profile_from_sidecar(&sidecar, &roots);
        let DevelopProfileRestore::Resolved { summary, .. } = restore else {
            panic!("the saved uuid must still resolve through the overlapping roots: {restore:?}");
        };
        assert_eq!(summary.uuid, "UUID-OVERLAP");
        assert_eq!(summary.path, profile_path_string);

        fs::remove_dir_all(&parent).expect("remove temp dir");
    }
}
