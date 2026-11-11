//! The real window's transcript text surfaces: a content text renders as
//! selectable markdown through the guard and opens the Copy context menu,
//! while a tool title is a label and opens nothing.
//!
//! The text menu is drawn by GPUI, so the probe checks what the real window
//! exposes: a menu role and a completed Copy action (the clipboard carries the
//! text the menu captured when it opened). `Window::debug_a11y_tree_json()` is
//! inspected too; on a headless test platform accessibility is never activated,
//! so the tree is absent and the role/clipboard assertions are the proof.

use gpui::{Modifiers, MouseButton, Pixels, Role, TestAppContext, VisualTestContext, point, px};
use gpui_kit::test::TestQueryExt as _;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use warp_agent::store::{ChatItem, SessionItem, Store, ToolDetails, ToolItem};
use warp_agent::ui::window::WarpWindow;

/// Surface selectors, stable across frames and shared with the render code.
const USER_TEXT: &str = "text-user";
const REASONING_TEXT: &str = "text-reasoning";
const TOOL_DETAILS: &str = "text-tool-details";
const TOOL_TITLE: &str = "tool-title";
const REASONING_FOLD: &str = "fold-reasoning";
const DETAILS_FOLD: &str = "fold-tool-details";
const TILE_MENU: &str = "tile-menu";
const TILE_MENU_COPY: &str = "tile-menu-row-0";

const USER_MARKDOWN: &str = "## Probe question\n\nPlease **copy** this paragraph.";
const REASONING_MARKDOWN: &str = "I should check the **guard** path before answering.";
const TOOL_OUTPUT: &str = "tool output: the guard renders this line as selectable text";
const TOOL_TITLE_TEXT: &str = "Read the probe file";

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

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
}

fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing text surface {selector}"))
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let at = bounds(cx, selector).center();
    cx.simulate_click(at, Modifiers::default());
    draw(cx);
}

/// Right-press and release at the surface's center, then draw the frame that
/// renders whatever menu the surface owns.
fn right_click(cx: &mut VisualTestContext, selector: &'static str) {
    let at = bounds(cx, selector).center();
    cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::default());
    cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::default());
    draw(cx);
}

fn selected_text(cx: &mut VisualTestContext) -> String {
    cx.update(gpui_kit::base::TextSelection::selected_text)
}

fn clipboard_text(cx: &VisualTestContext) -> String {
    cx.read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default()
}

/// Drags across a surface's middle band so its selectable text registers a
/// selection before the menu builds.
fn select_surface(cx: &mut VisualTestContext, selector: &'static str) {
    let rect = bounds(cx, selector);
    let from = point(rect.left() + px(2.), rect.center().y);
    let to = point(rect.right() - px(2.), rect.center().y);
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    draw(cx);
}

fn menu_count(cx: &mut VisualTestContext) -> usize {
    cx.update(|window, _| window.find_all_by_role(Role::Menu).len())
}

/// The debug a11y tree is only built while the platform reports assistive
/// technology; the headless test adapter never activates it, so this is None
/// here and the role/clipboard assertions below carry the proof.
fn a11y_json(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|window, _| window.debug_a11y_tree_json())
}

#[gpui::test]
fn content_text_keeps_its_copy_menu_and_a_tool_title_stays_plain(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("text-surfaces");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });

    let workspace = home.home.join("workspace");
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
        let items = vec![
            ChatItem::UserMessage {
                key: 0,
                text: USER_MARKDOWN.into(),
                created_ms: 0,
            },
            ChatItem::Reasoning {
                key: "reasoning-1".into(),
                text: REASONING_MARKDOWN.into(),
            },
            ChatItem::Tool(ToolItem::probe_completed(
                "tool-1",
                "read",
                TOOL_TITLE_TEXT,
                ToolDetails {
                    input: String::new(),
                    output: TOOL_OUTPUT.into(),
                    error: String::new(),
                },
            )),
        ];
        let key = store.seed_probe_tile(&folder, "probe-chat", "Probe chat", items, false, cx);
        // the real window renders a seeded tile only once it is the open chat
        store.focus_tile(&key, cx);
    });

    let (_window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);
    draw(cx);

    // Reasoning and tool details are folded by default; open them through the
    // real headers so their text surfaces exist.
    click(cx, REASONING_FOLD);
    click(cx, DETAILS_FOLD);
    assert_eq!(menu_count(cx), 0, "no menu is open before the right-clicks");

    // ---- content text: selection, menu, Copy ----
    select_surface(cx, REASONING_TEXT);
    let selected = selected_text(cx);
    assert!(
        selected.contains("guard"),
        "a drag over the reasoning surface selects its markdown text, got {selected:?}"
    );

    right_click(cx, REASONING_TEXT);
    assert_eq!(
        menu_count(cx),
        1,
        "the reasoning surface opens the text menu on a right-click"
    );
    if let Some(json) = a11y_json(cx) {
        assert!(
            json.contains("Copy"),
            "the a11y tree lists the text menu's Copy item: {json}"
        );
    } else {
        eprintln!(
            "warp-agent text_surfaces_probe: Window::debug_a11y_tree_json() is None on this \
             headless test platform (accessibility never activates), so the Copy item is proven \
             by its menu role and the clipboard action below"
        );
    }
    let selected_after_press = selected_text(cx);
    assert!(
        !selected_after_press.is_empty(),
        "the right-press that opens the menu keeps the window selection the menu captures"
    );

    // Down selects the menu's first (only) item, Enter runs its Copy handler.
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(menu_count(cx), 0, "activating Copy dismisses the text menu");
    assert_eq!(
        clipboard_text(cx),
        selected_after_press,
        "the text menu's Copy item writes the selected text to the clipboard"
    );

    // ---- tool details: the same content path through transcript_text ----
    select_surface(cx, TOOL_DETAILS);
    let details_selected = selected_text(cx);
    assert!(
        details_selected.contains("selectable"),
        "a drag over the tool details selects its text, got {details_selected:?}"
    );
    right_click(cx, TOOL_DETAILS);
    assert_eq!(
        menu_count(cx),
        1,
        "the tool details surface opens the text menu on a right-click"
    );
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert_eq!(
        menu_count(cx),
        0,
        "activating the tool details Copy dismisses the text menu"
    );
    assert_eq!(
        clipboard_text(cx),
        details_selected,
        "the tool details Copy item writes the selected text to the clipboard"
    );

    // ---- tool title: a label, no menu ----
    right_click(cx, TOOL_TITLE);
    assert_eq!(
        menu_count(cx),
        0,
        "a right-click on the tool title opens no menu"
    );
    assert!(
        cx.debug_bounds(TILE_MENU).is_none(),
        "a right-click on the tool title opens no item menu"
    );
    if let Some(json) = a11y_json(cx) {
        assert!(
            !json.contains("Copy"),
            "no Copy item is exposed for the tool title: {json}"
        );
    }

    // ---- user message: content text with the tile's own Copy menu ----
    right_click(cx, USER_TEXT);
    assert!(
        cx.debug_bounds(TILE_MENU).is_some(),
        "the user message opens its context menu on a right-click"
    );
    if let Some(json) = a11y_json(cx) {
        assert!(
            json.contains("Copy"),
            "the user message menu offers a Copy row: {json}"
        );
    }
    click(cx, TILE_MENU_COPY);
    assert!(
        cx.debug_bounds(TILE_MENU).is_none(),
        "the Copy row closes the user message menu"
    );
    assert_eq!(
        clipboard_text(cx),
        USER_MARKDOWN,
        "the user message Copy row writes the message text to the clipboard"
    );

    // The tile still renders after the menus; the probe closed it cleanly.
    assert!(
        cx.debug_bounds("transcript").is_some(),
        "the transcript stays rendered"
    );
}
