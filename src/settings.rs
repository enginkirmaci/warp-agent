//! settings.json (user preferences) + ui.json (client-side chat state).
//! Kept in two files exactly like the C# app: settings churn rarely, ui.json
//! is rewritten on every focus/send/idle.

use crate::paths::AppPaths;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;

pub const DEFAULT_EXECUTABLE: &str = "opencode";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppSettings {
    #[serde(default = "default_executable")]
    pub open_code_executable: String,
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
    #[serde(rename = "Accent", default)]
    pub accent: Option<String>,
    #[serde(rename = "ShowToolTips", default = "tru")]
    pub show_tool_tips: bool,
    /// Master switch: keep every workspace's MCP servers disconnected,
    /// overriding the composer's per-workspace toggle.
    #[serde(rename = "McpDisabled", default)]
    pub mcp_disabled: bool,
}

fn default_executable() -> String {
    DEFAULT_EXECUTABLE.into()
}
fn d1() -> i64 { 1 }
fn d15() -> i64 { 15 }
fn d60() -> i64 { 60 }
fn d600() -> i64 { 600 }
fn h12() -> i64 { 12 }
fn h14() -> i64 { 14 }
fn h18() -> i64 { 18 }
fn h8() -> i64 { 8 }
fn tru() -> bool { true }

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            open_code_executable: default_executable(),
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
            lunch_start_hour: 12,
            lunch_end_hour: 14,
            off_hours_start_hour: 18,
            workday_start_hour: 8,
            font_scale: None,
            accent: None,
            show_tool_tips: true,
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

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UiState {
    #[serde(rename = "Pins", default)]
    pub pins: std::collections::HashMap<String, bool>,
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
            tracing::warn!("{} unreadable ({}); falling back to defaults", path.display(), err);
            None
        }
    }
}

fn write_json_pretty<T: Serialize>(path: std::path::PathBuf, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(value) {
        Ok(text) => {
            let tmp = path.with_extension("json.tmp");
            if fs::File::create(&tmp)
                .and_then(|mut f| f.write_all(text.as_bytes()))
                .is_ok()
            {
                let _ = fs::rename(&tmp, &path);
            }
        }
        Err(_) => {}
    }
}
