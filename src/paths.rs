//! Global per-user locations: the app's own data lives under `~/.warp/agent`
//! — settings.json, ui.json, the model cache, the General workspace and the
//! door endpoint — and the rolling logs go to the suite's shared
//! `~/.warp/logs`. On first run the legacy `~/.warp-agent` folder moves over
//! (and the older `~/.local/share/opencode-agent` files after it), so
//! workspaces, pins and settings survive the move.

use std::path::PathBuf;
use warpui_kit_platform::paths;

pub struct AppPaths;

impl AppPaths {
    /// The app's own folder, `~/.warp/agent`.
    pub fn config_dir() -> PathBuf {
        paths::app_dir("agent")
    }
    /// The suite's shared logs folder, `~/.warp/logs`.
    pub fn logs_dir() -> PathBuf {
        paths::logs_dir()
    }
    pub fn settings_file() -> PathBuf {
        Self::config_dir().join("settings.json")
    }
    pub fn ui_state_file() -> PathBuf {
        Self::config_dir().join("ui.json")
    }
    pub fn models_cache_file() -> PathBuf {
        Self::config_dir().join("models.cache.json")
    }
    /// Cwd of the built-in General workspace: general-purpose chats that belong
    /// to no project folder. The server still needs a directory; this one is ours.
    pub fn general_workspace_path() -> PathBuf {
        Self::config_dir().join("general")
    }

    /// One-time layout compatibility: the app's data lived in `~/.warp-agent`
    /// before every app moved under the shared `~/.warp` root. The logs go to
    /// the shared folder first, then the rest of the folder is adopted as
    /// `~/.warp/agent`; both steps are no-ops once there is nothing to move.
    pub fn migrate_legacy_layout() {
        paths::adopt_legacy_logs(&legacy_dir().join("logs"));
        paths::adopt_legacy_dir(&legacy_dir(), &Self::config_dir());
    }

    /// Copies a legacy opencode-agent file into `~/.warp/agent` when the new copy does not exist yet.
    pub fn migrate_legacy_file(file_name: &str) {
        let target = Self::config_dir().join(file_name);
        if target.exists() {
            return;
        }
        let legacy = paths::home()
            .join(".local")
            .join("share")
            .join("opencode-agent")
            .join(file_name);
        let Ok(metadata) = std::fs::metadata(&legacy) else {
            return;
        };
        if !metadata.is_file() {
            return;
        }
        if let Err(err) = std::fs::create_dir_all(Self::config_dir())
            .and_then(|_| std::fs::copy(&legacy, &target))
        {
            tracing::warn!("could not migrate legacy {file_name}: {err}");
        } else {
            tracing::info!(
                "migrated {file_name} from the legacy directory to {}",
                Self::config_dir().display()
            );
        }
    }
}

/// The folder this app owned before the suite layout: `~/.warp-agent`.
fn legacy_dir() -> PathBuf {
    paths::home().join(".warp-agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_lives_under_the_suite_root() {
        assert_eq!(AppPaths::config_dir(), paths::suite_root().join("agent"));
        assert_eq!(AppPaths::logs_dir(), paths::suite_root().join("logs"));
        assert!(AppPaths::settings_file().ends_with("settings.json"));
        assert!(AppPaths::general_workspace_path().ends_with("general"));
    }

    #[test]
    fn the_legacy_folder_keeps_its_old_name() {
        assert_eq!(legacy_dir(), paths::home().join(".warp-agent"));
    }
}
