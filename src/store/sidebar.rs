use super::*;

// ---- sidebar models ------------------------------------------------------

/// Sidebar display name for a workspace folder: "Tasks" for the built-in
/// workspace (general-purpose chats; it leads the sidebar as a plain task
/// entry), otherwise the folder's basename.
fn workspace_display_name(path: &str) -> String {
    if path == crate::paths::AppPaths::general_workspace_path().to_string_lossy() {
        return "Tasks".to_string();
    }
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Sort key: Tasks leads, then favorites, the rest alphabetical by name
/// (case-insensitive). Single-sourced so every workspace-list sort keeps
/// Tasks on top.
pub(super) fn workspace_order_key(
    path: &str,
    favorites: &std::collections::HashMap<String, bool>,
) -> (bool, bool, String) {
    let general = path == crate::paths::AppPaths::general_workspace_path().to_string_lossy();
    (
        !general,
        !favorites.contains_key(path),
        workspace_display_name(path).to_lowercase(),
    )
}

/// Sidebar order: Tasks leads, then favorites, the rest alphabetical.
pub(super) fn sort_workspaces(
    workspaces: &mut [WorkspaceState],
    favorites: &std::collections::HashMap<String, bool>,
) {
    workspaces.sort_by_key(|w| workspace_order_key(&w.path, favorites));
}

#[derive(Clone)]
pub struct WorkspaceItem {
    pub state: WorkspaceState,
    pub name: String,
    pub is_running: bool,
    pub is_selected: bool,
    pub is_favorite: bool,
    pub chats: Vec<crate::store::SessionItem>,
}

impl WorkspaceItem {
    pub fn new(state: WorkspaceState, favorites: &std::collections::HashMap<String, bool>) -> Self {
        let name = workspace_display_name(&state.path);
        let is_favorite = favorites.contains_key(&state.path);
        Self {
            state,
            name,
            is_running: false,
            is_selected: false,
            is_favorite,
            chats: Vec::new(),
        }
    }

    pub fn folder_path(&self) -> &str {
        &self.state.path
    }

    pub fn is_general(&self) -> bool {
        self.state.path == crate::paths::AppPaths::general_workspace_path().to_string_lossy()
    }

    pub fn is_expanded(&self) -> bool {
        !self.state.collapsed
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.state.collapsed = !expanded;
    }

    pub fn has_chats(&self) -> bool {
        !self.chats.is_empty()
    }

    pub fn show_empty_hint(&self) -> bool {
        self.is_running && self.chats.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct SessionItem {
    pub id: String,
    pub title: String,
    pub updated_ms: i64,
    pub created_ms: i64,
    pub is_pinned: bool,
    pub is_selected: bool,
    pub is_running: bool,
    pub is_editing: bool,
    pub draft_title: String,
}

impl SessionItem {
    pub fn updated_label(&self) -> String {
        relative_time_label(self.updated_ms)
    }
    pub fn pin_visible(&self) -> bool {
        self.is_pinned && !self.is_running
    }
}

/// Sidebar chat order: pinned first, then newest first — by last activity,
/// falling back to creation time when a server never fills `updated`.
pub(super) fn session_recency(s: &SessionItem) -> std::cmp::Reverse<(bool, i64, i64)> {
    std::cmp::Reverse((s.is_pinned, s.updated_ms, s.created_ms))
}
