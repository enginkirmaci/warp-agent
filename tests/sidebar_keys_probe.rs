//! The real window's sidebar keys: the focused sidebar owns them, and the
//! cursor walks the rows the sidebar actually flattens.

use gpui::TestAppContext;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use warp_agent::store::{SessionItem, Store};
use warp_agent::ui::window::WarpWindow;

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// A scratch home so `Store::new` neither reads nor writes the developer's
/// `~/.warp/agent`. Tests in one binary share the process environment, so the
/// scenarios take turns behind `HOME_LOCK`.
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

fn draw(cx: &mut gpui::VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
}

fn cursor(cx: &gpui::VisualTestContext, window: &gpui::Entity<WarpWindow>) -> Option<String> {
    window.read_with(cx, |window, _| window.sidebar_cursor_for_probe())
}

#[gpui::test]
fn sidebar_keys_walk_the_real_window(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _home = HomeGuard::new("sidebar-keys");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });

    let workspace = _home.home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace folder");
    let store = cx.update(Store::for_probe);
    let folder = workspace.to_string_lossy().to_string();
    store.update(cx, |store, cx| {
        store.add_workspace(&folder, cx);
        if let Some(workspace) = store
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.folder_path() == folder)
        {
            workspace.chats.push(SessionItem {
                id: "probe-chat".into(),
                title: "Probe chat".into(),
                updated_ms: 0,
                created_ms: 0,
                is_pinned: false,
                is_selected: false,
                is_running: false,
                is_editing: false,
                draft_title: String::new(),
            });
        }
    });

    let (window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);

    let focus = window.read_with(cx, |window, _| window.sidebar_focus_handle());
    cx.update(|window, cx| window.focus(&focus, cx));
    draw(cx);

    // Tasks (the General workspace) leads the tree, then this folder, then its
    // chat: three steps land on the chat.
    for _ in 0..3 {
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
    }
    assert_eq!(
        cursor(cx, &window).as_deref(),
        Some("chat:probe-chat"),
        "the focused sidebar walks to the chat row"
    );

    // Home returns to the first navigable row (Tasks), End to the chat.
    cx.simulate_keystrokes("home");
    cx.run_until_parked();
    assert!(
        cursor(cx, &window)
            .as_deref()
            .is_some_and(|it| it.starts_with("workspace:")),
        "Home lands on the first workspace row"
    );
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    assert_eq!(
        cursor(cx, &window).as_deref(),
        Some("chat:probe-chat"),
        "End lands on the last row"
    );

    let filter = window
        .read_with(cx, |window, cx| window.sidebar_filter_focus_for_probe(cx))
        .expect("the sidebar filter renders");
    cx.update(|window, cx| window.focus(&filter, cx));
    draw(cx);
    let before = cursor(cx, &window);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(
        cursor(cx, &window),
        before,
        "the filter owns the arrows while it is focused"
    );
}

#[gpui::test]
fn sidebar_enter_activates_the_chat_row(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _home = HomeGuard::new("sidebar-enter");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });

    let workspace = _home.home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace folder");
    let store = cx.update(Store::for_probe);
    let folder = workspace.to_string_lossy().to_string();
    store.update(cx, |store, cx| {
        store.add_workspace(&folder, cx);
        if let Some(workspace) = store
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.folder_path() == folder)
        {
            workspace.chats.push(SessionItem {
                id: "probe-chat".into(),
                title: "Probe chat".into(),
                updated_ms: 0,
                created_ms: 0,
                is_pinned: false,
                is_selected: false,
                is_running: false,
                is_editing: false,
                draft_title: String::new(),
            });
        }
        // the row activates through the session map, which needs a tile to
        // focus; the tile itself is not focused until Enter runs
        store.seed_probe_tile(&folder, "probe-chat", "Probe chat", Vec::new(), false, cx);
    });

    let (window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);

    let focus = window.read_with(cx, |window, _| window.sidebar_focus_handle());
    cx.update(|window, cx| window.focus(&focus, cx));
    draw(cx);

    // Tasks (the General workspace) leads the tree, then this folder, then its
    // chat: three steps land on the chat.
    for _ in 0..3 {
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
    }
    assert_eq!(
        cursor(cx, &window).as_deref(),
        Some("chat:probe-chat"),
        "the focused sidebar walks to the chat row"
    );
    assert!(
        store.read_with(cx, |store, _| store.focused_tile().is_none()),
        "the cursor walk does not open the chat"
    );

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    draw(cx);

    assert_eq!(
        store.read_with(cx, |store, _| store
            .focused_tile()
            .map(|tile| tile.key.clone())),
        Some("chat:probe-chat".to_string()),
        "Enter focuses the chat's tile"
    );
    assert_eq!(
        cursor(cx, &window).as_deref(),
        Some("chat:probe-chat"),
        "activation leaves the cursor on the chat row"
    );
}
