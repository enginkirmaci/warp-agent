//! The composer's worktree toggle at the store seam: the toggle is offered only
//! where warp-agent knows the workspace is a repository, and the first send
//! creates the worktree before it creates a session.
//!
//! The git call itself lives in `warpui-kit-worktree` and is tested there
//! against real repositories; here the net thread's answer and the creation
//! result arrive as the same events it sends in the app, so the probe covers
//! the state machine and not a fake git.

use gpui::TestAppContext;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use warp_agent::store::{ChatItem, SessionItem, Store, UiEvent, WorktreeView};

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// A scratch home so `Store::new` neither reads nor writes the developer's
/// `~/.warp/agent`; the scenarios take turns behind `HOME_LOCK`.
struct HomeGuard {
    home: PathBuf,
    old_home: Option<OsString>,
    old_profile: Option<OsString>,
}

impl HomeGuard {
    fn new(name: &str) -> Self {
        let home =
            std::env::temp_dir().join(format!("warp-agent-probe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let old_home = std::env::var_os("HOME");
        let old_profile = std::env::var_os("USERPROFILE");
        // edition 2024 marks the process environment as unsafe to mutate
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("USERPROFILE", &home);
        }
        Self {
            home,
            old_home,
            old_profile,
        }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.old_profile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn notes(store: &Store, key: &str) -> Vec<(String, bool)> {
    store
        .tile_by_key(key)
        .map(|tile| {
            tile.items
                .iter()
                .filter_map(|item| match item.as_ref() {
                    ChatItem::SystemNote { text, is_error, .. } => {
                        Some((text.to_string(), *is_error))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn setup(cx: &mut TestAppContext, name: &str) -> (HomeGuard, PathBuf, gpui::Entity<Store>) {
    let home = HomeGuard::new(name);
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });
    let workspace = home.home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace folder");
    let store = cx.update(Store::for_probe);
    let folder = workspace.to_string_lossy().to_string();
    store.update(cx, |store, cx| store.add_workspace(&folder, cx));
    (home, workspace, store)
}

#[gpui_kit::gpui::test]
fn the_toggle_is_offered_only_where_the_workspace_is_a_repository(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (home, workspace, store) = setup(cx, "worktree-toggle");
    let folder = workspace.to_string_lossy().to_string();
    let key = store.update(cx, |store, cx| {
        store.new_chat(cx);
        store.focused_tile.clone().expect("a focused draft")
    });

    // no answer yet: the toggle stays hidden
    let view: WorktreeView = store.read_with(cx, |store, _| store.worktree_view(&key));
    assert!(!view.visible);

    store.update(cx, |store, cx| {
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: folder.clone(),
                repository: Some(folder.clone()),
                worktrees: Vec::new(),
            },
            cx,
        );
    });
    let view = store.read_with(cx, |store, _| store.worktree_view(&key));
    assert!(view.visible, "a repository workspace offers the toggle");

    // a workspace that is not a repository: the same draft's toggle disappears
    let plain = home.home.join("plain");
    std::fs::create_dir_all(&plain).expect("plain folder");
    let plain_folder = plain.to_string_lossy().to_string();
    store.update(cx, |store, cx| {
        store.add_workspace(&plain_folder, cx);
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: plain_folder.clone(),
                repository: None,
                worktrees: Vec::new(),
            },
            cx,
        );
        store.new_chat(cx);
    });
    let plain_key = store.read_with(cx, |store, _| {
        store.focused_tile.clone().expect("a focused draft")
    });
    let view = store.read_with(cx, |store, _| store.worktree_view(&plain_key));
    assert!(!view.visible, "a plain folder offers no worktree");
}

#[gpui_kit::gpui::test]
fn the_toggle_names_the_branch_after_the_chat(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_home, workspace, store) = setup(cx, "worktree-branch");
    let folder = workspace.to_string_lossy().to_string();
    let key = store.update(cx, |store, cx| {
        store.new_chat(cx);
        let key = store.focused_tile.clone().expect("a focused draft");
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: folder.clone(),
                repository: Some(folder.clone()),
                worktrees: Vec::new(),
            },
            cx,
        );
        if let Some(tile) = store.tile_by_key_mut(&key) {
            tile.input = "Fix the login retry".into();
        }
        store.toggle_worktree(&key, cx);
        key
    });

    let view = store.read_with(cx, |store, _| store.worktree_view(&key));
    assert!(view.enabled);
    assert_eq!(view.branch, "agent/fix-the-login-retry");
    assert!(!view.locked, "a draft keeps the branch editable");

    // the field's edits are what git will use
    store.update(cx, |store, _| {
        store.set_worktree_branch(&key, "agent/renamed".into());
    });
    let view = store.read_with(cx, |store, _| store.worktree_view(&key));
    assert_eq!(view.branch, "agent/renamed");

    // a second click turns it off and leaves the branch remembered
    store.update(cx, |store, cx| store.toggle_worktree(&key, cx));
    let view = store.read_with(cx, |store, _| store.worktree_view(&key));
    assert!(!view.enabled);
    assert_eq!(view.branch, "agent/renamed");
}

#[gpui_kit::gpui::test]
fn the_first_send_creates_the_worktree_before_the_session(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_home, workspace, store) = setup(cx, "worktree-send");
    let folder = workspace.to_string_lossy().to_string();
    let key = store.update(cx, |store, cx| {
        store.new_chat(cx);
        let key = store.focused_tile.clone().expect("a focused draft");
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: folder.clone(),
                repository: Some(folder.clone()),
                worktrees: Vec::new(),
            },
            cx,
        );
        if let Some(tile) = store.tile_by_key_mut(&key) {
            tile.input = "Fix the login retry".into();
        }
        store.toggle_worktree(&key, cx);
        store.send_from_tile(&key, cx);
        key
    });

    let (session, input, pending) = store.read_with(cx, |store, _| {
        let tile = store.tile_by_key(&key).expect("the draft");
        (
            tile.session_id.clone(),
            tile.input.clone(),
            tile.worktree.as_ref().map(|worktree| {
                (
                    worktree.creating,
                    worktree.folder.clone(),
                    worktree.branch.clone(),
                )
            }),
        )
    });
    assert!(session.is_none(), "no session before the worktree exists");
    assert_eq!(
        input, "Fix the login retry",
        "the send stays in the composer"
    );
    let (creating, folder_at, branch) = pending.expect("a worktree on the draft");
    assert!(creating);
    assert!(folder_at.is_none());
    assert_eq!(branch, "agent/fix-the-login-retry");
    let system_notes = store.read_with(cx, |store, _| notes(store, &key));
    assert_eq!(system_notes.len(), 1, "{system_notes:?}");
    assert!(!system_notes[0].1, "a creation note is not an error");

    // a second press while creation is in flight starts nothing new
    store.update(cx, |store, cx| store.send_from_tile(&key, cx));
    let system_notes = store.read_with(cx, |store, _| notes(store, &key));
    assert_eq!(system_notes.len(), 1, "no second creation note");
}

#[gpui_kit::gpui::test]
fn a_created_worktree_becomes_the_chats_workspace(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_home, workspace, store) = setup(cx, "worktree-created");
    let folder = workspace.to_string_lossy().to_string();
    let worktree_folder = workspace
        .join(".worktrees")
        .join("agent-fix-the-login-retry");
    std::fs::create_dir_all(&worktree_folder).expect("worktree folder");
    let worktree_folder = worktree_folder.to_string_lossy().to_string();
    let key = store.update(cx, |store, cx| {
        store.new_chat(cx);
        let key = store.focused_tile.clone().expect("a focused draft");
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: folder.clone(),
                repository: Some(folder.clone()),
                worktrees: Vec::new(),
            },
            cx,
        );
        if let Some(tile) = store.tile_by_key_mut(&key) {
            tile.input = "Fix the login retry".into();
        }
        store.toggle_worktree(&key, cx);
        store.send_from_tile(&key, cx);
        store.on_net_event(
            UiEvent::WorktreeCreated {
                tile_key: key.clone(),
                folder: worktree_folder.clone(),
                branch: "agent/fix-the-login-retry".into(),
            },
            cx,
        );
        key
    });

    let (tile_folder, creating, installed) = store.read_with(cx, |store, _| {
        let tile = store.tile_by_key(&key).expect("the draft");
        let state = tile.worktree.as_ref().expect("a worktree on the draft");
        (
            state.folder.clone(),
            state.creating,
            store
                .workspaces
                .iter()
                .any(|workspace| workspace.folder_path() == worktree_folder),
        )
    });
    assert_eq!(tile_folder.as_deref(), Some(worktree_folder.as_str()));
    assert!(!creating);
    assert!(installed, "the worktree is installed as a workspace");

    let view = store.read_with(cx, |store, _| store.worktree_view(&key));
    assert!(view.enabled, "the chat still shows where it runs");
    assert_eq!(view.branch, "agent/fix-the-login-retry");
}

#[gpui_kit::gpui::test]
fn a_failed_creation_keeps_the_send_in_the_composer(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_home, workspace, store) = setup(cx, "worktree-failed");
    let folder = workspace.to_string_lossy().to_string();
    let key = store.update(cx, |store, cx| {
        store.new_chat(cx);
        let key = store.focused_tile.clone().expect("a focused draft");
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: folder.clone(),
                repository: Some(folder.clone()),
                worktrees: Vec::new(),
            },
            cx,
        );
        if let Some(tile) = store.tile_by_key_mut(&key) {
            tile.input = "Fix the login retry".into();
        }
        store.toggle_worktree(&key, cx);
        store.send_from_tile(&key, cx);
        store.on_net_event(
            UiEvent::WorktreeFailed {
                tile_key: key.clone(),
                error: "fatal: a branch named 'agent/fix-the-login-retry' already exists".into(),
            },
            cx,
        );
        key
    });

    let (creating, pending_send, input) = store.read_with(cx, |store, _| {
        let tile = store.tile_by_key(&key).expect("the draft");
        let state = tile.worktree.as_ref().expect("a worktree on the draft");
        (state.creating, state.pending_send, tile.input.clone())
    });
    assert!(!creating, "the toggle is usable again");
    assert!(!pending_send, "nothing is parked waiting for a server");
    assert_eq!(input, "Fix the login retry");

    let system_notes = store.read_with(cx, |store, _| notes(store, &key));
    assert!(
        system_notes
            .iter()
            .any(|(text, is_error)| *is_error && text.contains("Could not create the worktree")),
        "the failure is reported: {system_notes:?}"
    );
}

#[gpui_kit::gpui::test]
fn the_composer_renders_with_the_worktree_toggle(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_home, workspace, store) = setup(cx, "worktree-render");
    let folder = workspace.to_string_lossy().to_string();
    store.update(cx, |store, cx| {
        store.new_chat(cx);
        let key = store.focused_tile.clone().expect("a focused draft");
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: folder.clone(),
                repository: Some(folder.clone()),
                worktrees: Vec::new(),
            },
            cx,
        );
        if let Some(tile) = store.tile_by_key_mut(&key) {
            tile.input = "Fix the login retry".into();
        }
        store.toggle_worktree(&key, cx);
    });

    // the real window: the toggle and the branch field live in the composer,
    // so a render that reaches them is the check
    let (_window, cx) =
        cx.add_window_view(|_, cx| warp_agent::ui::window::WarpWindow::new(store.clone(), cx));
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
}

#[gpui_kit::gpui::test]
fn a_worktree_lists_its_chats_under_its_repository(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (home, workspace, store) = setup(cx, "worktree-nest");
    let repo = workspace.to_string_lossy().to_string();
    let worktree = workspace.join(".worktrees").join("agent-one");
    std::fs::create_dir_all(&worktree).expect("worktree folder");
    let worktree = worktree.to_string_lossy().to_string();

    store.update(cx, |store, cx| {
        store.add_workspace(&worktree, cx);
        if let Some(item) = store
            .workspaces
            .iter_mut()
            .find(|item| item.folder_path() == worktree)
        {
            item.chats.push(SessionItem {
                id: "worktree-chat".into(),
                title: "A chat in the worktree".into(),
                updated_ms: 1,
                created_ms: 1,
                is_pinned: false,
                is_selected: false,
                is_running: false,
                is_editing: false,
                draft_title: String::new(),
            });
        }
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: repo.clone(),
                repository: Some(repo.clone()),
                worktrees: vec![(worktree.clone(), "agent/one".into())],
            },
            cx,
        );
    });

    let (window, cx) =
        cx.add_window_view(|_, cx| warp_agent::ui::window::WarpWindow::new(store.clone(), cx));
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
    let rows = window.read_with(cx, |window, cx| window.sidebar_rows_for_probe(cx));

    assert!(
        rows.contains(&format!("workspace:{repo}")),
        "the repository keeps its row: {rows:?}"
    );
    assert!(
        !rows.contains(&format!("workspace:{worktree}")),
        "the worktree's own row is hidden: {rows:?}"
    );
    let header = rows
        .iter()
        .position(|row| row == &format!("worktree:{worktree}"))
        .expect("a worktree header");
    let chat = rows
        .iter()
        .position(|row| row == "chat:worktree-chat")
        .expect("the worktree's chat");
    assert!(header < chat, "the header leads its chats: {rows:?}");
    let parent = rows
        .iter()
        .position(|row| row == &format!("workspace:{repo}"))
        .expect("the repository row");
    assert!(
        parent < header,
        "the repository leads its worktrees: {rows:?}"
    );
    let _ = home;
}

#[gpui_kit::gpui::test]
fn a_worktree_chat_installs_its_missing_repository(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("worktree-parent");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });
    let repo = home.home.join("repo");
    let worktree = repo.join(".worktrees").join("agent-one");
    std::fs::create_dir_all(&worktree).expect("worktree folder");
    let repo = repo.to_string_lossy().to_string();
    let worktree = worktree.to_string_lossy().to_string();
    let store = cx.update(Store::for_probe);
    store.update(cx, |store, cx| {
        store.add_workspace(&worktree, cx);
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: repo.clone(),
                repository: Some(repo.clone()),
                worktrees: vec![(worktree.clone(), "agent/one".into())],
            },
            cx,
        );
    });

    let workspaces: Vec<String> = store.read_with(cx, |store, _| {
        store
            .workspaces
            .iter()
            .map(|workspace| workspace.folder_path().to_string())
            .collect()
    });
    assert!(
        workspaces.contains(&repo),
        "the repository is installed so the chat has a home: {workspaces:?}"
    );
    assert!(workspaces.contains(&worktree));
}

#[gpui_kit::gpui::test]
fn a_worktree_offers_no_second_worktree(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (home, workspace, store) = setup(cx, "worktree-no-second");
    let repo = workspace.to_string_lossy().to_string();
    let worktree = workspace.join(".worktrees").join("agent-one");
    std::fs::create_dir_all(&worktree).expect("worktree folder");
    let worktree = worktree.to_string_lossy().to_string();

    let key = store.update(cx, |store, cx| {
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: repo.clone(),
                repository: Some(repo.clone()),
                worktrees: vec![(worktree.clone(), "agent/one".into())],
            },
            cx,
        );
        store.add_workspace(&worktree, cx);
        store.new_chat(cx);
        store.focused_tile.clone().expect("a focused draft")
    });

    let view: WorktreeView = store.read_with(cx, |store, _| store.worktree_view(&key));
    assert!(
        !view.visible,
        "a chat already in a worktree gets no worktree toggle"
    );
    store.update(cx, |store, cx| store.toggle_worktree(&key, cx));
    let view = store.read_with(cx, |store, _| store.worktree_view(&key));
    assert!(!view.enabled);
    let _ = home;
}

#[gpui_kit::gpui::test]
fn the_sidebar_new_chat_button_reaches_a_worktree(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (home, workspace, store) = setup(cx, "worktree-new-chat");
    let repo = workspace.to_string_lossy().to_string();
    let worktree = workspace.join(".worktrees").join("agent-one");
    std::fs::create_dir_all(&worktree).expect("worktree folder");
    let worktree = worktree.to_string_lossy().to_string();

    store.update(cx, |store, cx| {
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: repo.clone(),
                repository: Some(repo.clone()),
                worktrees: vec![(worktree.clone(), "agent/one".into())],
            },
            cx,
        );
        // nobody opened the checkout: the button opens it and drafts there
        store.new_chat_in_folder(&worktree, cx);
    });

    let (selected, draft_ws, installed) = store.read_with(cx, |store, _| {
        let key = store.focused_tile.clone().expect("a focused draft");
        (
            store.selected_workspace.clone(),
            store
                .tile_by_key(&key)
                .map(|tile| tile.session_id.is_none())
                .unwrap_or(false),
            store
                .workspaces
                .iter()
                .any(|workspace| workspace.folder_path() == worktree),
        )
    });
    assert_eq!(selected.as_deref(), Some(worktree.as_str()));
    assert!(installed, "the checkout becomes a workspace");
    assert!(draft_ws, "the new chat starts as a draft there");
    let _ = home;
}

#[gpui_kit::gpui::test]
fn a_missing_worktree_blocks_the_send_and_offers_a_restore(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_home, workspace, store) = setup(cx, "worktree-missing");
    // the restore finds the repository by walking up to a `.git`
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    let repo = workspace.to_string_lossy().to_string();
    let worktree = workspace.join(".worktrees").join("agent-gone");
    std::fs::create_dir_all(&worktree).expect("checkout");
    let worktree = worktree.to_string_lossy().to_string();

    let key = store.update(cx, |store, cx| {
        store.new_chat(cx);
        let key = store.focused_tile.clone().expect("a focused draft");
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: repo.clone(),
                repository: Some(repo.clone()),
                worktrees: vec![(worktree.clone(), "agent/gone".into())],
            },
            cx,
        );
        if let Some(tile) = store.tile_by_key_mut(&key) {
            tile.input = "Keep this chat going".into();
        }
        store.toggle_worktree(&key, cx);
        store.send_from_tile(&key, cx);
        store.on_net_event(
            UiEvent::WorktreeCreated {
                tile_key: key.clone(),
                folder: worktree.clone(),
                branch: "agent/gone".into(),
            },
            cx,
        );
        key
    });

    // the checkout disappears underneath the chat
    std::fs::remove_dir_all(&worktree).expect("delete the checkout");
    store.update(cx, |store, cx| {
        if let Some(tile) = store.tile_by_key_mut(&key) {
            tile.input = "Keep this chat going".into();
        }
        store.send_from_tile(&key, cx);
    });

    let (session, input, view, system_notes) = store.read_with(cx, |store, _| {
        let tile = store.tile_by_key(&key).expect("the draft");
        (
            tile.session_id.clone(),
            tile.input.clone(),
            store.worktree_view(&key),
            notes(store, &key),
        )
    });
    assert!(
        session.is_none(),
        "nothing was sent from a missing checkout"
    );
    assert_eq!(input, "Keep this chat going", "the composer keeps the text");
    assert!(view.missing, "the row says the checkout is gone");
    assert!(
        system_notes
            .iter()
            .any(|(text, is_error)| *is_error && text.contains("is gone")),
        "the refusal is explained: {system_notes:?}"
    );

    // the same branch at the same path, through the repair path
    store.update(cx, |store, cx| store.recreate_worktree(&key, cx));
    let notes_while_restoring = store.read_with(cx, |store, _| notes(store, &key));
    assert!(
        notes_while_restoring
            .iter()
            .any(|(text, _)| text.contains("Restoring the worktree")),
        "{notes_while_restoring:?}"
    );
    store.update(cx, |store, cx| {
        store.on_net_event(
            UiEvent::WorktreeAttached {
                tile_key: key.clone(),
                folder: worktree.clone(),
                branch: "agent/gone".into(),
                error: None,
            },
            cx,
        );
    });
    let view = store.read_with(cx, |store, _| store.worktree_view(&key));
    assert!(!view.missing, "the chat runs again after the restore");
}

#[gpui_kit::gpui::test]
fn the_guard_clears_when_the_folder_comes_back(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_home, workspace, store) = setup(cx, "worktree-back");
    let repo = workspace.to_string_lossy().to_string();
    let worktree = workspace.join(".worktrees").join("agent-back");
    std::fs::create_dir_all(&worktree).expect("checkout");
    let worktree = worktree.to_string_lossy().to_string();
    let key = store.update(cx, |store, cx| {
        store.new_chat(cx);
        let key = store.focused_tile.clone().expect("a focused draft");
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: repo.clone(),
                repository: Some(repo.clone()),
                worktrees: vec![(worktree.clone(), "agent/back".into())],
            },
            cx,
        );
        store.toggle_worktree(&key, cx);
        store.send_from_tile(&key, cx);
        store.on_net_event(
            UiEvent::WorktreeCreated {
                tile_key: key.clone(),
                folder: worktree.clone(),
                branch: "agent/back".into(),
            },
            cx,
        );
        key
    });

    std::fs::remove_dir_all(&worktree).expect("delete");
    store.update(cx, |store, cx| {
        if let Some(tile) = store.tile_by_key_mut(&key) {
            tile.input = "one".into();
        }
        store.send_from_tile(&key, cx);
    });
    assert!(store.read_with(cx, |store, _| store.worktree_view(&key).missing));

    // someone restores the folder by hand: the next send clears the alarm
    std::fs::create_dir_all(&worktree).expect("restore");
    store.update(cx, |store, cx| store.send_from_tile(&key, cx));
    let view = store.read_with(cx, |store, _| store.worktree_view(&key));
    assert!(!view.missing, "a present checkout clears the guard");
}

#[gpui_kit::gpui::test]
fn the_composer_offers_restore_for_a_missing_checkout(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_home, workspace, store) = setup(cx, "worktree-restore-ui");
    let repo = workspace.to_string_lossy().to_string();
    let worktree = workspace.join(".worktrees").join("agent-ui");
    std::fs::create_dir_all(&worktree).expect("checkout");
    let worktree = worktree.to_string_lossy().to_string();
    store.update(cx, |store, cx| {
        store.new_chat(cx);
        let key = store.focused_tile.clone().expect("a focused draft");
        store.on_net_event(
            UiEvent::RepositoryProbed {
                folder: repo.clone(),
                repository: Some(repo.clone()),
                worktrees: vec![(worktree.clone(), "agent/ui".into())],
            },
            cx,
        );
        if let Some(tile) = store.tile_by_key_mut(&key) {
            tile.input = "ui".into();
            // a chat that already ran in that checkout
            tile.worktree = Some(warp_agent::store::WorktreeDraft {
                enabled: true,
                branch: "agent/ui".into(),
                folder: Some(worktree.clone()),
                creating: false,
                pending_send: false,
                missing: true,
            });
            tile.session_id = Some("probe-session".into());
        }
    });

    let (_window, cx) =
        cx.add_window_view(|_, cx| warp_agent::ui::window::WarpWindow::new(store.clone(), cx));
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
    assert!(
        cx.debug_bounds("worktree-restore").is_some(),
        "the missing checkout offers the restore"
    );
}
