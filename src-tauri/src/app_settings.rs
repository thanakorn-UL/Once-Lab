use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::app_state::AppState;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SortCriteria {
    pub key: String,
    pub order: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FilterCriteria {
    pub rating: u8,
    pub raw_status: String,
    #[serde(default)]
    pub edited_status: Option<String>,
    #[serde(default)]
    pub colors: Vec<String>,
}

impl Default for FilterCriteria {
    fn default() -> Self {
        Self {
            rating: 0,
            raw_status: "all".to_string(),
            edited_status: Some("all".to_string()),
            colors: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FolderTreeSort {
    pub key: String,
    pub order: String,
}

impl Default for FolderTreeSort {
    fn default() -> Self {
        Self {
            key: "name".to_string(),
            order: "asc".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LastFolderState {
    #[serde(default)]
    pub current_folder_path: Option<String>,
    #[serde(default)]
    pub expanded_folders: Vec<String>,
    #[serde(default)]
    pub active_album_id: Option<String>,
    #[serde(default)]
    pub expanded_album_groups: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MyLens {
    pub maker: String,
    pub model: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum PasteMode {
    Merge,
    Replace,
}

pub fn all_available_adjustments() -> HashSet<String> {
    [
        "exposure",
        "brightness",
        "contrast",
        "curves",
        "pointCurves",
        "parametricCurve",
        "curveMode",
        "highlights",
        "shadows",
        "whites",
        "blacks",
        "toneMapper",
        "temperature",
        "tint",
        "saturation",
        "vibrance",
        "hsl",
        "hue",
        "colorGrading",
        "colorCalibration",
        "clarity",
        "structure",
        "dehaze",
        "sharpness",
        "sharpnessThreshold",
        "centré",
        "lumaNoiseReduction",
        "colorNoiseReduction",
        "chromaticAberrationRedCyan",
        "chromaticAberrationBlueYellow",
        "vignetteAmount",
        "vignetteFeather",
        "vignetteMidpoint",
        "vignetteRoundness",
        "grainAmount",
        "grainRoughness",
        "grainSize",
        "lutIntensity",
        "lutName",
        "lutPath",
        "lutSize",
        "lutData",
        "lutIsSceneReferred",
        "glowAmount",
        "halationAmount",
        "flareAmount",
        "crop",
        "aspectRatio",
        "rotation",
        "flipHorizontal",
        "flipVertical",
        "orientationSteps",
        "transformDistortion",
        "transformVertical",
        "transformHorizontal",
        "transformRotate",
        "transformAspect",
        "transformScale",
        "transformXOffset",
        "transformYOffset",
        "masks",
        "xmpProfile",
        "lensCorrectionMode",
        "lensMaker",
        "lensModel",
        "lensDistortionAmount",
        "lensVignetteAmount",
        "lensTcaAmount",
        "lensDistortionEnabled",
        "lensTcaEnabled",
        "lensVignetteEnabled",
        "guidedPerspective",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn default_included_adjustments() -> HashSet<String> {
    let mut defaults = all_available_adjustments();

    let off_by_default = [
        "crop",
        "aspectRatio",
        "rotation",
        "flipHorizontal",
        "flipVertical",
        "orientationSteps",
        "transformDistortion",
        "transformVertical",
        "transformHorizontal",
        "transformRotate",
        "transformAspect",
        "transformScale",
        "transformXOffset",
        "transformYOffset",
        "masks",
        "lensCorrectionMode",
        "lensMaker",
        "lensModel",
        "lensDistortionAmount",
        "lensVignetteAmount",
        "lensTcaAmount",
        "lensDistortionEnabled",
        "lensTcaEnabled",
        "lensVignetteEnabled",
        "guidedPerspective",
    ];

    for item in off_by_default.iter() {
        defaults.remove(*item);
    }

    defaults
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CopyPasteSettings {
    pub mode: PasteMode,
    #[serde(default = "default_included_adjustments")]
    pub included_adjustments: HashSet<String>,
    #[serde(default)]
    pub known_adjustments: HashSet<String>,
    #[serde(default)]
    pub auto_sync: bool,
}

impl Default for CopyPasteSettings {
    fn default() -> Self {
        Self {
            mode: PasteMode::Merge,
            included_adjustments: default_included_adjustments(),
            known_adjustments: all_available_adjustments(),
            auto_sync: false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ExportPreset {
    pub id: String,
    pub name: String,
    pub file_format: String,
    pub jpeg_quality: u8,
    pub enable_resize: bool,
    pub resize_mode: String,
    pub resize_value: u32,
    pub dont_enlarge: bool,
    pub keep_metadata: bool,
    pub strip_gps: bool,
    pub filename_template: String,
    pub enable_watermark: bool,
    pub watermark_path: Option<String>,
    pub watermark_anchor: Option<String>,
    pub watermark_scale: u32,
    pub watermark_spacing: u32,
    pub watermark_opacity: u32,
    #[serde(default)]
    pub export_masks: Option<bool>,
    #[serde(default)]
    pub preserve_folders: Option<bool>,
    #[serde(default)]
    pub last_export_path: Option<String>,
    #[serde(default)]
    pub destination_type: Option<String>,
    #[serde(default)]
    pub subfolder: Option<String>,
}

pub fn default_export_presets() -> Vec<ExportPreset> {
    vec![
        ExportPreset {
            id: "default-hq".to_string(),
            name: "High Quality".to_string(),
            file_format: "jpeg".to_string(),
            jpeg_quality: 95,
            enable_resize: false,
            resize_mode: "longEdge".to_string(),
            resize_value: 2048,
            dont_enlarge: true,
            keep_metadata: true,
            strip_gps: false,
            filename_template: "{original_filename}".to_string(),
            enable_watermark: false,
            watermark_path: None,
            watermark_anchor: Some("bottomRight".to_string()),
            watermark_scale: 10,
            watermark_spacing: 5,
            watermark_opacity: 75,
            export_masks: Some(false),
            preserve_folders: Some(false),
            last_export_path: None,
            destination_type: Some("customFolder".to_string()),
            subfolder: Some("".to_string()),
        },
        ExportPreset {
            id: "default-fast".to_string(),
            name: "Fast (Web)".to_string(),
            file_format: "jpeg".to_string(),
            jpeg_quality: 80,
            enable_resize: true,
            resize_mode: "width".to_string(),
            resize_value: 2048,
            dont_enlarge: true,
            keep_metadata: false,
            strip_gps: true,
            filename_template: "{original_filename}_web".to_string(),
            enable_watermark: false,
            watermark_path: None,
            watermark_anchor: Some("bottomRight".to_string()),
            watermark_scale: 10,
            watermark_spacing: 5,
            watermark_opacity: 75,
            export_masks: Some(false),
            preserve_folders: Some(false),
            last_export_path: None,
            destination_type: Some("customFolder".to_string()),
            subfolder: Some("".to_string()),
        },
    ]
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceState {
    pub left_panel_width: u32,
    pub right_panel_width: u32,
    pub left_top_height: u32,
    pub right_top_height: u32,
    pub panel_layout: HashMap<String, Vec<String>>,
    pub active_panels: HashMap<String, Option<String>>,
    pub panel_switcher_placement: HashMap<String, String>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        let mut panel_layout = HashMap::new();
        #[allow(unused)]
        let mut left_top = vec![
            "metadata".to_string(),
            "folderTree".to_string(),
            "export".to_string(),
        ];

        #[cfg(feature = "tethering")]
        left_top.push("tethering".to_string());

        panel_layout.insert("leftTop".to_string(), left_top);
        panel_layout.insert("leftBottom".to_string(), vec![]);

        panel_layout.insert(
            "rightTop".to_string(),
            vec![
                "adjustments".to_string(),
                "crop".to_string(),
                "masks".to_string(),
                "ai".to_string(),
                "presets".to_string(),
            ],
        );
        panel_layout.insert("rightBottom".to_string(), vec![]);

        let mut active_panels = HashMap::new();
        active_panels.insert("leftTop".to_string(), Some("folderTree".to_string()));
        active_panels.insert("leftBottom".to_string(), None);
        active_panels.insert("rightTop".to_string(), Some("adjustments".to_string()));
        active_panels.insert("rightBottom".to_string(), None);

        let mut panel_switcher_placement = HashMap::new();
        panel_switcher_placement.insert("leftTop".to_string(), "bottom".to_string());
        panel_switcher_placement.insert("leftBottom".to_string(), "bottom".to_string());
        panel_switcher_placement.insert("rightTop".to_string(), "right".to_string());
        panel_switcher_placement.insert("rightBottom".to_string(), "right".to_string());

        Self {
            left_panel_width: 320,
            right_panel_width: 320,
            left_top_height: 450,
            right_top_height: 450,
            panel_layout,
            active_panels,
            panel_switcher_placement,
        }
    }
}

pub fn default_linear_raw_mode() -> String {
    "auto".to_string()
}

pub fn default_tagging_shortcuts_option() -> Option<Vec<String>> {
    Some(vec![
        "portrait".to_string(),
        "landscape".to_string(),
        "architecture".to_string(),
        "travel".to_string(),
        "street".to_string(),
        "family".to_string(),
        "nature".to_string(),
        "food".to_string(),
        "event".to_string(),
    ])
}

pub fn default_adjustment_visibility() -> HashMap<String, bool> {
    let mut map = HashMap::new();
    map.insert("sharpening".to_string(), true);
    map.insert("presence".to_string(), true);
    map.insert("noiseReduction".to_string(), true);
    map.insert("chromaticAberration".to_string(), false);
    map.insert("vignette".to_string(), true);
    map.insert("colorCalibration".to_string(), false);
    map.insert("grain".to_string(), true);
    map
}

pub fn default_open_tree_sections() -> Vec<String> {
    vec!["current".to_string()]
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub last_root_path: Option<String>,
    #[serde(default)]
    pub root_folders: Vec<String>,
    #[serde(default)]
    pub pinned_folders: Vec<String>,
    pub editor_preview_resolution: Option<u32>,
    #[serde(default)]
    pub small_thumbnail_resolution: Option<u32>,
    #[serde(default)]
    pub medium_thumbnail_resolution: Option<u32>,
    #[serde(default)]
    pub enable_zoom_hifi: Option<bool>,
    #[serde(default)]
    pub use_full_dpi_rendering: Option<bool>,
    #[serde(default)]
    pub high_res_zoom_multiplier: Option<f32>,
    #[serde(default)]
    pub enable_live_previews: Option<bool>,
    #[serde(default)]
    pub live_preview_quality: Option<String>,
    pub sort_criteria: Option<SortCriteria>,
    pub filter_criteria: Option<FilterCriteria>,
    pub theme: Option<String>,
    #[serde(default)]
    pub font_family: Option<String>,
    pub decorations: Option<bool>,
    #[serde(alias = "comfyuiAddress")]
    pub ai_connector_address: Option<String>,
    pub last_folder_state: Option<LastFolderState>,
    pub ui_visibility: Option<Value>,
    pub enable_ai_tagging: Option<bool>,
    pub tagging_thread_count: Option<u32>,
    #[serde(default = "default_tagging_shortcuts_option")]
    pub tagging_shortcuts: Option<Vec<String>>,
    #[serde(default)]
    pub custom_ai_tags: Option<Vec<String>>,
    #[serde(default)]
    pub ai_tag_count: Option<u32>,
    pub thumbnail_size: Option<String>,
    pub thumbnail_aspect_ratio: Option<String>,
    pub ai_provider: Option<String>,
    #[serde(default = "default_adjustment_visibility")]
    pub adjustment_visibility: HashMap<String, bool>,
    #[serde(default = "default_open_tree_sections")]
    pub open_tree_sections: Vec<String>,
    #[serde(default)]
    pub copy_paste_settings: CopyPasteSettings,
    #[serde(default)]
    pub raw_highlight_compression: Option<f32>,
    #[serde(default)]
    pub processing_backend: Option<String>,
    #[serde(default)]
    pub linux_gpu_optimization: Option<bool>,
    #[serde(default)]
    pub linux_gpu_optimization_migrated_v1: Option<bool>,
    #[serde(default)]
    pub library_view_mode: Option<String>,
    #[serde(default = "default_export_presets")]
    pub export_presets: Vec<ExportPreset>,
    #[serde(default)]
    pub my_lenses: Option<Vec<MyLens>>,
    #[serde(default)]
    pub enable_folder_image_counts: Option<bool>,
    #[serde(default)]
    pub display_edit_icon: Option<bool>,
    #[serde(default = "default_linear_raw_mode")]
    pub linear_raw_mode: String,
    #[serde(default)]
    pub enable_xmp_sync: Option<bool>,
    #[serde(default)]
    pub create_xmp_if_missing: Option<bool>,
    /// Profile folders the user added, persisted by exact path so the library
    /// can be restored at startup. Detected Adobe roots are never stored here,
    /// and neither is the active profile nor its amount.
    #[serde(default)]
    pub xmp_profile_roots: Vec<String>,
    #[serde(default)]
    pub is_waveform_visible: Option<bool>,
    #[serde(default)]
    pub waveform_height: Option<u32>,
    #[serde(default)]
    pub active_waveform_channel: Option<String>,
    #[serde(default)]
    pub use_wgpu_renderer: Option<bool>,
    #[serde(default)]
    pub canvas_input_mode: Option<String>,
    #[serde(default)]
    pub zoom_speed_multiplier: Option<f32>,
    #[serde(default)]
    pub zoom_photo_to_pixel_click: Option<bool>,
    #[serde(default)]
    pub keybinds: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub thumbnail_worker_threads: Option<u32>,
    #[serde(default)]
    pub image_cache_size: Option<u32>,
    #[serde(default)]
    pub tonemapper_override_enabled: Option<bool>,
    #[serde(default)]
    pub default_raw_tonemapper: Option<String>,
    #[serde(default)]
    pub default_non_raw_tonemapper: Option<String>,
    #[serde(default)]
    pub enable_focus_mode: Option<bool>,
    #[serde(default)]
    pub folder_icons: Option<HashMap<String, String>>,
    #[serde(default)]
    pub raw_preprocessing_color_nr: Option<f32>,
    #[serde(default)]
    pub raw_preprocessing_sharpening: Option<f32>,
    #[serde(default)]
    pub apply_preprocessing_to_non_raws: Option<bool>,
    #[serde(default)]
    pub exif_overlay: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub folder_tree_sort: Option<FolderTreeSort>,
    #[serde(default)]
    pub library_display_mode: Option<String>,
    #[serde(default)]
    pub grouping: Option<String>,
    #[serde(default)]
    pub require_matching_exif: Option<bool>,
    #[serde(default)]
    pub group_edited_files: Option<bool>,
    #[serde(default, skip_serializing)] // legacy
    #[allow(dead_code)]
    pub group_associated_files: Option<bool>,
    #[serde(default, skip_serializing)] // legacy
    #[allow(dead_code)]
    pub group_preferred_type: Option<String>,
    #[serde(default)]
    pub always_decode_raw_thumbnails: Option<bool>,
    /// Isolated Adobe Lightroom comparison pipeline for RAW files. When
    /// enabled, RAW development uses the unclipped camera -> Linear ProPhoto
    /// calibration, skips the highlight compressor and the post-develop
    /// noise-reduction/sharpening pass, and the output stage uses the plain
    /// sRGB encoding instead of the RAW brightness/contrast emulation. Works
    /// with or without a Creative XMP profile (RAW baseline). Takes effect on
    /// newly loaded images; `ONCELAB_ADOBE_COMPARISON=1` forces it on for
    /// headless runs.
    #[serde(default)]
    pub adobe_comparison_mode: Option<bool>,
    /// Optional path to a user-supplied `.dcp` camera profile used by the
    /// Adobe comparison pipeline for the camera transform, HueSatMap,
    /// LookTable and ProfileToneCurve stages. `None` keeps the Rawler
    /// color-matrix fallback (explicitly not "Adobe Standard" parity).
    #[serde(default)]
    pub adobe_camera_profile_path: Option<String>,
    #[serde(default)]
    pub workspace: WorkspaceState,
}

impl AppSettings {
    /// Whether the isolated Adobe Lightroom comparison pipeline is active.
    /// The persisted setting can be overridden for headless runs with
    /// `ONCELAB_ADOBE_COMPARISON=1` (and `=0` never forces it off; the setting
    /// remains authoritative).
    pub fn adobe_comparison_enabled(&self) -> bool {
        self.adobe_comparison_mode.unwrap_or(false)
            || std::env::var("ONCELAB_ADOBE_COMPARISON").as_deref() == Ok("1")
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            last_root_path: None,
            root_folders: Vec::new(),
            pinned_folders: Vec::new(),
            small_thumbnail_resolution: Some(480),
            medium_thumbnail_resolution: Some(1280),
            #[cfg(target_os = "android")]
            editor_preview_resolution: Some(1280),
            #[cfg(not(target_os = "android"))]
            editor_preview_resolution: Some(1920),
            enable_zoom_hifi: Some(true),
            use_full_dpi_rendering: Some(false),
            enable_live_previews: Some(true),
            live_preview_quality: Some("high".to_string()),
            sort_criteria: None,
            filter_criteria: None,
            theme: Some("dark".to_string()),
            font_family: None,
            decorations: Some(false),
            ai_connector_address: None,
            last_folder_state: None,
            ui_visibility: None,
            enable_ai_tagging: Some(false),
            tagging_thread_count: Some(3),
            tagging_shortcuts: default_tagging_shortcuts_option(),
            custom_ai_tags: Some(Vec::new()),
            ai_tag_count: Some(10),
            #[cfg(target_os = "android")]
            thumbnail_size: Some("small".to_string()),
            #[cfg(not(target_os = "android"))]
            thumbnail_size: Some("medium".to_string()),
            thumbnail_aspect_ratio: Some("contain".to_string()),
            ai_provider: Some("cpu".to_string()),
            adjustment_visibility: default_adjustment_visibility(),
            open_tree_sections: default_open_tree_sections(),
            copy_paste_settings: CopyPasteSettings::default(),
            raw_highlight_compression: Some(2.5),
            processing_backend: Some("auto".to_string()),
            linux_gpu_optimization: Some(false),
            linux_gpu_optimization_migrated_v1: Some(true),
            library_view_mode: Some("flat".to_string()),
            export_presets: default_export_presets(),
            my_lenses: Some(Vec::new()),
            #[cfg(target_os = "android")]
            high_res_zoom_multiplier: Some(0.75),
            #[cfg(not(target_os = "android"))]
            high_res_zoom_multiplier: Some(1.0),
            enable_folder_image_counts: Some(false),
            display_edit_icon: Some(true),
            linear_raw_mode: default_linear_raw_mode(),
            enable_xmp_sync: Some(true),
            create_xmp_if_missing: Some(false),
            xmp_profile_roots: Vec::new(),
            is_waveform_visible: Some(false),
            waveform_height: Some(220),
            active_waveform_channel: Some("luma".to_string()),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            use_wgpu_renderer: Some(false),
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            use_wgpu_renderer: Some(true),
            canvas_input_mode: Some("mouse".to_string()),
            zoom_speed_multiplier: Some(1.0),
            zoom_photo_to_pixel_click: Some(false),
            keybinds: HashMap::new(),
            #[cfg(target_os = "android")]
            thumbnail_worker_threads: Some(2),
            #[cfg(not(target_os = "android"))]
            thumbnail_worker_threads: Some(4),
            #[cfg(target_os = "android")]
            image_cache_size: Some(2),
            #[cfg(not(target_os = "android"))]
            image_cache_size: Some(5),
            tonemapper_override_enabled: Some(false),
            default_raw_tonemapper: Some("agx".to_string()),
            default_non_raw_tonemapper: Some("basic".to_string()),
            enable_focus_mode: Some(false),
            folder_icons: Some(HashMap::new()),
            raw_preprocessing_color_nr: Some(0.5),
            raw_preprocessing_sharpening: Some(0.35),
            apply_preprocessing_to_non_raws: Some(false),
            exif_overlay: Some("off".to_string()),
            language: Some("en".to_string()),
            folder_tree_sort: Some(FolderTreeSort::default()),
            library_display_mode: Some("grid".to_string()),
            grouping: Some("off".to_string()),
            require_matching_exif: Some(false),
            group_edited_files: Some(true),
            group_associated_files: Some(false),
            group_preferred_type: Some("raw".to_string()),
            always_decode_raw_thumbnails: Some(false),
            adobe_comparison_mode: Some(false),
            adobe_camera_profile_path: None,
            workspace: WorkspaceState::default(),
        }
    }
}

pub fn get_settings_path(app_handle: &AppHandle) -> Result<PathBuf, String> {
    let settings_dir = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?;

    if !settings_dir.exists() {
        fs::create_dir_all(&settings_dir).map_err(|e| e.to_string())?;
    }

    Ok(settings_dir.join("settings.json"))
}

#[tauri::command]
pub fn is_tethering_supported() -> bool {
    cfg!(feature = "tethering")
}

/// Brings a loaded settings file up to date with the copy/paste keys this build
/// knows about.
///
/// A saved file with no recorded key list predates the mechanism entirely, so it
/// is seeded with the current defaults. Otherwise, every key this build knows
/// that the saved file did not is recorded as known, and is added to the user's
/// included set only when this build includes it by default — so an existing
/// user gains a newly shipped key automatically, while a user who deliberately
/// unchecked it keeps it unchecked. Returns whether `settings` changed.
pub(crate) fn migrate_copy_paste_settings(settings: &mut AppSettings) -> bool {
    let all_current_keys = all_available_adjustments();
    let default_included = default_included_adjustments();

    if settings.copy_paste_settings.known_adjustments.is_empty() {
        settings.copy_paste_settings.included_adjustments = default_included;
        settings.copy_paste_settings.known_adjustments = all_current_keys;
        return true;
    }

    let new_features: Vec<String> = all_current_keys
        .difference(&settings.copy_paste_settings.known_adjustments)
        .cloned()
        .collect();

    if new_features.is_empty() {
        return false;
    }

    for feature in new_features {
        if default_included.contains(&feature) {
            settings
                .copy_paste_settings
                .included_adjustments
                .insert(feature.clone());
        }
        settings
            .copy_paste_settings
            .known_adjustments
            .insert(feature);
    }

    true
}

#[tauri::command]
pub fn load_settings(app_handle: AppHandle) -> Result<AppSettings, String> {
    let path = get_settings_path(&app_handle)?;

    let mut settings: AppSettings = if path.exists() {
        let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        AppSettings::default()
    };

    let mut settings_modified = false;

    if settings.root_folders.is_empty()
        && let Some(last) = &settings.last_root_path
    {
        settings.root_folders.push(last.clone());
        settings_modified = true;
    }

    #[cfg(target_os = "linux")]
    if !settings.linux_gpu_optimization_migrated_v1.unwrap_or(false) {
        if settings.linux_gpu_optimization == Some(true) {
            settings.linux_gpu_optimization = Some(false);
        }
        settings.linux_gpu_optimization_migrated_v1 = Some(true);
        settings_modified = true;
    }

    if migrate_copy_paste_settings(&mut settings) {
        settings_modified = true;
    }

    if settings_modified && let Ok(json_string) = serde_json::to_string_pretty(&settings) {
        let _ = fs::write(&path, json_string);
    }

    Ok(settings)
}

#[tauri::command]
pub fn save_settings(settings: AppSettings, app_handle: AppHandle) -> Result<(), String> {
    let path = get_settings_path(&app_handle)?;
    let json_string = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    fs::write(path, json_string).map_err(|e| e.to_string())?;

    let state = app_handle.state::<AppState>();
    let cache_size = settings.image_cache_size.unwrap_or(5) as usize;
    state
        .decoded_image_cache
        .lock()
        .unwrap()
        .set_capacity(cache_size);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Settings written before the profile root list existed must keep loading,
    /// with the list defaulting to empty.
    #[test]
    fn old_settings_without_xmp_profile_roots_still_load() {
        let old = r#"{
            "lastRootPath": "/photos",
            "rootFolders": ["/photos"],
            "theme": "light",
            "thumbnailSize": "large",
            "language": "de"
        }"#;

        let settings: AppSettings =
            serde_json::from_str(old).expect("an older settings file must still deserialize");

        assert!(settings.xmp_profile_roots.is_empty());
        assert_eq!(settings.theme.as_deref(), Some("light"));
        assert_eq!(settings.root_folders, vec!["/photos".to_string()]);
        assert_eq!(settings.language.as_deref(), Some("de"));
    }

    /// The persisted root list survives a save/load cycle unchanged.
    #[test]
    fn xmp_profile_roots_roundtrip_exactly() {
        let roots = vec!["/profiles/a".to_string(), "/profiles/b".to_string()];
        let settings = AppSettings {
            xmp_profile_roots: roots.clone(),
            ..AppSettings::default()
        };

        let serialized = serde_json::to_string(&settings).expect("serialize settings");
        assert!(serialized.contains("\"xmpProfileRoots\""));

        let reloaded: AppSettings =
            serde_json::from_str(&serialized).expect("deserialize settings");
        assert_eq!(reloaded.xmp_profile_roots, roots);
    }

    /// Settings written before the Adobe comparison mode existed must keep
    /// loading, with the comparison pipeline defaulting to OFF.
    #[test]
    fn old_settings_without_adobe_comparison_still_load() {
        let settings: AppSettings =
            serde_json::from_str(r#"{ "lastRootPath": "/photos", "theme": "dark" }"#)
                .expect("old settings must deserialize");
        assert_eq!(settings.adobe_comparison_mode, None);
        assert!(!settings.adobe_comparison_enabled());
    }

    #[test]
    fn adobe_comparison_mode_roundtrips_and_enables() {
        let settings = AppSettings {
            adobe_comparison_mode: Some(true),
            ..AppSettings::default()
        };
        let serialized = serde_json::to_string(&settings).expect("serialize settings");
        assert!(serialized.contains("\"adobeComparisonMode\":true"));

        let reloaded: AppSettings =
            serde_json::from_str(&serialized).expect("deserialize settings");
        assert!(reloaded.adobe_comparison_enabled());
    }

    /// The copy/paste adjustment sets are `HashSet`s, so their arrays come back
    /// in a different order on every run; sorting them keeps the comparison
    /// about content rather than about hash order.
    fn normalize_hashed_arrays(value: &mut Value) {
        for key in ["includedAdjustments", "knownAdjustments"] {
            if let Some(Value::Array(items)) =
                value.pointer_mut(&format!("/copyPasteSettings/{key}"))
            {
                items.sort_by_key(|item| item.as_str().map(str::to_string));
            }
        }
    }

    /// The frontend saves the whole settings object with only the root list
    /// changed, so nothing else may be lost or rewritten in the process.
    #[test]
    fn changing_roots_preserves_every_unrelated_field() {
        let partial = r#"{
            "lastRootPath": "/photos",
            "rootFolders": ["/photos", "/photos/2024"],
            "theme": "light",
            "fontFamily": "Inter",
            "language": "de",
            "thumbnailSize": "large",
            "sortCriteria": { "key": "modified", "order": "desc" },
            "keybinds": { "copy": ["mod+c"] },
            "exportPresets": [],
            "xmpProfileRoots": ["/profiles/a"]
        }"#;

        let loaded: AppSettings = serde_json::from_str(partial).expect("load settings");
        let full = serde_json::to_string(&loaded).expect("serialize settings");

        let mut updated: AppSettings = serde_json::from_str(&full).expect("reload settings");
        updated.xmp_profile_roots.push("/profiles/b".to_string());
        let saved = serde_json::to_string(&updated).expect("serialize settings");

        let mut before: Value = serde_json::from_str(&full).expect("parse json");
        let mut after: Value = serde_json::from_str(&saved).expect("parse json");

        let roots_before = before
            .as_object_mut()
            .expect("settings object")
            .remove("xmpProfileRoots")
            .expect("root list");
        let roots_after = after
            .as_object_mut()
            .expect("settings object")
            .remove("xmpProfileRoots")
            .expect("root list");

        assert_eq!(roots_before, json!(["/profiles/a"]));
        assert_eq!(roots_after, json!(["/profiles/a", "/profiles/b"]));

        normalize_hashed_arrays(&mut before);
        normalize_hashed_arrays(&mut after);

        assert_eq!(
            before, after,
            "saving the root list must leave every other setting untouched"
        );
    }

    /// Every copy/paste key this build knows except the 6C4 profile marker: the
    /// exact key list a settings file written before 6C4 recorded.
    fn pre_xmp_profile_known_keys() -> Vec<String> {
        all_available_adjustments()
            .into_iter()
            .filter(|key| key != "xmpProfile")
            .collect()
    }

    #[test]
    fn all_available_adjustments_contains_the_xmp_profile_marker() {
        assert!(
            all_available_adjustments().contains("xmpProfile"),
            "the copy/paste profile marker must be a key this build knows"
        );
    }

    #[test]
    fn xmp_profile_marker_is_included_by_default() {
        assert!(
            default_included_adjustments().contains("xmpProfile"),
            "a newly shipped copy/paste category is included unless it is explicitly off by default"
        );
    }

    /// The load-bearing migration: an existing user's settings file, whose known
    /// key list predates the profile marker, must gain the marker on load.
    #[test]
    fn an_existing_settings_file_gains_the_xmp_profile_marker_on_load() {
        let mut settings: AppSettings = serde_json::from_value(json!({
            "copyPasteSettings": {
                "mode": "merge",
                "includedAdjustments": ["exposure", "contrast"],
                "knownAdjustments": pre_xmp_profile_known_keys(),
                "autoSync": false,
            }
        }))
        .expect("an older settings file must still deserialize");

        // Precondition: the saved file is exactly the pre-6C4 state.
        assert!(
            !settings
                .copy_paste_settings
                .included_adjustments
                .contains("xmpProfile")
        );
        assert!(
            !settings
                .copy_paste_settings
                .known_adjustments
                .contains("xmpProfile")
        );

        assert!(migrate_copy_paste_settings(&mut settings));

        assert!(
            settings
                .copy_paste_settings
                .included_adjustments
                .contains("xmpProfile"),
            "an existing user gains the profile marker on load: {:?}",
            settings.copy_paste_settings.included_adjustments
        );
        assert!(
            settings
                .copy_paste_settings
                .known_adjustments
                .contains("xmpProfile"),
            "the marker is recorded as known so the next load is a no-op"
        );
        // The user's own subset survives: what they included stays in, and a key
        // they never included is not resurrected by the migration.
        assert!(
            settings
                .copy_paste_settings
                .included_adjustments
                .contains("exposure")
        );
        assert!(
            settings
                .copy_paste_settings
                .included_adjustments
                .contains("contrast")
        );
        assert!(
            !settings
                .copy_paste_settings
                .included_adjustments
                .contains("masks"),
            "a key the user deliberately left out must not be added back"
        );
    }

    /// Non-vacuity for the test above: the migration's "on by default" filter is
    /// what adds the marker, so a key that IS off by default is not added.
    #[test]
    fn an_off_by_default_key_is_not_auto_included_by_the_migration() {
        let mut settings: AppSettings = serde_json::from_value(json!({
            "copyPasteSettings": {
                "mode": "merge",
                "includedAdjustments": [],
                "knownAdjustments": all_available_adjustments()
                    .into_iter()
                    .filter(|key| key != "xmpProfile" && key != "guidedPerspective")
                    .collect::<Vec<String>>(),
                "autoSync": false,
            }
        }))
        .expect("settings must deserialize");

        assert!(migrate_copy_paste_settings(&mut settings));

        let included = &settings.copy_paste_settings.included_adjustments;
        assert!(
            included.contains("xmpProfile"),
            "the on-by-default marker is added: {included:?}"
        );
        assert!(
            !included.contains("guidedPerspective"),
            "an off-by-default key is never auto-included by the migration: {included:?}"
        );

        // Both are recorded as known regardless of the default, so re-running the
        // migration changes nothing.
        let known = &settings.copy_paste_settings.known_adjustments;
        assert!(known.contains("xmpProfile") && known.contains("guidedPerspective"));
        assert!(
            !migrate_copy_paste_settings(&mut settings),
            "a second migration has nothing left to do"
        );
    }

    /// 6C4-R2: a marker the user deliberately UNCHECKED is never resurrected.
    ///
    /// `xmpProfile` is already KNOWN (so it is not a new feature the migration
    /// would introduce) but is absent from the user's included set. The migration
    /// must leave that set exactly as it is.
    ///
    /// Non-vacuity: had the migration inserted every key present in
    /// `known_adjustments`, or matched against `included_adjustments` instead of
    /// `known_adjustments`, the marker would reappear here and both assertions
    /// would fail.
    #[test]
    fn a_known_but_unchecked_on_by_default_key_is_not_resurrected() {
        let mut settings: AppSettings = serde_json::from_value(json!({
            "copyPasteSettings": {
                "mode": "merge",
                "includedAdjustments": ["exposure", "contrast", "saturation"],
                "knownAdjustments": all_available_adjustments(),
                "autoSync": false,
            }
        }))
        .expect("settings must deserialize");

        // Precondition: the marker is known, on by default, and NOT included.
        assert!(
            settings
                .copy_paste_settings
                .known_adjustments
                .contains("xmpProfile")
        );
        assert!(
            !settings
                .copy_paste_settings
                .included_adjustments
                .contains("xmpProfile")
        );

        let before = settings.copy_paste_settings.included_adjustments.clone();

        assert!(
            !migrate_copy_paste_settings(&mut settings),
            "a file that already knows every key has nothing for the migration to do"
        );

        let after = &settings.copy_paste_settings.included_adjustments;
        assert!(
            !after.contains("xmpProfile"),
            "a deliberately unchecked, on-by-default key must never be resurrected: {after:?}"
        );
        assert_eq!(
            after, &before,
            "includedAdjustments must be otherwise unchanged: {after:?}"
        );
    }
}
