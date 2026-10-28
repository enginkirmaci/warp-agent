//! Settings page edit buffers — the SettingsPageViewModel port. Values live in
//! plain strings until Save parses and persists them.

use crate::store::Store;
use crate::theme;
use gpui::Entity;

#[derive(Clone)]
pub struct SettingsBuffers {
    pub open_code_executable: String,
    pub default_model: String,
    pub default_variant: String,
    pub variant_highest: bool,
    pub default_agent: String,
    pub auto_allow: bool,
    pub auto_allow_instant: bool,
    pub show_tool_tips: bool,
    pub mcp_disabled: bool,
    pub auto_allow_default_min: String,
    pub auto_allow_default_max: String,
    pub auto_allow_away_min: String,
    pub auto_allow_away_max: String,
    pub lunch_start: String,
    pub lunch_end: String,
    pub off_hours_start: String,
    pub workday_start: String,
    pub text_compact: bool,
    pub text_normal: bool,
    pub text_large: bool,
    pub accent_key: String,
}

impl SettingsBuffers {
    pub fn from_settings(s: &crate::settings::AppSettings) -> Self {
        let (text_compact, text_normal, text_large) = match theme::normalize_font_scale(s.font_scale.as_deref()) {
            theme::FONT_COMPACT => (true, false, false),
            theme::FONT_LARGE => (false, false, true),
            _ => (false, true, false),
        };
        Self {
            open_code_executable: s.open_code_executable.clone(),
            default_model: s.default_model.clone().unwrap_or_default(),
            default_variant: s.default_variant.clone().unwrap_or_default(),
            variant_highest: s.default_variant_highest,
            default_agent: s.default_agent.clone().unwrap_or_default(),
            auto_allow: s.auto_allow_always.unwrap_or(true),
            auto_allow_instant: s.auto_allow_instant,
            show_tool_tips: s.show_tool_tips,
            mcp_disabled: s.mcp_disabled,
            auto_allow_default_min: s.auto_allow_default_min_seconds.to_string(),
            auto_allow_default_max: s.auto_allow_default_max_seconds.to_string(),
            auto_allow_away_min: s.auto_allow_away_min_seconds.to_string(),
            auto_allow_away_max: s.auto_allow_away_max_seconds.to_string(),
            lunch_start: s.lunch_start_hour.to_string(),
            lunch_end: s.lunch_end_hour.to_string(),
            off_hours_start: s.off_hours_start_hour.to_string(),
            workday_start: s.workday_start_hour.to_string(),
            text_compact,
            text_normal,
            text_large,
            accent_key: theme::normalize_accent(s.accent.as_deref()).key.to_string(),
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
            set.open_code_executable = if self.open_code_executable.trim().is_empty() {
                crate::settings::DEFAULT_EXECUTABLE.into()
            } else {
                self.open_code_executable.trim().to_string()
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
            set.show_tool_tips = self.show_tool_tips;
            set.mcp_disabled = self.mcp_disabled;
            set.auto_allow_default_min_seconds = Self::parse_int(&self.auto_allow_default_min, set.auto_allow_default_min_seconds, 1);
            set.auto_allow_default_max_seconds = Self::parse_int(&self.auto_allow_default_max, set.auto_allow_default_max_seconds, 15);
            set.auto_allow_away_min_seconds = Self::parse_int(&self.auto_allow_away_min, set.auto_allow_away_min_seconds, 60);
            set.auto_allow_away_max_seconds = Self::parse_int(&self.auto_allow_away_max, set.auto_allow_away_max_seconds, 600);
            set.lunch_start_hour = Self::parse_int(&self.lunch_start, set.lunch_start_hour, 12);
            set.lunch_end_hour = Self::parse_int(&self.lunch_end, set.lunch_end_hour, 14);
            set.off_hours_start_hour = Self::parse_int(&self.off_hours_start, set.off_hours_start_hour, 18);
            set.workday_start_hour = Self::parse_int(&self.workday_start, set.workday_start_hour, 8);
            set.font_scale = Some(
                (if self.text_compact {
                    theme::FONT_COMPACT
                } else if self.text_large {
                    theme::FONT_LARGE
                } else {
                    theme::FONT_NORMAL
                })
                .to_string(),
            );
            set.accent = Some(self.accent_key.clone());
            s.apply_saved_settings(cx);
        });
    }
}
