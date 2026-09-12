use serde::{Deserialize, Serialize};

pub const DEVELOP_STATE_VERSION: u32 = 1;
pub const DEFAULT_AMOUNT_PERCENT: f32 = 100.0;

fn default_amount_percent() -> f32 {
    DEFAULT_AMOUNT_PERCENT
}

/// A record with no `version` is not a *future* record, it is a malformed or
/// hand-written one; the compatibility policy maps an older/missing state to the
/// baseline shape, so it is read as the current schema. A record that DOES carry
/// a version keeps it verbatim.
fn default_version() -> u32 {
    DEVELOP_STATE_VERSION
}

/// A reference to the Adobe XMP RGB profile an image is developed with.
///
/// Identity is UUID-first with the saved path as the fallback. `name` is
/// display-only and must NEVER be used to resolve a profile: resolving by name
/// alone could silently apply a different profile. An image may legitimately
/// reference a profile that is not installed, so both identifiers are optional
/// and the reference is retained until the profile is available again.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DevelopProfileReference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Profile Amount in PERCENT, matching the editor's `xmpProfileAmountPercent`
    /// domain (100 = the profile exactly as authored).
    #[serde(default = "default_amount_percent")]
    pub amount_percent: f32,
}

/// The persisted per-image develop state record.
///
/// Holds the schema version and the profile reference. The adjustment payload
/// stays in `ImageMetadata::adjustments`, which remains the single place
/// adjustments are stored; together they are the canonical develop state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DevelopState {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<DevelopProfileReference>,
    /// Keys this build does not know about, preserved so that reading and
    /// rewriting a document written by a newer build cannot silently drop its
    /// data. Never interpreted.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl Default for DevelopState {
    fn default() -> Self {
        DevelopState {
            version: DEVELOP_STATE_VERSION,
            profile: None,
            unknown: serde_json::Map::new(),
        }
    }
}

#[allow(dead_code)]
impl DevelopState {
    /// Whether this build may interpret the record. A record written by a newer
    /// build is preserved but never applied.
    pub fn is_supported(&self) -> bool {
        self.version <= DEVELOP_STATE_VERSION
    }
}

/// One currently visible profile, as the app can see it right now.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredProfile {
    pub uuid: String,
    pub path: String,
    pub name: String,
}

/// Outcome of resolving a saved reference against what is currently visible.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum ProfileResolution {
    /// The referenced profile is available; apply it at `amount_percent`.
    Resolved {
        profile: DiscoveredProfile,
        amount_percent: f32,
    },
    /// No profile available. The reference is returned unchanged so it can
    /// resolve later if the profile is installed or restored.
    Missing { reference: DevelopProfileReference },
}

#[allow(dead_code)]
pub fn resolve_profile_reference(
    reference: &DevelopProfileReference,
    candidates: &[DiscoveredProfile],
) -> ProfileResolution {
    let uuid = reference.uuid.as_deref().filter(|value| !value.is_empty());
    let path = reference.path.as_deref().filter(|value| !value.is_empty());

    let resolved = match (uuid, path) {
        (Some(uuid), Some(path)) => {
            let at_path = candidates
                .iter()
                .find(|candidate| candidate.path == path && candidate.uuid == uuid);
            at_path.or_else(|| candidates.iter().find(|candidate| candidate.uuid == uuid))
        }
        (Some(uuid), None) => candidates.iter().find(|candidate| candidate.uuid == uuid),
        (None, Some(path)) => candidates.iter().find(|candidate| candidate.path == path),
        (None, None) => None,
    };

    match resolved {
        Some(profile) => ProfileResolution::Resolved {
            profile: profile.clone(),
            amount_percent: reference.amount_percent,
        },
        None => ProfileResolution::Missing {
            reference: reference.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_processing::ImageMetadata;
    use serde_json::json;

    fn reference(
        uuid: Option<&str>,
        path: Option<&str>,
        name: Option<&str>,
    ) -> DevelopProfileReference {
        DevelopProfileReference {
            uuid: uuid.map(str::to_string),
            path: path.map(str::to_string),
            name: name.map(str::to_string),
            amount_percent: DEFAULT_AMOUNT_PERCENT,
        }
    }

    fn discovered(uuid: &str, path: &str, name: &str) -> DiscoveredProfile {
        DiscoveredProfile {
            uuid: uuid.to_string(),
            path: path.to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn default_state_is_version_1_with_no_profile() {
        let state = DevelopState::default();
        assert_eq!(state.version, 1);
        assert!(state.profile.is_none());
        assert!(state.unknown.is_empty());
    }

    #[test]
    fn develop_state_round_trips_through_json() {
        let state = DevelopState {
            version: DEVELOP_STATE_VERSION,
            profile: Some(DevelopProfileReference {
                uuid: Some("uuid-1".to_string()),
                path: Some("/profiles/a.xmp".to_string()),
                name: Some("Profile A".to_string()),
                amount_percent: 150.0,
            }),
            unknown: serde_json::Map::new(),
        };

        let json = serde_json::to_string(&state).expect("serialize develop state");
        let back: DevelopState = serde_json::from_str(&json).expect("deserialize develop state");

        assert_eq!(back, state);
        let profile = back.profile.expect("profile survives the round trip");
        assert_eq!(profile.uuid.as_deref(), Some("uuid-1"));
        assert_eq!(profile.path.as_deref(), Some("/profiles/a.xmp"));
        assert_eq!(profile.name.as_deref(), Some("Profile A"));
        assert_eq!(profile.amount_percent, 150.0);
    }

    #[test]
    fn no_profile_state_round_trips() {
        let state = DevelopState {
            version: DEVELOP_STATE_VERSION,
            profile: None,
            unknown: serde_json::Map::new(),
        };

        let json = serde_json::to_string(&state).expect("serialize develop state");
        let back: DevelopState = serde_json::from_str(&json).expect("deserialize develop state");

        assert!(back.profile.is_none());

        let value: serde_json::Value = serde_json::from_str(&json).expect("parse serialized form");
        assert!(
            value.get("profile").is_none(),
            "an absent profile must be omitted, not serialized as null: {json}"
        );
    }

    #[test]
    fn profile_reference_round_trips() {
        let reference = DevelopProfileReference {
            uuid: Some("uuid-1".to_string()),
            path: Some("/profiles/a.xmp".to_string()),
            name: Some("Profile A".to_string()),
            amount_percent: 100.0,
        };

        let json = serde_json::to_string(&reference).expect("serialize reference");
        let back: DevelopProfileReference =
            serde_json::from_str(&json).expect("deserialize reference");

        assert_eq!(back, reference);
    }

    #[test]
    fn amount_percent_round_trips() {
        for amount in [0.0_f32, 50.0, 100.0, 200.0, 33.5] {
            let reference = DevelopProfileReference {
                uuid: Some("uuid-1".to_string()),
                path: None,
                name: None,
                amount_percent: amount,
            };

            let json = serde_json::to_string(&reference).expect("serialize reference");
            let back: DevelopProfileReference =
                serde_json::from_str(&json).expect("deserialize reference");

            assert_eq!(
                back.amount_percent, amount,
                "amount {amount} must round-trip exactly"
            );
        }
    }

    #[test]
    fn missing_uuid_or_path_are_allowed() {
        let no_uuid = reference(None, Some("/profiles/a.xmp"), Some("Profile A"));
        let no_path = reference(Some("uuid-1"), None, None);

        for reference in [no_uuid, no_path] {
            let json = serde_json::to_string(&reference).expect("serialize reference");
            let back: DevelopProfileReference =
                serde_json::from_str(&json).expect("deserialize reference");
            assert_eq!(back, reference);
        }
    }

    #[test]
    fn omitted_amount_defaults_to_100() {
        let reference: DevelopProfileReference =
            serde_json::from_str(r#"{"uuid":"uuid-1"}"#).expect("deserialize reference");
        assert_eq!(reference.amount_percent, DEFAULT_AMOUNT_PERCENT);
    }

    #[test]
    fn missing_version_defaults_to_the_current_version() {
        let state: DevelopState =
            serde_json::from_str("{}").expect("a version-less record deserializes");
        assert_eq!(state.version, DEVELOP_STATE_VERSION);
        assert!(state.profile.is_none());
        assert!(state.unknown.is_empty());
        assert!(state.is_supported());

        // The end-to-end hazard that motivated the fix: `load_sidecar` falls back
        // to `ImageMetadata::default()` on ANY parse error, so a malformed
        // develop-state object must not fail the WHOLE sidecar and silently
        // discard this image's rating and adjustments.
        let document =
            r#"{"version":1,"rating":4,"adjustments":{"exposure":0.5},"develop_state":{}}"#;
        let metadata: ImageMetadata = serde_json::from_str(document)
            .expect("a sidecar with an empty develop_state object still parses");

        assert_eq!(metadata.rating, 4, "rating survives a version-less develop_state");
        assert_eq!(
            metadata.adjustments,
            json!({ "exposure": 0.5 }),
            "adjustments survive a version-less develop_state"
        );
        assert_eq!(metadata.develop_state(), DevelopState::default());
    }

    #[test]
    fn legacy_metadata_without_develop_state_maps_to_the_default() {
        let json = r#"{"version":1,"rating":3,"adjustments":{"exposure":1.0}}"#;
        let metadata: ImageMetadata =
            serde_json::from_str(json).expect("legacy sidecar deserializes");

        assert!(metadata.develop_state.is_none());
        assert_eq!(metadata.develop_state(), DevelopState::default());
    }

    #[test]
    fn future_state_version_is_retained_but_not_applied() {
        let json = r#"{"version":2,"futureField":7}"#;
        let state: DevelopState = serde_json::from_str(json).expect("future document deserializes");

        assert_eq!(state.version, 2, "the future version is preserved verbatim");
        assert!(!state.is_supported());
        assert!(DevelopState::default().is_supported());
        assert_eq!(state.unknown.get("futureField"), Some(&json!(7)));

        let round = serde_json::to_string(&state).expect("serialize future state");
        let reparsed: DevelopState = serde_json::from_str(&round).expect("re-deserialize");

        assert_eq!(reparsed.version, 2);
        assert_eq!(reparsed.unknown.get("futureField"), Some(&json!(7)));
    }

    #[test]
    fn unknown_keys_are_preserved_on_round_trip() {
        let json = r#"{"version":1,"a":1,"b":"two","c":[3,4]}"#;
        let state: DevelopState = serde_json::from_str(json).expect("deserialize");

        let mut parked: Vec<String> = state.unknown.keys().cloned().collect();
        parked.sort();
        assert_eq!(
            parked,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(state.unknown.get("a"), Some(&json!(1)));
        assert_eq!(state.unknown.get("b"), Some(&json!("two")));
        assert_eq!(state.unknown.get("c"), Some(&json!([3, 4])));

        let round = serde_json::to_string(&state).expect("serialize");
        let reparsed: DevelopState = serde_json::from_str(&round).expect("re-deserialize");
        assert_eq!(reparsed.unknown, state.unknown);
    }

    #[test]
    fn resolution_prefers_the_saved_path_when_its_uuid_matches() {
        let at_path = discovered("uuid-1", "/a", "A");
        let same_uuid_elsewhere = discovered("uuid-1", "/b", "B");
        let reference = reference(Some("uuid-1"), Some("/a"), None);

        let resolution =
            resolve_profile_reference(&reference, &[same_uuid_elsewhere, at_path.clone()]);

        assert_eq!(
            resolution,
            ProfileResolution::Resolved {
                profile: at_path,
                amount_percent: DEFAULT_AMOUNT_PERCENT,
            }
        );
    }

    #[test]
    fn resolution_falls_back_to_uuid_lookup_when_the_saved_path_is_gone() {
        let candidate = discovered("uuid-1", "/b", "B");
        let reference = reference(Some("uuid-1"), Some("/gone"), None);

        let resolution = resolve_profile_reference(&reference, std::slice::from_ref(&candidate));

        assert_eq!(
            resolution,
            ProfileResolution::Resolved {
                profile: candidate,
                amount_percent: DEFAULT_AMOUNT_PERCENT,
            }
        );
    }

    #[test]
    fn resolution_never_uses_a_different_uuid_at_the_saved_path() {
        let wrong_uuid_at_path = discovered("other-uuid", "/p", "X");
        let uuid_match_elsewhere = discovered("uuid-1", "/elsewhere", "Y");
        let reference = reference(Some("uuid-1"), Some("/p"), None);

        let resolved = resolve_profile_reference(
            &reference,
            &[wrong_uuid_at_path.clone(), uuid_match_elsewhere.clone()],
        );
        assert_eq!(
            resolved,
            ProfileResolution::Resolved {
                profile: uuid_match_elsewhere,
                amount_percent: DEFAULT_AMOUNT_PERCENT,
            }
        );

        let missing =
            resolve_profile_reference(&reference, std::slice::from_ref(&wrong_uuid_at_path));
        assert_eq!(
            missing,
            ProfileResolution::Missing {
                reference: reference.clone(),
            }
        );
    }

    #[test]
    fn resolution_never_matches_by_name() {
        let same_name = discovered("other-uuid", "/other", "Same Name");
        let with_identifiers = reference(Some("uuid-1"), Some("/p"), Some("Same Name"));

        let resolution =
            resolve_profile_reference(&with_identifiers, std::slice::from_ref(&same_name));
        assert_eq!(
            resolution,
            ProfileResolution::Missing {
                reference: with_identifiers.clone(),
            }
        );

        let name_only = reference(None, None, Some("Same Name"));
        let resolution = resolve_profile_reference(&name_only, std::slice::from_ref(&same_name));
        assert_eq!(
            resolution,
            ProfileResolution::Missing {
                reference: name_only.clone(),
            }
        );
    }

    #[test]
    fn resolution_without_any_identifier_is_missing() {
        let candidate = discovered("uuid-1", "/p", "Whatever");
        let reference = reference(None, None, Some("Whatever"));

        let resolution = resolve_profile_reference(&reference, std::slice::from_ref(&candidate));

        assert_eq!(
            resolution,
            ProfileResolution::Missing {
                reference: reference.clone(),
            }
        );
    }

    #[test]
    fn resolution_treats_blank_identifiers_as_absent() {
        let real = discovered("uuid-1", "/p", "P");

        let blank_uuid_with_path = reference(Some(""), Some("/p"), None);
        assert_eq!(
            resolve_profile_reference(&blank_uuid_with_path, std::slice::from_ref(&real)),
            ProfileResolution::Resolved {
                profile: real.clone(),
                amount_percent: DEFAULT_AMOUNT_PERCENT,
            }
        );

        let blank_candidate = discovered("", "/z", "Z");
        let blank_only = reference(Some(""), None, None);
        assert_eq!(
            resolve_profile_reference(&blank_only, &[blank_candidate]),
            ProfileResolution::Missing {
                reference: blank_only.clone(),
            }
        );

        let both_blank = reference(Some(""), Some(""), None);
        assert_eq!(
            resolve_profile_reference(&both_blank, std::slice::from_ref(&real)),
            ProfileResolution::Missing {
                reference: both_blank.clone(),
            }
        );
    }

    #[test]
    fn resolution_carries_the_amount_into_the_resolved_profile() {
        let candidate = discovered("uuid-1", "/p", "P");
        let reference = DevelopProfileReference {
            uuid: Some("uuid-1".to_string()),
            path: Some("/p".to_string()),
            name: None,
            amount_percent: 42.5,
        };

        let resolution = resolve_profile_reference(&reference, std::slice::from_ref(&candidate));

        assert_eq!(
            resolution,
            ProfileResolution::Resolved {
                profile: candidate,
                amount_percent: 42.5,
            }
        );
    }

    #[test]
    fn resolution_is_deterministic_for_duplicate_uuids() {
        let first = discovered("dup", "/1", "1");
        let second = discovered("dup", "/2", "2");
        let reference = reference(Some("dup"), None, None);
        let candidates = [first.clone(), second.clone()];

        let a = resolve_profile_reference(&reference, &candidates);
        let b = resolve_profile_reference(&reference, &candidates);

        assert_eq!(a, b);
        assert_eq!(
            a,
            ProfileResolution::Resolved {
                profile: first.clone(),
                amount_percent: DEFAULT_AMOUNT_PERCENT,
            }
        );

        // The rule is "the FIRST candidate in the GIVEN order", not a sorting,
        // last-match, or name-preferred rule: reversing the candidates must
        // resolve the OTHER duplicate.
        let reversed = [second.clone(), first];
        let reversed_resolution = resolve_profile_reference(&reference, &reversed);
        assert_eq!(
            reversed_resolution,
            ProfileResolution::Resolved {
                profile: second,
                amount_percent: DEFAULT_AMOUNT_PERCENT,
            },
            "reversing the candidate order must select the other duplicate"
        );
    }

    #[test]
    fn serialized_develop_state_contains_only_develop_keys() {
        let clean = DevelopState {
            version: DEVELOP_STATE_VERSION,
            profile: Some(DevelopProfileReference {
                uuid: Some("uuid-1".to_string()),
                path: Some("/profiles/a.xmp".to_string()),
                name: Some("A".to_string()),
                amount_percent: 120.0,
            }),
            unknown: serde_json::Map::new(),
        };

        let value = serde_json::to_value(&clean).expect("serialize develop state");
        let object = value.as_object().expect("develop state is an object");
        let mut keys: Vec<String> = object.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["profile".to_string(), "version".to_string()],
            "a develop state with no unknown keys must serialize only develop keys"
        );

        let ui_keys = [
            "zoom",
            "histogram",
            "selectedImage",
            "xmpProfileRollback",
            "finalPreviewUrl",
            "isSliderDragging",
        ];
        let mut polluted = json!({
            "version": 1,
            "profile": { "uuid": "uuid-1", "amount_percent": 100.0 },
        });
        for key in ui_keys {
            polluted[key] = json!("transient-ui-value");
        }

        let parsed: DevelopState = serde_json::from_value(polluted).expect("deserialize");

        assert_eq!(parsed.version, 1, "a UI key must not overwrite the version");
        assert_eq!(
            parsed.profile,
            Some(DevelopProfileReference {
                uuid: Some("uuid-1".to_string()),
                path: None,
                name: None,
                amount_percent: 100.0,
            })
        );

        let mut parked: Vec<String> = parsed.unknown.keys().cloned().collect();
        parked.sort();
        let mut expected: Vec<String> = ui_keys.iter().map(|key| key.to_string()).collect();
        expected.sort();
        assert_eq!(
            parked, expected,
            "editor-transient UI keys must land in `unknown`, never interpreted"
        );
        for key in ui_keys {
            assert_eq!(parsed.unknown[key], json!("transient-ui-value"));
        }

        let reserialized = serde_json::to_value(&parsed).expect("serialize");
        let reserialized_object = reserialized
            .as_object()
            .expect("develop state is an object");
        assert_eq!(reserialized_object.get("version"), Some(&json!(1)));
        assert!(reserialized_object.contains_key("profile"));
        for key in ui_keys {
            assert!(
                reserialized_object.contains_key(key),
                "unknown key {key} is preserved without being promoted"
            );
        }
    }
}
