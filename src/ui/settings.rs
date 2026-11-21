//! Settings page edit buffers — the SettingsPageViewModel port. Values live in
//! plain strings until Save parses and persists them.

use crate::store::Store;
use gpui::Entity;
use std::collections::BTreeMap;

/// The harness the Chat defaults page is configuring. A tiny entity because
/// the model picker's data closure outlives a render and must re-scope when
/// the segmented control changes without borrowing the window view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HarnessChoice(pub crate::settings::HarnessKind);

#[derive(Clone)]
pub struct SettingsBuffers {
    /// Per-harness settings keyed by wire id; the Server section renders a row
    /// per registry descriptor and Save writes this map straight back.
    pub harnesses: BTreeMap<String, crate::settings::HarnessEntry>,
    pub opencode_executable: String,
    pub dsh_executable: String,
    pub v2_executable: String,
    pub default_harness: crate::settings::HarnessKind,
    pub default_model: String,
    pub default_variant: String,
    pub variant_highest: bool,
    pub default_agent: String,
    pub auto_allow: bool,
    pub auto_allow_instant: bool,
    pub show_tooltips: bool,
    pub reduce_motion: bool,
    pub mcp_disabled: bool,
    pub chats_per_workspace: String,
    pub auto_allow_default_min: String,
    pub auto_allow_default_max: String,
    pub auto_allow_away_min: String,
    pub auto_allow_away_max: String,
    pub server_idle_min: String,
    pub lunch_start: String,
    pub lunch_end: String,
    pub off_hours_start: String,
    pub workday_start: String,
}

impl SettingsBuffers {
    pub fn harness_entry(&self, id: &str) -> Option<&crate::settings::HarnessEntry> {
        self.harnesses.get(id)
    }

    pub fn harness_enabled(&self, id: &str) -> bool {
        self.harness_entry(id).is_some_and(|entry| entry.enabled)
    }

    /// The harness the buffers currently select.
    pub fn harness(&self) -> crate::settings::HarnessKind {
        self.default_harness
    }

    pub fn from_settings(s: &crate::settings::AppSettings) -> Self {
        Self {
            harnesses: s.harnesses.clone(),
            opencode_executable: s.harness_executable("opencode"),
            dsh_executable: s.harness_executable("dsh"),
            v2_executable: s.harness_executable("opencode-v2"),
            default_harness: s.default_harness,
            default_model: s.default_model.clone().unwrap_or_default(),
            default_variant: s.default_variant.clone().unwrap_or_default(),
            variant_highest: s.default_variant_highest,
            default_agent: s.default_agent.clone().unwrap_or_default(),
            auto_allow: s.auto_allow_always.unwrap_or(true),
            auto_allow_instant: s.auto_allow_instant,
            show_tooltips: s.show_tooltips,
            reduce_motion: s.reduce_motion,
            mcp_disabled: s.mcp_disabled,
            chats_per_workspace: s.sidebar_chats_per_workspace.to_string(),
            auto_allow_default_min: s.auto_allow_default_min_seconds.to_string(),
            auto_allow_default_max: s.auto_allow_default_max_seconds.to_string(),
            auto_allow_away_min: s.auto_allow_away_min_seconds.to_string(),
            auto_allow_away_max: s.auto_allow_away_max_seconds.to_string(),
            server_idle_min: s.server_idle_stop_minutes.to_string(),
            lunch_start: s.lunch_start_hour.to_string(),
            lunch_end: s.lunch_end_hour.to_string(),
            off_hours_start: s.off_hours_start_hour.to_string(),
            workday_start: s.workday_start_hour.to_string(),
        }
    }

    pub fn parse_int(text: &str, current: i64, fallback: i64) -> i64 {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            current
        } else {
            trimmed.parse().unwrap_or(fallback)
        }
    }

    /// Writes the buffers into the shared settings and pushes them into live state.
    pub fn save(&self, store: &Entity<Store>, cx: &mut gpui::App) {
        store.update(cx, |s, cx| {
            let set = &mut s.settings;
            let executable = |text: &str, fallback: &str| {
                let text = text.trim();
                if text.is_empty() {
                    fallback.to_string()
                } else {
                    text.to_string()
                }
            };
            set.harnesses = self.harnesses.clone();
            set.harnesses
                .entry("opencode".into())
                .or_insert_with(|| crate::settings::HarnessEntry::new(true, "opencode"))
                .executable = executable(
                &self.opencode_executable,
                crate::settings::DEFAULT_EXECUTABLE,
            );
            set.harnesses
                .entry("dsh".into())
                .or_insert_with(|| crate::settings::HarnessEntry::new(true, "dsh"))
                .executable = executable(
                &self.dsh_executable,
                crate::settings::DEFAULT_DSH_EXECUTABLE,
            );
            set.harnesses
                .entry("opencode-v2".into())
                .or_insert_with(|| crate::settings::HarnessEntry::new(false, "opencode"))
                .executable = executable(&self.v2_executable, crate::settings::DEFAULT_EXECUTABLE);
            if !crate::settings::HarnessKind::ALL
                .iter()
                .any(|harness| set.harness_enabled(harness.label()))
                && let Some(entry) = set.harnesses.get_mut("opencode")
            {
                entry.enabled = true;
            }
            set.default_harness = if set.harness_enabled(self.default_harness.label()) {
                self.default_harness
            } else {
                set.default_harness_kind()
            };
            let null_if_blank = |v: &String| {
                let t = v.trim();
                (!t.is_empty()).then(|| t.to_string())
            };
            set.default_model = null_if_blank(&self.default_model);
            set.default_variant = null_if_blank(&self.default_variant);
            set.default_variant_highest = self.variant_highest;
            set.default_agent = null_if_blank(&self.default_agent);
            set.auto_allow_always = Some(self.auto_allow);
            set.auto_allow_instant = self.auto_allow_instant;
            set.show_tooltips = self.show_tooltips;
            set.reduce_motion = self.reduce_motion;
            set.mcp_disabled = self.mcp_disabled;
            set.sidebar_chats_per_workspace = Self::parse_int(
                &self.chats_per_workspace,
                set.sidebar_chats_per_workspace,
                crate::settings::DEFAULT_SIDEBAR_CHATS_PER_WORKSPACE,
            )
            .clamp(1, 500);
            set.auto_allow_default_min_seconds = Self::parse_int(
                &self.auto_allow_default_min,
                set.auto_allow_default_min_seconds,
                1,
            );
            set.auto_allow_default_max_seconds = Self::parse_int(
                &self.auto_allow_default_max,
                set.auto_allow_default_max_seconds,
                15,
            );
            set.auto_allow_away_min_seconds = Self::parse_int(
                &self.auto_allow_away_min,
                set.auto_allow_away_min_seconds,
                60,
            );
            set.auto_allow_away_max_seconds = Self::parse_int(
                &self.auto_allow_away_max,
                set.auto_allow_away_max_seconds,
                600,
            );
            set.server_idle_stop_minutes =
                Self::parse_int(&self.server_idle_min, set.server_idle_stop_minutes, 10)
                    .clamp(0, 1440);
            set.lunch_start_hour = Self::parse_int(&self.lunch_start, set.lunch_start_hour, 12);
            set.lunch_end_hour = Self::parse_int(&self.lunch_end, set.lunch_end_hour, 14);
            set.off_hours_start_hour =
                Self::parse_int(&self.off_hours_start, set.off_hours_start_hour, 18);
            set.workday_start_hour =
                Self::parse_int(&self.workday_start, set.workday_start_hour, 8);
            s.apply_saved_settings(cx);
        });
    }
}
