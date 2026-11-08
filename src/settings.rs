//! settings.json (user preferences) + ui.json (client-side chat state).
//! Both are versioned snake_case documents. The pre-versioning PascalCase
//! files are imported once at the edge; the live types never see them.

use crate::paths::AppPaths;
use serde::{Deserialize, Serialize};
use std::fs;
use warpui_kit_platform::config::{Error, KeyMap, Store, rename_legacy_keys};

pub const DEFAULT_EXECUTABLE: &str = "opencode";
pub const DEFAULT_DSH_EXECUTABLE: &str = "dsh";

/// Which backend a chat runs on. Sessions carry their own harness so a
/// workspace can mix opencode and dsh tiles; dsh chats all live on one
/// shared `dsh web` process (dsh sessions carry their own cwd).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum HarnessKind {
    #[default]
    OpenCode,
    Dsh,
}

impl HarnessKind {
    /// Every harness the app can run. The door reports this list, so clients
    /// (DevTools settings) never hardcode what warp-agent supports.
    pub const ALL: [HarnessKind; 2] = [HarnessKind::OpenCode, HarnessKind::Dsh];

    /// The wire id: the settings key and the door's `harness` value.
    pub fn label(self) -> &'static str {
        match self {
            HarnessKind::OpenCode => "opencode",
            HarnessKind::Dsh => "dsh",
        }
    }

    /// What a picker calls the harness; the wire id stays stable even when
    /// the display name is friendlier.
    pub fn option_label(self) -> &'static str {
        match self {
            HarnessKind::OpenCode => "opencode",
            HarnessKind::Dsh => "DeepSeek (dsh)",
        }
    }

    pub fn from_label(label: &str) -> Self {
        match label {
            "dsh" => HarnessKind::Dsh,
            _ => HarnessKind::OpenCode,
        }
    }

    /// Strict wire parse for the door: an unknown id is a client bug, not
    /// something to silently route to opencode.
    pub fn from_wire(label: &str) -> Option<Self> {
        match label {
            "opencode" => Some(HarnessKind::OpenCode),
            "dsh" => Some(HarnessKind::Dsh),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case", default, deny_unknown_fields)]
pub struct AppSettings {
    #[serde(default = "default_executable")]
    pub opencode_executable: String,
    #[serde(default = "default_dsh_executable")]
    pub dsh_executable: String,
    /// Harness new chats start on (the composer pill can switch per chat).
    pub default_harness: HarnessKind,
    pub default_model: Option<String>,
    pub default_variant: Option<String>,
    pub default_variant_highest: bool,
    pub default_agent: Option<String>,
    pub auto_allow_always: Option<bool>,
    pub auto_allow_instant: bool,
    #[serde(default = "d1")]
    pub auto_allow_default_min_seconds: i64,
    #[serde(default = "d15")]
    pub auto_allow_default_max_seconds: i64,
    #[serde(default = "d60")]
    pub auto_allow_away_min_seconds: i64,
    #[serde(default = "d600")]
    pub auto_allow_away_max_seconds: i64,
    /// Minutes an opencode workspace server stays up with no busy chat before
    /// the idle sweep stops it (the port closes); 0 keeps servers running.
    #[serde(default = "idle10")]
    pub server_idle_stop_minutes: i64,
    #[serde(default = "h12")]
    pub lunch_start_hour: i64,
    #[serde(default = "h14")]
    pub lunch_end_hour: i64,
    #[serde(default = "h18")]
    pub off_hours_start_hour: i64,
    #[serde(default = "h8")]
    pub workday_start_hour: i64,
    pub text_size: Option<String>,
    #[serde(default = "tru")]
    pub show_tooltips: bool,
    /// Turns the family motion off, spinners included; busy words stay.
    pub reduce_motion: bool,
    /// Master switch: keep every workspace's MCP servers disconnected,
    /// overriding the composer's per-workspace toggle.
    pub mcp_disabled: bool,
}

fn default_executable() -> String {
    DEFAULT_EXECUTABLE.into()
}
fn default_dsh_executable() -> String {
    DEFAULT_DSH_EXECUTABLE.into()
}
fn d1() -> i64 {
    1
}
fn d15() -> i64 {
    15
}
fn d60() -> i64 {
    60
}
fn d600() -> i64 {
    600
}
fn idle10() -> i64 {
    10
}
fn h12() -> i64 {
    12
}
fn h14() -> i64 {
    14
}
fn h18() -> i64 {
    18
}
fn h8() -> i64 {
    8
}
fn tru() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            opencode_executable: default_executable(),
            dsh_executable: default_dsh_executable(),
            default_harness: HarnessKind::default(),
            default_model: None,
            default_variant: None,
            default_variant_highest: false,
            default_agent: None,
            auto_allow_always: None,
            auto_allow_instant: false,
            auto_allow_default_min_seconds: 1,
            auto_allow_default_max_seconds: 15,
            auto_allow_away_min_seconds: 60,
            auto_allow_away_max_seconds: 600,
            server_idle_stop_minutes: 10,
            lunch_start_hour: 12,
            lunch_end_hour: 14,
            off_hours_start_hour: 18,
            workday_start_hour: 8,
            text_size: None,
            show_tooltips: true,
            reduce_motion: false,
            mcp_disabled: false,
        }
    }
}

impl AppSettings {
    pub fn load() -> Self {
        AppPaths::migrate_legacy_file("settings.json");
        let path = AppPaths::settings_file();
        let store = Store::<AppSettings>::new(path.clone());
        match store.load() {
            Ok(settings) => settings,
            Err(err) if err.needs_import() => {
                let settings = import_settings(&path).unwrap_or_default();
                let _ = store.save(&settings);
                settings
            }
            Err(err) => {
                tracing::warn!("could not read {}: {err}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        if let Err(err) = Store::<AppSettings>::new(AppPaths::settings_file()).save(self) {
            tracing::warn!("could not save settings: {err}");
        }
    }
}

/// The old PascalCase settings file, translated once. The live type is
/// strict; this is the only place that knows the old spelling.
fn import_settings(path: &std::path::Path) -> Option<AppSettings> {
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    rename_legacy_keys(&mut value, &[]).ok()?;
    move_key(&mut value, "open_code_executable", "opencode_executable");
    move_key(&mut value, "font_scale", "text_size");
    move_key(&mut value, "show_tool_tips", "show_tooltips");
    move_key(
        &mut value,
        "server_idle_stop_min",
        "server_idle_stop_minutes",
    );
    serde_json::from_value(value).ok()
}

/// `ui.json` carries user keys inside maps: session ids under `pins`, `drafts`
/// and `harness_by_session`, folder paths under `favorites`. Rename the
/// containers, never the keys inside them.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "snake_case", default)]
pub struct UiState {
    pub pins: KeyMap<bool>,
    /// Favorite workspace folder paths (the station repos list's star). Chats
    /// are kept by pins, not favorites. Legacy files may still carry session
    /// ids here; the store prunes everything that is not a workspace path.
    pub favorites: KeyMap<bool>,
    pub workspaces: Vec<WorkspaceState>,
    pub active_workspace: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub agent: Option<String>,
    pub folder: Option<String>,
    pub auto_allow_always: Option<bool>,
    pub sidebar_width: Option<f64>,
    pub sidebar_visible: Option<bool>,
    /// Unsent composer text per open chat, keyed by session id; restored when
    /// the chat's tile reopens. Draft-only tiles (no session yet) are not
    /// persisted — the app still never launches with a phantom draft.
    pub drafts: KeyMap<String>,
    /// Workspace folder paths whose MCP servers the composer toggle keeps
    /// disconnected; re-applied after every server (re)start.
    pub mcp_disabled_workspaces: Vec<String>,
    /// Session id → harness. A chat keeps the backend it was created with
    /// across restarts, so reopening routes to the right server.
    pub harness_by_session: KeyMap<HarnessKind>,
    /// Chat ids the user deleted. dsh archives are per-process, so an archived
    /// chat still rides `session/list` after the app restarts; these ids keep
    /// it out of every list instead of letting it reappear.
    pub deleted_sessions: Vec<String>,
}

impl UiState {
    pub fn load() -> Self {
        AppPaths::migrate_legacy_file("ui.json");
        let path = AppPaths::ui_state_file();
        let store = Store::<UiState>::new(path.clone());
        match store.load() {
            Ok(ui) => ui,
            Err(err) if err.needs_import() => {
                let ui = import_ui(&path).unwrap_or_default();
                let _ = store.save(&ui);
                ui
            }
            Err(err @ Error::Newer { .. }) => {
                tracing::warn!(
                    "{} was written by a newer build ({err}); leaving it alone",
                    path.display()
                );
                Self::default()
            }
            Err(err) => {
                // move the corrupt file aside: falling back to defaults and
                // then overwriting it would destroy whatever is left of the
                // user's pins/workspaces/drafts with no trace
                tracing::warn!(
                    "{} unreadable ({}); moving it aside, using defaults",
                    path.display(),
                    err
                );
                let _ = fs::rename(&path, path.with_extension("json.bad"));
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        if let Err(err) = Store::<UiState>::new(AppPaths::ui_state_file()).save(self) {
            tracing::warn!("could not save ui state: {err}");
        }
    }
}

fn import_ui(path: &std::path::Path) -> Option<UiState> {
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    rename_legacy_keys(
        &mut value,
        &["pins", "favorites", "drafts", "harness_by_session"],
    )
    .ok()?;
    move_key(&mut value, "mcp_disabled", "mcp_disabled_workspaces");
    serde_json::from_value(value).ok()
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "snake_case", default)]
pub struct WorkspaceState {
    pub path: String,
    pub sessions: Vec<String>,
    pub active_session: Option<String>,
    /// Chats currently tiled in the grid, in reading order; only the restore
    /// banner's Restore button reopens them — connecting never does.
    pub open_sessions: Vec<String>,
    pub collapsed: bool,
}

/// Rename one top-level key when the target is absent. The old file's keys
/// already went through [`rename_legacy_keys`], so both use snake spelling.
fn move_key(value: &mut serde_json::Value, old: &str, new: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let Some(moved) = object.remove(old) else {
        return;
    };
    if !object.contains_key(new) {
        object.insert(new.to_string(), moved);
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct ModelsCache {
    models: Vec<crate::oc::ModelInfo>,
}

/// The last model catalog this app saw, carrying each model's harness and
/// variants so pickers (and the door's client) keep working before a server
/// starts. The released cache was a flat `["provider/model"]` list; that
/// shape still loads as opencode entries with no known variants.
pub fn load_models_cache() -> Vec<crate::oc::ModelInfo> {
    let path = AppPaths::models_cache_file();
    let store = Store::<ModelsCache>::new(path.clone());
    match store.load() {
        Ok(cache) => cache.models,
        Err(err) if err.needs_import() => {
            let models = fs::read_to_string(&path)
                .ok()
                .and_then(|text| parse_legacy_models(&text))
                .unwrap_or_default();
            if !models.is_empty() {
                let _ = store.save(&ModelsCache {
                    models: models.clone(),
                });
            }
            models
        }
        Err(err) => {
            tracing::warn!("could not read {}: {err}", path.display());
            Vec::new()
        }
    }
}

fn parse_legacy_models(text: &str) -> Option<Vec<crate::oc::ModelInfo>> {
    if let Ok(models) = serde_json::from_str::<Vec<crate::oc::ModelInfo>>(text) {
        return Some(models);
    }
    let keys = serde_json::from_str::<Vec<String>>(text).ok()?;
    Some(
        keys.into_iter()
            .filter_map(|key| {
                let key = key.trim().to_string();
                (!key.is_empty() && !key.starts_with('(')).then_some(crate::oc::ModelInfo {
                    key,
                    harness: HarnessKind::OpenCode,
                    variants: Vec::new(),
                    context_limit: 0,
                })
            })
            .collect(),
    )
}

pub fn save_models_cache(models: &[crate::oc::ModelInfo]) {
    let mut entries: Vec<crate::oc::ModelInfo> = Vec::with_capacity(models.len());
    for model in models {
        let key = model.key.trim();
        if key.is_empty() || key.starts_with('(') {
            continue;
        }
        if entries
            .iter()
            .any(|known| known.harness == model.harness && known.key == key)
        {
            continue;
        }
        let mut entry = model.clone();
        entry.key = key.to_string();
        entries.push(entry);
    }
    if !entries.is_empty()
        && let Err(err) = Store::<ModelsCache>::new(AppPaths::models_cache_file())
            .save(&ModelsCache { models: entries })
    {
        tracing::warn!("could not save models cache: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "warp-agent-settings-{name}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_old_settings_file_keeps_its_values_under_new_keys() {
        let dir = scratch("settings");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "open_code_executable": "/bin/opencode",
                "DefaultHarness": "dsh",
                "DefaultModel": "opencode-go/x",
                "ServerIdleStopMin": 25,
                "FontScale": "large",
                "ShowToolTips": false,
                "McpDisabled": true
            }"#,
        )
        .unwrap();

        let settings = import_settings(&path).expect("imports");
        assert_eq!(settings.opencode_executable, "/bin/opencode");
        assert_eq!(settings.default_harness, HarnessKind::Dsh);
        assert_eq!(settings.default_model.as_deref(), Some("opencode-go/x"));
        assert_eq!(settings.server_idle_stop_minutes, 25);
        assert_eq!(settings.text_size.as_deref(), Some("large"));
        assert!(!settings.show_tooltips);
        assert!(settings.mcp_disabled);

        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("\"opencode_executable\""));
        assert!(json.contains("\"text_size\""));
        assert!(json.contains("\"show_tooltips\""));
        assert!(!json.contains("DefaultHarness"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_old_ui_file_keeps_opaque_map_keys() {
        let dir = scratch("ui");
        let path = dir.join("ui.json");
        std::fs::write(
            &path,
            r#"{
                "Pins": {"ses_UPPER": true},
                "Favorites": {"/mnt/Repo": true},
                "Drafts": {"ses_DRAFT": "hello"},
                "HarnessBySession": {"ses_HARNESS": "dsh"},
                "McpDisabled": ["/mnt/Repo"],
                "Workspaces": [{"Path": "/mnt/Repo", "Sessions": ["ses_UPPER"]}]
            }"#,
        )
        .unwrap();

        let ui = import_ui(&path).expect("imports");
        assert_eq!(ui.pins.get("ses_UPPER"), Some(&true));
        assert_eq!(ui.favorites.get("/mnt/Repo"), Some(&true));
        assert_eq!(
            ui.drafts.get("ses_DRAFT").map(String::as_str),
            Some("hello")
        );
        assert_eq!(
            ui.harness_by_session.get("ses_HARNESS"),
            Some(&HarnessKind::Dsh)
        );
        assert_eq!(ui.mcp_disabled_workspaces, vec!["/mnt/Repo"]);
        assert_eq!(ui.workspaces[0].path, "/mnt/Repo");
        assert_eq!(ui.workspaces[0].sessions, vec!["ses_UPPER"]);

        let json = serde_json::to_string(&ui).unwrap();
        assert!(json.contains("\"pins\""));
        assert!(json.contains("\"ses_UPPER\""));
        assert!(json.contains("\"mcp_disabled_workspaces\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_flat_model_list_loads_as_opencode_entries() {
        let models = parse_legacy_models(r#"["a/x", "b/y", ""]"#).expect("legacy list");
        assert_eq!(models.len(), 2);
        assert!(
            models
                .iter()
                .all(|model| model.harness == HarnessKind::OpenCode)
        );
        assert!(models.iter().all(|model| model.variants.is_empty()));
    }
}
