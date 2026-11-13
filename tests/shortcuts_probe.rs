//! The real window's keyboard sheet: the E2E env hook opens it on launch, the
//! card takes the keys, and every close path returns them to the window root.
//!
//! The sheet is `warpui_kit_core::shortcuts::dialog`; these probes use its
//! debug selectors (`shortcut-sheet-card`, `shortcut-sheet-close`) and the
//! window's probe handles, never a second copy of the content.

use gpui::{FocusHandle, Modifiers, TestAppContext, VisualTestContext, point};
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

/// `WARP_AGENT_SHORTCUTS=1` for the length of one scenario; the window reads
/// it once in `WarpWindow::new`.
struct ShortcutsEnv {
    old: Option<OsString>,
}

impl ShortcutsEnv {
    fn on() -> Self {
        let old = std::env::var_os("WARP_AGENT_SHORTCUTS");
        unsafe { std::env::set_var("WARP_AGENT_SHORTCUTS", "1") };
        Self { old }
    }
}

impl Drop for ShortcutsEnv {
    fn drop(&mut self) {
        unsafe {
            match &self.old {
                Some(value) => std::env::set_var("WARP_AGENT_SHORTCUTS", value),
                None => std::env::remove_var("WARP_AGENT_SHORTCUTS"),
            }
        }
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
}

fn open(cx: &VisualTestContext, window: &gpui::Entity<WarpWindow>) -> bool {
    window.read_with(cx, |window, _| window.shortcuts_open_for_probe())
}

fn handle<F>(cx: &VisualTestContext, window: &gpui::Entity<WarpWindow>, read: F) -> FocusHandle
where
    F: Fn(&WarpWindow) -> FocusHandle,
{
    window.read_with(cx, |window, _| read(window))
}

fn is_focused(cx: &mut VisualTestContext, handle: &FocusHandle) -> bool {
    cx.update(|window, _| handle.is_focused(window))
}

fn click_at(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} draws"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// A store with one workspace and one chat, so the sidebar and titlebar draw
/// their normal shapes around the card.
fn seeded_store(cx: &mut TestAppContext, home: &std::path::Path) -> gpui::Entity<Store> {
    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace folder");
    let folder = workspace.to_string_lossy().to_string();
    let store = cx.update(Store::for_probe);
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
    store
}

#[gpui::test]
fn the_env_hook_opens_a_focused_sheet_and_every_close_returns_the_root(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("shortcuts-env");
    let _env = ShortcutsEnv::on();
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });
    let store = seeded_store(cx, &home.home);

    let (window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);

    assert!(
        open(cx, &window),
        "WARP_AGENT_SHORTCUTS=1 opens the sheet on the first frame"
    );
    let card = handle(cx, &window, WarpWindow::shortcuts_focus_handle);
    let root = handle(cx, &window, WarpWindow::root_focus_handle);
    assert!(
        is_focused(cx, &card),
        "the card takes the keyboard while it is open"
    );
    assert!(
        cx.debug_bounds("shortcut-sheet-list").is_some(),
        "the sheet body draws its groups"
    );

    // Escape is the sheet's first key: it closes and hands the keyboard back
    // to the window root.
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(!open(cx, &window), "Escape closes the sheet");
    assert!(
        is_focused(cx, &root),
        "closing returns the keys to the window root"
    );

    // `?` reopens it from the root, and the close button is a second close
    // path with the same focus restore.
    cx.simulate_keystrokes("?");
    draw(cx);
    assert!(
        open(cx, &window),
        "`?` opens the sheet from the window root"
    );
    assert!(is_focused(cx, &card), "the reopened card takes the keys");
    click_at(cx, "shortcut-sheet-close");
    draw(cx);
    assert!(!open(cx, &window), "the close button closes the sheet");
    assert!(
        is_focused(cx, &root),
        "the close button returns the keys to the window root"
    );

    // Ctrl+/ is the family's other chord; it toggles like `?`.
    cx.simulate_keystrokes("ctrl-/");
    draw(cx);
    assert!(open(cx, &window), "Ctrl+/ opens the sheet");
    cx.simulate_keystrokes("ctrl-/");
    draw(cx);
    assert!(!open(cx, &window), "Ctrl+/ closes the sheet again");
    assert!(is_focused(cx, &root), "the chord close restores the root");

    // The titlebar icon is the pointer path into the same surface.
    click_at(cx, "shortcuts-toggle");
    draw(cx);
    assert!(open(cx, &window), "the titlebar icon opens the sheet");
    assert!(is_focused(cx, &card), "the titlebar path focuses the card");

    // A press on the scrim is the remaining close path.
    let corner = point(gpui::px(4.), gpui::px(4.));
    cx.simulate_click(corner, Modifiers::default());
    draw(cx);
    assert!(!open(cx, &window), "a scrim press closes the sheet");
    assert!(
        is_focused(cx, &root),
        "the scrim close returns the keys to the window root"
    );
}

#[gpui::test]
fn the_sheet_is_modal_over_lists_and_skips_fields(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("shortcuts-modal");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });
    let store = seeded_store(cx, &home.home);

    let (window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);

    // Put the sidebar's cursor on a row, then open the sheet from the list:
    // the keys now belong to the card, so the cursor must not step under it.
    let sidebar = handle(cx, &window, WarpWindow::sidebar_focus_handle);
    cx.update(|window, cx| window.focus(&sidebar, cx));
    draw(cx);
    for _ in 0..3 {
        cx.simulate_keystrokes("down");
        draw(cx);
    }
    let cursor = window.read_with(cx, |window, _| window.sidebar_cursor_for_probe());
    assert!(cursor.is_some(), "the sidebar cursor starts on a row");

    cx.simulate_keystrokes("?");
    draw(cx);
    assert!(open(cx, &window), "`?` opens the sheet from the sidebar");
    let card = handle(cx, &window, WarpWindow::shortcuts_focus_handle);
    assert!(is_focused(cx, &card), "the card takes the keys");

    cx.simulate_keystrokes("down");
    draw(cx);
    assert_eq!(
        window.read_with(cx, |window, _| window.sidebar_cursor_for_probe()),
        cursor,
        "the sidebar cursor does not move while the card is up"
    );

    cx.simulate_keystrokes("escape");
    draw(cx);

    // With the filter field focused, `?` is typing, not the sheet.
    let filter = window
        .read_with(cx, |window, cx| window.sidebar_filter_focus_for_probe(cx))
        .expect("the sidebar filter renders");
    cx.update(|window, cx| window.focus(&filter, cx));
    draw(cx);
    cx.simulate_keystrokes("?");
    draw(cx);
    assert!(
        !open(cx, &window),
        "a focused text field keeps the question mark"
    );
}
