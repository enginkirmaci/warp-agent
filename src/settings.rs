//! settings.json (user preferences) + ui.json (client-side chat state).
//! Kept in two files exactly like the C# app: settings churn rarely, ui.json
//! is rewritten on every focus/send/idle.

use crate::paths::AppPaths;
use serde::{Deserialize, Serialize};
use std::fs;

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
pub struct AppSettings {
    #[serde(default = "default_executable")]
    pub open_code_executable: String,
    #[serde(default = "default_dsh_executable")]
    pub dsh_executable: String,
    /// Harness new chats start on (the composer pill can switch per chat).
    #[serde(rename = "DefaultHarness", default)]
    pub default_harness: HarnessKind,
    #[serde(rename = "DefaultModel", default)]
    pub default_model: Option<String>,
    #[serde(rename = "DefaultVariant", default)]
    pub default_variant: Option<String>,
    #[serde(rename = "DefaultVariantHighest", default)]
    pub default_variant_highest: bool,
    #[serde(rename = "DefaultAgent", default)]
    pub default_agent: Option<String>,
    #[serde(rename = "AutoAllowAlways", default)]
    pub auto_allow_always: Option<bool>,
    #[serde(rename = "AutoAllowInstant", default)]
    pub auto_allow_instant: bool,
    #[serde(rename = "AutoAllowDefaultMinSeconds", default = "d1")]
    pub auto_allow_default_min_seconds: i64,
    #[serde(rename = "AutoAllowDefaultMaxSeconds", default = "d15")]
    pub auto_allow_default_max_seconds: i64,
    #[serde(rename = "AutoAllowAwayMinSeconds", default = "d60")]
    pub auto_allow_away_min_seconds: i64,
    #[serde(rename = "AutoAllowAwayMaxSeconds", default = "d600")]
    pub auto_allow_away_max_seconds: i64,
    /// Minutes an opencode workspace server stays up with no busy chat before
    /// the idle sweep stops it (the port closes); 0 keeps servers running.
    #[serde(rename = "ServerIdleStopMin", default = "idle10")]
    pub server_idle_stop_min: i64,
    #[serde(rename = "LunchStartHour", default = "h12")]
    pub lunch_start_hour: i64,
    #[serde(rename = "LunchEndHour", default = "h14")]
    pub lunch_end_hour: i64,
    #[serde(rename = "OffHoursStartHour", default = "h18")]
    pub off_hours_start_hour: i64,
    #[serde(rename = "WorkdayStartHour", default = "h8")]
    pub workday_start_hour: i64,
    #[serde(rename = "FontScale", default)]
    pub font_scale: Option<String>,
    #[serde(rename = "ShowToolTips", default = "tru")]
    pub show_tool_tips: bool,
    /// Turns the family motion off, spinners included; busy words stay.
    #[serde(rename = "ReduceMotion", default)]
    pub reduce_motion: bool,
    /// Master switch: keep every workspace's MCP servers disconnected,
    /// overriding the composer's per-workspace toggle.
    #[serde(rename = "McpDisabled", default)]
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
            open_code_executable: default_executable(),
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
            server_idle_stop_min: 10,
            lunch_start_hour: 12,
            lunch_end_hour: 14,
            off_hours_start_hour: 18,
            workday_start_hour: 8,
            font_scale: None,
            show_tool_tips: true,
            reduce_motion: false,
            mcp_disabled: false,
        }
    }
}

impl AppSettings {
    pub fn load() -> Self {
        AppPaths::migrate_legacy_file("settings.json");
        read_json(AppPaths::settings_file()).unwrap_or_default()
    }

    pub fn save(&self) {
        write_json_pretty(AppPaths::settings_file(), self);
    }
}

/// The last model catalog this app saw, carrying each model's harness and
/// variants so pickers (and the door's client) keep working before a server
/// starts. The released cache was a flat `["provider/model"]` list; that
/// shape still loads as opencode entries with no known variants.
pub fn load_models_cache() -> Vec<crate::oc::ModelInfo> {
    let path = AppPaths::models_cache_file();
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    if let Ok(models) = serde_json::from_str::<Vec<crate::oc::ModelInfo>>(&text) {
        return models;
    }
    match serde_json::from_str::<Vec<String>>(&text) {
        Ok(keys) => keys
            .into_iter()
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
        Err(err) => {
            tracing::warn!(
                "{} is neither a catalog nor a model list ({err}); ignoring it",
                path.display()
            );
            Vec::new()
        }
    }
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
    if !entries.is_empty() {
        write_json_pretty(AppPaths::models_cache_file(), &entries);
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UiState {
    #[serde(rename = "Pins", default)]
    pub pins: std::collections::HashMap<String, bool>,
    /// Favorite workspace folder paths (the station repos list's star). Chats
    /// are kept by pins, not favorites. Legacy files may still carry session
    /// ids here; the store prunes everything that is not a workspace path.
    #[serde(rename = "Favorites", default)]
    pub favorites: std::collections::HashMap<String, bool>,
    #[serde(rename = "Workspaces", default)]
    pub workspaces: Vec<WorkspaceState>,
    #[serde(rename = "ActiveWorkspace", default)]
    pub active_workspace: Option<String>,
    #[serde(rename = "Model", default)]
    pub model: Option<String>,
    #[serde(rename = "Variant", default)]
    pub variant: Option<String>,
    #[serde(rename = "Agent", default)]
    pub agent: Option<String>,
    #[serde(rename = "Folder", default)]
    pub folder: Option<String>,
    #[serde(rename = "AutoAllowAlways", default)]
    pub auto_allow_always: Option<bool>,
    #[serde(rename = "SidebarWidth", default)]
    pub sidebar_width: Option<f64>,
    #[serde(rename = "SidebarVisible", default)]
    pub sidebar_visible: Option<bool>,
    /// Unsent composer text per open chat, keyed by session id; restored when
    /// the chat's tile reopens. Draft-only tiles (no session yet) are not
    /// persisted — the app still never launches with a phantom draft.
    #[serde(rename = "Drafts", default)]
    pub drafts: std::collections::HashMap<String, String>,
    /// Workspace folder paths whose MCP servers the composer toggle keeps
    /// disconnected; re-applied after every server (re)start.
    #[serde(rename = "McpDisabled", default)]
    pub mcp_disabled_ws: Vec<String>,
    /// Session id → harness. A chat keeps the backend it was created with
    /// across restarts, so reopening routes to the right server.
    #[serde(rename = "HarnessBySession", default)]
    pub harness_by_session: std::collections::HashMap<String, HarnessKind>,
    /// Chat ids the user deleted. dsh archives are per-process, so an archived
    /// chat still rides `session/list` after the app restarts; these ids keep
    /// it out of every list instead of letting it reappear.
    #[serde(rename = "DeletedSessions", default)]
    pub deleted_sessions: Vec<String>,
}

impl UiState {
    pub fn load() -> Self {
        AppPaths::migrate_legacy_file("ui.json");
        read_json(AppPaths::ui_state_file()).unwrap_or_default()
    }

    pub fn save(&self) {
        write_json_pretty(AppPaths::ui_state_file(), self);
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct WorkspaceState {
    #[serde(rename = "Path", default)]
    pub path: String,
    #[serde(rename = "Sessions", default)]
    pub sessions: Vec<String>,
    #[serde(rename = "ActiveSession", default)]
    pub active_session: Option<String>,
    /// Chats currently tiled in the grid, in reading order; only the restore
    /// banner's Restore button reopens them — connecting never does.
    #[serde(rename = "OpenSessions", default)]
    pub open_sessions: Vec<String>,
    #[serde(rename = "Collapsed", default)]
    pub collapsed: bool,
}

fn read_json<T: serde::de::DeserializeOwned>(path: std::path::PathBuf) -> Option<T> {
    let text = fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(err) => {
            // move the corrupt file aside: falling back to defaults and then
            // overwriting it would destroy whatever is left of the user's
            // pins/workspaces/drafts with no trace
            tracing::warn!(
                "{} unreadable ({}); moving it aside, using defaults",
                path.display(),
                err
            );
            let _ = fs::rename(&path, path.with_extension("json.bad"));
            None
        }
    }
}

fn write_json_pretty<T: Serialize>(path: std::path::PathBuf, value: &T) {
    // the platform kit's atomic replace (unique temp file + rename): a crash
    // mid-write leaves the old file, never a truncated one
    if let Err(err) = warpui_kit_platform::io::write_json(&path, value) {
        tracing::warn!("could not write {}: {err}", path.display());
    }
}
