//! Develop-state IO: resolve and persist an image's saved profile reference.
//!
//! `develop_state.rs` stays pure, so every filesystem access for the develop
//! state — reading the sidecar, probing the profile roots and writing the
//! sidecar back — lives here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::develop_state::{
    DEVELOP_STATE_VERSION, DevelopProfileReference, DevelopState, DiscoveredProfile,
    ProfileResolution, resolve_profile_reference,
};
use crate::image_processing::ImageMetadata;
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

    resolve_reference_against_roots(&reference, roots)
}

/// Resolves an already-decoded `reference` against `roots`.
///
/// This is the ONE resolver shared by the `.rrdata` restore (6C2) and the XMP
/// sidecar restore (6C3). The candidate list is built from the roots exactly as
/// before, and only the profile identified by `reference`'s uuid/path resolves —
/// a different profile sitting at the saved path is never substituted.
pub(crate) fn resolve_reference_against_roots(
    reference: &DevelopProfileReference,
    roots: &[PathBuf],
) -> DevelopProfileRestore {
    let candidates = collect_profile_candidates(roots, reference);

    match resolve_profile_reference(reference, &candidates) {
        ProfileResolution::Resolved { profile, .. } => {
            let profile_path = Path::new(&profile.path);

            match load_xmp_rgb_profile_from_path(profile_path) {
                Ok(loaded) => DevelopProfileRestore::Resolved {
                    reference: reference.clone(),
                    summary: summarize_xmp_profile(&loaded, profile_path),
                },
                Err(error) => {
                    log::warn!(
                        "Saved XMP profile became unreadable '{}': {error}",
                        profile_path.display()
                    );
                    DevelopProfileRestore::Missing {
                        reference: reference.clone(),
                    }
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

/// Applies `profile` to the develop state of an ALREADY-LOADED sidecar.
///
/// The single place a profile is written, shared by
/// `write_develop_profile_to_sidecar` (the profile-only path) and the combined
/// adjustments+profile batch: a record written by a newer build is refused (the
/// error is returned so the caller can skip that path, leaving its file
/// byte-unchanged), a `None` profile clears the reference, and the record's
/// unknown keys are preserved.
pub(crate) fn apply_profile_to_metadata(
    metadata: &mut ImageMetadata,
    profile: Option<DevelopProfileReference>,
) -> Result<(), String> {
    let existing = metadata.develop_state();

    if metadata.develop_state.is_some() && !existing.is_supported() {
        return Err(format!(
            "refusing to overwrite a develop state written by a newer Once-Lab version ({})",
            existing.version
        ));
    }

    metadata.set_develop_state(DevelopState {
        version: DEVELOP_STATE_VERSION,
        profile,
        unknown: existing.unknown,
    });

    Ok(())
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
    let saved_profile = profile.is_some();

    apply_profile_to_metadata(&mut metadata, profile)?;

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

/// Applies `profile` to the develop state of ONE target.
///
/// The read-modify-write is `write_develop_profile_to_sidecar`, so the version
/// refusal, the byte-preservation of every other sidecar field and the
/// "a cleared profile is omitted" shape stay in a single place. The sidecar is
/// resolved from the target's VIRTUAL path, so a `?vc=<id>` target writes its
/// OWN `<img>.<id>.rrdata` rather than the physical image's state.
pub(crate) fn save_develop_profile_for_path(
    path: &str,
    profile: Option<&DevelopProfileReference>,
) -> Result<(), String> {
    let (_, sidecar_path) = crate::file_management::parse_virtual_path(path);

    write_develop_profile_to_sidecar(&sidecar_path, profile.cloned())
}

/// Applies `profile` to every target in `paths`, in parallel.
///
/// Structurally mirrors `apply_adjustments_to_paths`: each target is an
/// independent read-modify-write, so a target that cannot be written (an
/// unwritable location, or an existing develop state written by a newer build)
/// is logged and skipped rather than failing the batch. The XMP sidecar is
/// deliberately never written here: only the explicit 6C3 commands may do that.
#[tauri::command]
pub fn save_develop_profile_for_paths(
    paths: Vec<String>,
    profile: Option<DevelopProfileReference>,
) -> Result<(), String> {
    let profile = profile.as_ref();

    paths.par_iter().for_each(|path| {
        if let Err(error) = save_develop_profile_for_path(path, profile) {
            log::warn!("Skipping develop profile write for '{path}': {error}");
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_management::{ProfileUpdate, apply_develop_state_batch, deduplicate_paths};
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

    /// The virtual path of `<dir>/<name>.NEF`, the exact shape the frontend
    /// passes to the batch command.
    fn image_path(dir: &Path, name: &str) -> String {
        dir.join(format!("{name}.NEF"))
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn batch_write_applies_the_profile_to_every_target() {
        let root = unique_temp_dir("batch_apply_root");
        let profile_path = write_profile(&root, "Alpha.xmp", "Alpha Profile", "UUID-ALPHA");
        let profile_path_string = profile_path.to_string_lossy().to_string();

        let targets = vec![
            image_path(&root, "a"),
            image_path(&root, "b"),
            image_path(&root, "c"),
        ];
        let saved = reference(
            Some("UUID-ALPHA"),
            Some(&profile_path_string),
            Some("Alpha Profile"),
            42.0,
        );

        save_develop_profile_for_paths(targets.clone(), Some(saved.clone())).expect("batch write");

        for target in &targets {
            let (_, sidecar) = crate::file_management::parse_virtual_path(target);
            let restore =
                resolve_develop_profile_from_sidecar(&sidecar, std::slice::from_ref(&root));
            let DevelopProfileRestore::Resolved { reference, .. } = restore else {
                panic!("every target must resolve the written profile ('{target}'): {restore:?}");
            };
            assert_eq!(reference, saved, "the reference survives for '{target}'");
            assert_eq!(reference.amount_percent, 42.0);
        }

        // Non-vacuity: a path that was never in the batch has no sidecar, so the
        // three above cannot be a coincidence of a directory-wide write.
        assert!(!root.join("d.NEF.rrdata").exists());

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn batch_write_clears_a_target_profile_with_none() {
        let root = unique_temp_dir("batch_clear_root");
        let target = image_path(&root, "photo");
        let (_, sidecar) = crate::file_management::parse_virtual_path(&target);

        write_develop_profile_to_sidecar(
            &sidecar,
            Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
        )
        .expect("seed a profile");
        assert!(read_sidecar(&sidecar)["develop_state"]["profile"].is_object());

        save_develop_profile_for_paths(vec![target], None).expect("batch clear");

        let stored = read_sidecar(&sidecar);
        assert!(
            stored["develop_state"].get("profile").is_none(),
            "a cleared profile is omitted entirely: {stored}"
        );
        assert_eq!(
            resolve_develop_profile_from_sidecar(&sidecar, std::slice::from_ref(&root)),
            DevelopProfileRestore::None
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn batch_write_leaves_the_source_unchanged_when_it_is_not_in_the_paths() {
        let root = unique_temp_dir("batch_source_root");
        let source = image_path(&root, "source");
        let (_, source_sidecar) = crate::file_management::parse_virtual_path(&source);
        let source_reference = reference(Some("UUID-SOURCE"), None, Some("Source"), 30.0);
        write_develop_profile_to_sidecar(&source_sidecar, Some(source_reference.clone()))
            .expect("seed the source");

        let before = fs::read(&source_sidecar).expect("read the source before");

        save_develop_profile_for_paths(
            vec![image_path(&root, "a"), image_path(&root, "b")],
            Some(reference(Some("UUID-NEW"), None, Some("New"), 80.0)),
        )
        .expect("batch write");

        assert_eq!(
            fs::read(&source_sidecar).expect("re-read the source"),
            before,
            "a path outside the batch must be byte-unchanged"
        );
        assert_eq!(
            resolve_develop_profile_from_sidecar(&source_sidecar, std::slice::from_ref(&root)),
            DevelopProfileRestore::Missing {
                reference: source_reference,
            },
            "the source still references its own profile"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn batch_write_reports_ok_when_one_path_is_unwritable() {
        let root = unique_temp_dir("batch_unwritable_root");
        // Precondition: the parent directory genuinely does not exist, so the
        // write for this one path must fail.
        assert!(
            !root.join("missing-dir").exists(),
            "the fixture parent must not exist"
        );
        let unwritable = root
            .join("missing-dir")
            .join("photo.NEF")
            .to_string_lossy()
            .to_string();
        let good = image_path(&root, "good");

        save_develop_profile_for_paths(
            vec![unwritable.clone(), good.clone()],
            Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
        )
        .expect("a per-path failure must never fail the batch");

        let (_, good_sidecar) = crate::file_management::parse_virtual_path(&good);
        assert!(
            good_sidecar.exists(),
            "the writable target must still be written"
        );
        assert_eq!(
            read_sidecar(&good_sidecar)["develop_state"]["profile"]["uuid"],
            json!("UUID-ALPHA")
        );

        let (_, bad_sidecar) = crate::file_management::parse_virtual_path(&unwritable);
        assert!(
            !bad_sidecar.exists(),
            "an unwritable target must not produce a file"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn batch_write_skips_a_target_with_an_unsupported_develop_state_version() {
        let root = unique_temp_dir("batch_newer_root");
        let newer = image_path(&root, "newer");
        let (_, newer_sidecar) = crate::file_management::parse_virtual_path(&newer);
        write_sidecar(
            &root,
            "newer.NEF.rrdata",
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
        let before = fs::read(&newer_sidecar).expect("read the newer sidecar");

        let good = image_path(&root, "good");

        save_develop_profile_for_paths(
            vec![newer.clone(), good.clone()],
            Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
        )
        .expect("batch write");

        assert_eq!(
            fs::read(&newer_sidecar).expect("re-read the newer sidecar"),
            before,
            "a develop state this build must not interpret is left byte-unchanged"
        );

        let (_, good_sidecar) = crate::file_management::parse_virtual_path(&good);
        assert_eq!(
            read_sidecar(&good_sidecar)["develop_state"]["profile"]["uuid"],
            json!("UUID-ALPHA"),
            "the other target still succeeds"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn batch_write_is_virtual_copy_aware() {
        let root = unique_temp_dir("batch_vc_root");
        // The physical image's own develop state.
        let physical_sidecar = write_sidecar(&root, "photo.NEF.rrdata", &legacy_document());
        let physical_before = fs::read(&physical_sidecar).expect("read the physical sidecar");

        let virtual_path = format!("{}?vc=copy-1", image_path(&root, "photo"));
        let saved = reference(Some("UUID-ALPHA"), None, Some("Alpha"), 55.0);

        save_develop_profile_for_paths(vec![virtual_path.clone()], Some(saved))
            .expect("batch write");

        let (_, copy_sidecar) = crate::file_management::parse_virtual_path(&virtual_path);
        assert_eq!(
            copy_sidecar,
            root.join("photo.NEF.copy-1.rrdata"),
            "a virtual copy must write its OWN sidecar"
        );
        let stored = read_sidecar(&copy_sidecar);
        assert_eq!(
            stored["develop_state"]["profile"]["uuid"],
            json!("UUID-ALPHA")
        );
        assert_eq!(
            stored["develop_state"]["profile"]["amount_percent"],
            json!(55.0)
        );

        assert_eq!(
            fs::read(&physical_sidecar).expect("re-read the physical sidecar"),
            physical_before,
            "the physical image's own state must be byte-unchanged"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn batch_write_preserves_every_other_field() {
        let root = unique_temp_dir("batch_preserve_root");
        let target = image_path(&root, "photo");
        let (_, sidecar) = crate::file_management::parse_virtual_path(&target);
        write_sidecar(&root, "photo.NEF.rrdata", &legacy_document());

        let before = read_sidecar(&sidecar);
        assert!(before.get("develop_state").is_none());

        save_develop_profile_for_paths(
            vec![target],
            Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
        )
        .expect("batch write");

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

    /// The combined batch is the paste's SINGLE writer: each target's `.rrdata`
    /// ends up carrying BOTH the merged adjustments and the develop state.
    ///
    /// Non-vacuity: the lost-update shape the two-command paste had — writing the
    /// file after only the adjustments step, before the profile step — would
    /// leave `develop_state` absent here and fail.
    #[test]
    fn combined_batch_applies_adjustments_and_profile_in_one_write() {
        let root = unique_temp_dir("combined_both_root");
        let targets = vec![
            image_path(&root, "a"),
            image_path(&root, "b"),
            image_path(&root, "c"),
        ];
        let delta = json!({ "exposure": 1.5, "contrast": 20 });
        let profile = ProfileUpdate::Set {
            profile: Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 42.0)),
        };

        apply_develop_state_batch(&targets, Some(&delta), &profile, None, false, false);

        for target in &targets {
            let (_, sidecar) = crate::file_management::parse_virtual_path(target);
            let stored = read_sidecar(&sidecar);
            assert_eq!(
                stored["adjustments"]["exposure"],
                json!(1.5),
                "the pasted adjustment key must be written for '{target}': {stored}"
            );
            assert_eq!(
                stored["adjustments"]["contrast"],
                json!(20),
                "every pasted adjustment key must be written for '{target}': {stored}"
            );
            assert_eq!(
                stored["develop_state"]["profile"]["uuid"],
                json!("UUID-ALPHA"),
                "the develop state must survive beside the adjustments for '{target}': {stored}"
            );
            assert_eq!(
                stored["develop_state"]["profile"]["amount_percent"],
                json!(42.0)
            );
        }

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn combined_batch_keeps_the_profile_when_asked_to_keep() {
        let root = unique_temp_dir("combined_keep_root");
        let target = image_path(&root, "photo");
        let (_, sidecar) = crate::file_management::parse_virtual_path(&target);

        write_sidecar(
            &root,
            "photo.NEF.rrdata",
            &json!({
                "version": 1,
                "rating": 0,
                "adjustments": { "exposure": 0.5 },
                "develop_state": {
                    "version": 1,
                    "profile": { "uuid": "UUID-SEED", "name": "Seed", "amount_percent": 77.0 },
                },
            }),
        );

        let before = read_sidecar(&sidecar);
        let delta = json!({ "exposure": 2.0 });

        apply_develop_state_batch(
            &[target],
            Some(&delta),
            &ProfileUpdate::Keep,
            None,
            false,
            false,
        );

        let after = read_sidecar(&sidecar);
        assert_eq!(
            after["develop_state"], before["develop_state"],
            "ProfileUpdate::Keep must leave the develop state untouched: {after}"
        );
        assert_eq!(
            after["develop_state"]["profile"]["uuid"],
            json!("UUID-SEED"),
            "an existing profile survives a Keep"
        );
        assert_eq!(
            after["develop_state"]["profile"]["amount_percent"],
            json!(77.0)
        );
        // Non-vacuity: the same call DID change the adjustments half, so the
        // unchanged develop state is not a no-op batch.
        assert_eq!(after["adjustments"]["exposure"], json!(2.0));

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn combined_batch_keeps_adjustments_when_none() {
        let root = unique_temp_dir("combined_none_adjust_root");
        let target = image_path(&root, "photo");
        let (_, sidecar) = crate::file_management::parse_virtual_path(&target);

        write_sidecar(
            &root,
            "photo.NEF.rrdata",
            &json!({
                "version": 1,
                "rating": 3,
                "adjustments": { "exposure": 0.5, "saturation": -10 },
                "tags": ["keep"],
            }),
        );

        let before = read_sidecar(&sidecar);
        assert!(before.get("develop_state").is_none());

        let profile = ProfileUpdate::Set {
            profile: Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
        };
        apply_develop_state_batch(&[target], None, &profile, None, false, false);

        let after = read_sidecar(&sidecar);
        assert_eq!(
            after["adjustments"], before["adjustments"],
            "with adjustments: None the adjustment payload is unchanged: {after}"
        );
        assert_eq!(after["rating"], json!(3));
        assert_eq!(after["tags"], json!(["keep"]));
        assert_eq!(
            after["develop_state"]["profile"]["uuid"],
            json!("UUID-ALPHA"),
            "only the develop state changed"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn combined_batch_clears_a_profile_with_a_null_profile() {
        let root = unique_temp_dir("combined_clear_root");
        let target = image_path(&root, "photo");
        let (_, sidecar) = crate::file_management::parse_virtual_path(&target);

        write_develop_profile_to_sidecar(
            &sidecar,
            Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
        )
        .expect("seed a profile");
        assert!(read_sidecar(&sidecar)["develop_state"]["profile"].is_object());

        let profile = ProfileUpdate::Set { profile: None };
        apply_develop_state_batch(&[target], None, &profile, None, false, false);

        let stored = read_sidecar(&sidecar);
        assert!(
            stored["develop_state"].get("profile").is_none(),
            "a null profile in a Set clears the reference entirely: {stored}"
        );
        assert_eq!(
            resolve_develop_profile_from_sidecar(&sidecar, std::slice::from_ref(&root)),
            DevelopProfileRestore::None
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn combined_batch_is_virtual_copy_aware() {
        let root = unique_temp_dir("combined_vc_root");
        let physical_sidecar = write_sidecar(&root, "photo.NEF.rrdata", &legacy_document());
        let physical_before = fs::read(&physical_sidecar).expect("read the physical sidecar");

        let virtual_path = format!("{}?vc=copy-1", image_path(&root, "photo"));
        let delta = json!({ "exposure": 1.25 });
        let profile = ProfileUpdate::Set {
            profile: Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 55.0)),
        };

        apply_develop_state_batch(
            std::slice::from_ref(&virtual_path),
            Some(&delta),
            &profile,
            None,
            false,
            false,
        );

        let (_, copy_sidecar) = crate::file_management::parse_virtual_path(&virtual_path);
        assert_eq!(
            copy_sidecar,
            root.join("photo.NEF.copy-1.rrdata"),
            "a virtual copy must write its OWN sidecar"
        );
        let stored = read_sidecar(&copy_sidecar);
        assert_eq!(stored["adjustments"]["exposure"], json!(1.25));
        assert_eq!(
            stored["develop_state"]["profile"]["uuid"],
            json!("UUID-ALPHA")
        );
        assert_eq!(
            stored["develop_state"]["profile"]["amount_percent"],
            json!(55.0)
        );

        assert_eq!(
            fs::read(&physical_sidecar).expect("re-read the physical sidecar"),
            physical_before,
            "the physical image's own state must be byte-unchanged"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    /// A target whose existing develop state was written by a newer build does
    /// not lose the adjustments half of the paste: the profile is refused (the
    /// record stays byte-for-byte as it was read) while the merged adjustment
    /// delta is still written.
    ///
    /// Non-vacuity: propagating the profile refusal with `?` (as
    /// `write_develop_profile_to_sidecar` still does for a profile-only write)
    /// aborts the whole per-path body, leaving `adjustments` untouched and
    /// failing the `exposure == 1.0` assertion below.
    #[test]
    fn combined_batch_keeps_a_newer_develop_state_but_still_applies_adjustments() {
        let root = unique_temp_dir("combined_newer_root");
        let newer = image_path(&root, "newer");
        let (_, newer_sidecar) = crate::file_management::parse_virtual_path(&newer);
        write_sidecar(
            &root,
            "newer.NEF.rrdata",
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
        let before = read_sidecar(&newer_sidecar);
        let before_develop_state = before["develop_state"].clone();

        let good = image_path(&root, "good");
        let delta = json!({ "exposure": 1.0 });
        let profile = ProfileUpdate::Set {
            profile: Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
        };

        apply_develop_state_batch(
            &[newer.clone(), good.clone()],
            Some(&delta),
            &profile,
            None,
            false,
            false,
        );

        let stored = read_sidecar(&newer_sidecar);
        assert_eq!(
            stored["develop_state"], before_develop_state,
            "a newer develop state is kept byte-unchanged, version, profile and all: {stored}"
        );
        assert_eq!(
            stored["develop_state"]["version"],
            json!(2),
            "the newer record must not be rewritten to the current version"
        );
        assert_eq!(
            stored["develop_state"]["profile"]["uuid"],
            json!("UUID-FUTURE"),
            "the newer profile reference must not be overwritten"
        );
        assert_eq!(
            stored["adjustments"]["exposure"],
            json!(1.0),
            "the adjustments half must still be written even though the profile was refused: {stored}"
        );

        let (_, good_sidecar) = crate::file_management::parse_virtual_path(&good);
        let stored_good = read_sidecar(&good_sidecar);
        assert_eq!(stored_good["adjustments"]["exposure"], json!(1.0));
        assert_eq!(
            stored_good["develop_state"]["profile"]["uuid"],
            json!("UUID-ALPHA"),
            "the other target still succeeds"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn combined_batch_reports_ok_when_one_path_is_unwritable() {
        let root = unique_temp_dir("combined_unwritable_root");
        // Precondition: the parent directory genuinely does not exist, so the
        // write for this one path must fail.
        assert!(
            !root.join("missing-dir").exists(),
            "the fixture parent must not exist"
        );
        let unwritable = root
            .join("missing-dir")
            .join("photo.NEF")
            .to_string_lossy()
            .to_string();
        let good = image_path(&root, "good");

        let delta = json!({ "exposure": 1.0 });
        let profile = ProfileUpdate::Set {
            profile: Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
        };

        // A single target's failure is logged and skipped; the batch itself never
        // rejects the caller.
        apply_develop_state_batch(
            &[unwritable.clone(), good.clone()],
            Some(&delta),
            &profile,
            None,
            false,
            false,
        );

        let (_, good_sidecar) = crate::file_management::parse_virtual_path(&good);
        let stored = read_sidecar(&good_sidecar);
        assert_eq!(
            stored["adjustments"]["exposure"],
            json!(1.0),
            "the writable target must still be written"
        );
        assert_eq!(
            stored["develop_state"]["profile"]["uuid"],
            json!("UUID-ALPHA")
        );

        let (_, bad_sidecar) = crate::file_management::parse_virtual_path(&unwritable);
        assert!(
            !bad_sidecar.exists(),
            "an unwritable target must not produce a file"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn combined_batch_preserves_rating_tags_and_exif() {
        let root = unique_temp_dir("combined_preserve_root");
        let target = image_path(&root, "photo");
        let (_, sidecar) = crate::file_management::parse_virtual_path(&target);
        write_sidecar(&root, "photo.NEF.rrdata", &legacy_document());

        let delta = json!({ "contrast": 12 });
        let profile = ProfileUpdate::Set {
            profile: Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
        };
        apply_develop_state_batch(&[target], Some(&delta), &profile, None, false, false);

        let stored = read_sidecar(&sidecar);
        assert_eq!(stored["rating"], json!(4));
        assert_eq!(stored["tags"], json!(["keep"]));
        assert_eq!(stored["exif"], json!({ "Make": "Once" }));
        assert_eq!(
            stored["adjustments"]["exposure"],
            json!(0.5),
            "the existing adjustment key survives the merge"
        );
        assert_eq!(
            stored["adjustments"]["contrast"],
            json!(12),
            "the pasted adjustment key is merged in"
        );
        assert_eq!(
            stored["develop_state"]["profile"]["uuid"],
            json!("UUID-ALPHA")
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    /// Tripwire, NOT evidence: the profile-only batch only ever writes a target's
    /// `.rrdata`, so with the current code this assertion cannot fail. It exists
    /// to catch a FUTURE change that would make the profile-only batch begin
    /// touching the image's `<img>.xmp`, and must not be read as proving the XMP
    /// sidecar is safe today. (The pre-existing rating/label/tag XMP sync under
    /// `enable_xmp_sync` fires from `ApplyAdjustmentsToPaths` and, when the batch
    /// carries adjustments, from the combined batch; a profile-only write never
    /// syncs.)
    #[test]
    fn batch_write_never_touches_an_xmp_sidecar() {
        let root = unique_temp_dir("batch_xmp_root");
        let xmp_path = write_sidecar(&root, "photo.NEF.xmp", &json!({ "handwritten": true }));
        let xmp_before = fs::read(&xmp_path).expect("read the xmp sidecar");

        save_develop_profile_for_paths(
            vec![image_path(&root, "photo")],
            Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
        )
        .expect("batch write");

        assert_eq!(
            fs::read(&xmp_path).expect("re-read the xmp sidecar"),
            xmp_before,
            "only the explicit 6C3 commands may write an XMP sidecar"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    /// FIX 1: the combined paste path restores the pre-6C4 rating/label/tag XMP
    /// sync. With `enable_xmp_sync` on and adjustments present, the target's
    /// `<img>.xmp` is rewritten from the sidecar's rating (`xmp:Rating`), colour
    /// tag (`xmp:Label`) and plain tags (`dc:subject`); a profile-only paste
    /// (`adjustments: None`) must never touch it.
    ///
    /// The exact observable form is `sync_metadata_to_xmp`'s behaviour: it
    /// replaces the existing `xmp:Rating` attribute, inserts an `xmp:Label`
    /// element and inserts a `dc:subject`/`rdf:Bag` for the non-colour tags.
    ///
    /// Non-vacuity:
    ///  - positive half: deleting the `sync_metadata_to_xmp` call from
    ///    `apply_develop_state_for_path` (or dropping its `enable_xmp_sync`
    ///    guard) leaves the XMP at `xmp:Rating="0"` and fails the sync asserts;
    ///  - negative half: dropping the `adjustments.is_some()` gate lets the
    ///    profile-only paste sync too, rewriting `xmp:Rating="0"` to `"4"` and
    ///    failing the byte-unchanged assert.
    #[test]
    fn combined_batch_syncs_augmented_xmp_when_adjustments_change() {
        let root = unique_temp_dir("combined_xmp_sync_root");

        // The pre-existing XMP a paste would amend: no label, no subject and a
        // zero rating the sidecar's rating/label/tags must overwrite.
        let xmp_fixture = |rating: &str| {
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RapidRAW">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmp:Rating="{rating}">
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#
            )
        };

        // The sidecar carries rating 4, the colour tag `color:red` (-> label
        // "Red") and the plain tag `keep` (-> dc:subject bag), none of which the
        // XMP currently holds.
        let sidecar_document = json!({
            "version": 1,
            "rating": 4,
            "adjustments": { "exposure": 0.5 },
            "tags": ["color:red", "keep"],
        });

        // --- positive half: adjustments are present, so the XMP is synced.
        let synced = image_path(&root, "synced");
        write_sidecar(&root, "synced.NEF.rrdata", &sidecar_document);
        let synced_xmp = root.join("synced.xmp");
        fs::write(&synced_xmp, xmp_fixture("0")).expect("write xmp fixture");
        assert!(
            !fs::read_to_string(&synced_xmp)
                .expect("read xmp fixture")
                .contains(r#"xmp:Rating="4""#),
            "precondition: the XMP must not already carry the sidecar's rating"
        );

        apply_develop_state_batch(
            std::slice::from_ref(&synced),
            Some(&json!({ "contrast": 20 })),
            &ProfileUpdate::Keep,
            None,
            true,
            false,
        );

        let (_, synced_sidecar) = crate::file_management::parse_virtual_path(&synced);
        assert_eq!(
            read_sidecar(&synced_sidecar)["adjustments"]["contrast"],
            json!(20),
            "precondition: the batch really ran and wrote the sidecar"
        );

        let synced_after = fs::read_to_string(&synced_xmp).expect("read synced xmp");
        assert!(
            synced_after.contains(r#"xmp:Rating="4""#),
            "the sidecar's rating must be synced into the XMP: {synced_after}"
        );
        assert!(
            synced_after.contains("<xmp:Label>Red</xmp:Label>"),
            "the sidecar's colour tag must become the XMP label: {synced_after}"
        );
        assert!(
            synced_after.contains("<rdf:li>keep</rdf:li>"),
            "the sidecar's plain tags must become the dc:subject bag: {synced_after}"
        );

        // --- negative half: a profile-only paste must leave the XMP untouched.
        let profile_only = image_path(&root, "profile_only");
        write_sidecar(&root, "profile_only.NEF.rrdata", &sidecar_document);
        let profile_only_xmp = root.join("profile_only.xmp");
        fs::write(&profile_only_xmp, xmp_fixture("0")).expect("write xmp fixture");
        let profile_only_xmp_before = fs::read(&profile_only_xmp).expect("read xmp fixture");

        apply_develop_state_batch(
            std::slice::from_ref(&profile_only),
            None,
            &ProfileUpdate::Set {
                profile: Some(reference(Some("UUID-ALPHA"), None, Some("Alpha"), 100.0)),
            },
            None,
            true,
            false,
        );

        let (_, profile_only_sidecar) = crate::file_management::parse_virtual_path(&profile_only);
        assert_eq!(
            read_sidecar(&profile_only_sidecar)["develop_state"]["profile"]["uuid"],
            json!("UUID-ALPHA"),
            "precondition: the profile-only batch really ran"
        );
        assert_eq!(
            fs::read(&profile_only_xmp).expect("re-read xmp"),
            profile_only_xmp_before,
            "a profile-only change must never sync the rating/label/tag XMP"
        );

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    /// FIX 2: the combined command deduplicates its incoming paths, preserving
    /// first-seen order, BEFORE the thumbnail queue count and the per-path loop,
    /// so a repeated path is read and written once. The command itself needs an
    /// `AppHandle`, so this asserts the exact helper the command calls; the
    /// duplicate-write risk it removes cannot be observed through file content
    /// (the merge is idempotent).
    ///
    /// Non-vacuity: making `deduplicate_paths` a pass-through (returning its
    /// input unchanged) yields the five-element input and fails the equality.
    #[test]
    fn apply_develop_state_to_paths_deduplicates_its_paths() {
        let a = "/photos/a.NEF".to_string();
        let b = "/photos/b.NEF".to_string();

        let deduped =
            deduplicate_paths(vec![a.clone(), b.clone(), a.clone(), b.clone(), a.clone()]);

        assert_eq!(
            deduped,
            vec![a, b],
            "duplicates must be removed while first-seen order is preserved"
        );
    }
}
