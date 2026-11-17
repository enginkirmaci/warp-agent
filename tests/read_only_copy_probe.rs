//! The window's read-only copy path: the text menus print the Ctrl+C key the
//! host answers, and the window root copies a selection that takes no focus —
//! while a focused `Input` keeps its own Copy.
//!
//! A read-only `LiveEditor` keeps its selection in the markdown kit's
//! read-only selection global; a `TextView`/diff body keeps it in the window's
//! `TextSelection`. Neither surface takes focus, so neither surface's own Copy
//! action dispatches: the window fallback tries the window selection first and
//! then the document's, exactly like the family's other windows.

use gpui::{Modifiers, MouseButton, Role, TestAppContext, VisualTestContext, point, px};
use gpui_kit::test::TestQueryExt as _;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use warp_agent::store::{ChatItem, SessionItem, Store, ToolDetails, ToolItem};
use warp_agent::ui::window::WarpWindow;
use warpui_kit_markdown::read_only_selection_text;

/// Surface selectors, stable across frames and shared with the render code.
const REASONING_TEXT: &str = "text-reasoning";
const TOOL_DETAILS: &str = "text-tool-details";
const REASONING_FOLD: &str = "fold-reasoning";
const DETAILS_FOLD: &str = "fold-tool-details";
/// The Copy row's keycap, drawn by the kit's menu row helper.
const COPY_KEYCAP: &str = "menu-key-Ctrl C";

const REASONING_MARKDOWN: &str = "I should check the **guard** path before answering.";
const TOOL_OUTPUT: &str = "tool output: the guard renders this line as selectable text";

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

fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
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

/// A read-only `LiveEditor` owns its selection, so the host's Ctrl+C fallback
/// global is the editor's equivalent of the window selection.
fn editor_selected_text(cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| read_only_selection_text(cx).unwrap_or_default())
}

fn clipboard_text(cx: &VisualTestContext) -> String {
    cx.read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default()
}

/// Drags across a surface's middle band so its selectable text registers a
/// selection before the menu or the key runs.
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

/// One workspace and one chat with a reasoning surface and a completed tool,
/// both folded by default; `focus_tile` makes the real window render it.
fn seed_store(cx: &mut TestAppContext, home: &HomeGuard) -> gpui::Entity<Store> {
    let workspace = home.home.join("workspace");
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
        let items = vec![
            ChatItem::Reasoning {
                key: "reasoning-1".into(),
                text: REASONING_MARKDOWN.into(),
            },
            ChatItem::Tool(ToolItem::probe_completed(
                "tool-1",
                "read",
                "Read the probe file",
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
    store
}

fn open_window<'a>(
    store: &gpui::Entity<Store>,
    cx: &'a mut TestAppContext,
) -> (gpui::Entity<WarpWindow>, &'a mut VisualTestContext) {
    let (window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);
    draw(cx);
    (window, cx)
}

/// Reasoning and tool details are folded by default; open them through the
/// real headers so their text surfaces exist and can be selected.
fn open_text_surfaces(cx: &mut VisualTestContext) {
    click(cx, DETAILS_FOLD);
    click(cx, REASONING_FOLD);
}

/// The markdown editor owns its menu, so the read-only host's key reaches it
/// through `LiveEditor::menu_keys`; the guard's `TextView` takes it through
/// `text_menu::attach`. Both print the same keycap.
#[gpui::test]
fn the_read_only_menus_print_the_copy_key(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("read-only-copy-keycap");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });
    let store = seed_store(cx, &home);
    let (_window, cx) = open_window(&store, cx);
    open_text_surfaces(cx);

    // The read-only editor's own menu: Copy prints the key the window answers.
    select_surface(cx, REASONING_TEXT);
    right_click(cx, REASONING_TEXT);
    assert_eq!(menu_count(cx), 1, "the reasoning surface opens its menu");
    assert!(
        cx.debug_bounds(COPY_KEYCAP).is_some(),
        "the read-only editor's Copy row prints the Ctrl+C keycap"
    );
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert_eq!(menu_count(cx), 0, "Escape closes the editor's menu");

    // The guard's `TextView` through `text_menu::attach`: the same keycap.
    select_surface(cx, TOOL_DETAILS);
    right_click(cx, TOOL_DETAILS);
    assert_eq!(menu_count(cx), 1, "the tool details surface opens its menu");
    assert!(
        cx.debug_bounds(COPY_KEYCAP).is_some(),
        "the guarded TextView's Copy row prints the Ctrl+C keycap"
    );
}

/// The `TextView`/diff body registers with the window's `TextSelectionLayer`
/// but takes no focus: the window root hands that selection to the clipboard.
#[gpui::test]
fn ctrl_c_copies_the_window_selection(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("read-only-copy-window");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });
    let store = seed_store(cx, &home);
    let (_window, cx) = open_window(&store, cx);
    open_text_surfaces(cx);

    select_surface(cx, TOOL_DETAILS);
    let selected = selected_text(cx);
    assert!(
        selected.contains("selectable"),
        "a drag over the tool details selects its text, got {selected:?}"
    );

    cx.simulate_keystrokes("ctrl-c");
    draw(cx);
    // gpui's root `Copy` binding answers the window selection first and trims
    // its edges; the window handler's window-selection branch reads the same
    // selection, so compare the text itself.
    let copied = clipboard_text(cx);
    assert_eq!(
        copied.trim(),
        selected.trim(),
        "Ctrl+C copies the window's read-only selection"
    );
}

/// The read-only `LiveEditor` takes no focus either, so its own Copy action
/// never dispatches: the window answers Ctrl+C from the document selection
/// when the window has none.
#[gpui::test]
fn ctrl_c_copies_the_editor_selection(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("read-only-copy-editor");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });
    let store = seed_store(cx, &home);
    let (_window, cx) = open_window(&store, cx);
    open_text_surfaces(cx);

    select_surface(cx, REASONING_TEXT);
    let selected = editor_selected_text(cx);
    assert!(
        selected.contains("guard"),
        "a drag over the reasoning surface selects its markdown text, got {selected:?}"
    );
    assert!(
        selected_text(cx).is_empty(),
        "the editor selection is not the window selection"
    );

    cx.simulate_keystrokes("ctrl-c");
    draw(cx);
    assert_eq!(
        clipboard_text(cx),
        selected,
        "Ctrl+C copies the read-only editor's selection"
    );
}

/// The fallback leaves the key alone while an `Input` owns it: the field's own
/// Copy runs and writes the field's selection, never the read-only text.
#[gpui::test]
fn a_focused_input_keeps_its_own_ctrl_c(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("read-only-copy-input");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });
    let store = seed_store(cx, &home);
    let (window, cx) = open_window(&store, cx);
    open_text_surfaces(cx);

    // A read-only selection exists in the window while the field takes the
    // keyboard, so a fallback that ignored the context stack would copy it.
    select_surface(cx, TOOL_DETAILS);
    let read_only = selected_text(cx);
    assert!(
        read_only.contains("selectable"),
        "a drag over the tool details selects its text, got {read_only:?}"
    );

    let filter = window
        .read_with(cx, |window, cx| window.sidebar_filter_focus_for_probe(cx))
        .expect("the sidebar filter renders");
    cx.update(|window, cx| window.focus(&filter, cx));
    draw(cx);
    cx.simulate_keystrokes("needle");
    draw(cx);
    assert!(
        selected_text(cx).contains("selectable"),
        "the read-only selection is still live while the field owns the keys"
    );
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_keystrokes("ctrl-c");
    draw(cx);

    assert_eq!(
        clipboard_text(cx),
        "needle",
        "a focused Input copies its own selection, not the read-only text"
    );
    assert_ne!(
        clipboard_text(cx),
        read_only,
        "the window fallback does not run while an Input owns the keys"
    );
}
