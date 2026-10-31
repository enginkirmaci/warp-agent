//! Global per-user locations: everything lives in ~/.warp-agent — settings.json,
//! ui.json and the rolling logs. On first run the legacy ~/.config/opencode-agent
//! files migrate over, so workspaces, pins and settings survive the move.

use std::fs;
use std::path::PathBuf;

pub struct AppPaths;

impl AppPaths {
    pub fn config_dir() -> PathBuf {
        dirs_home().join(".warp-agent")
    }
    pub fn logs_dir() -> PathBuf {
        Self::config_dir().join("logs")
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

    /// Copies a legacy opencode-agent file into ~/.warp-agent when the new copy does not exist yet.
    pub fn migrate_legacy_file(file_name: &str) {
        let target = Self::config_dir().join(file_name);
        if target.exists() {
            return;
        }
        let Some(home) = dirs_home_opt() else { return };
        let legacy = home
            .join(".local")
            .join("share")
            .join("opencode-agent")
            .join(file_name);
        let Ok(metadata) = fs::metadata(&legacy) else {
            return;
        };
        if !metadata.is_file() {
            return;
        }
        if let Err(err) =
            fs::create_dir_all(Self::config_dir()).and_then(|_| fs::copy(&legacy, &target))
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

fn dirs_home() -> PathBuf {
    dirs_home_opt().unwrap_or_else(|| PathBuf::from("."))
}

fn dirs_home_opt() -> Option<PathBuf> {
    // $HOME on unix; Windows only defines USERPROFILE
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}
