//! The real window's transcript keys: a click on the transcript takes focus,
//! and its Down/Up/PageDown/Home/End keys move the tile's `ListState` offset
//! while the sidebar cursor stays where it was.

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, point, px};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use warp_agent::store::{ChatItem, SessionItem, Store};
use warp_agent::ui::tile::TileView;
use warp_agent::ui::window::WarpWindow;

/// Enough messages that the transcript is taller than the window.
const MESSAGES: usize = 80;

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

/// `ListState` reports its scrollbar offset as a negative y (how far the
/// content moved up); the probe compares the positive distance scrolled.
fn scrolled(cx: &VisualTestContext, tile: &Entity<TileView>) -> f32 {
    -tile.read_with(cx, |tile, _| {
        tile.transcript_scroll_offset_for_probe().as_f32()
    })
}

fn cursor(cx: &VisualTestContext, window: &Entity<WarpWindow>) -> Option<String> {
    window.read_with(cx, |window, _| window.sidebar_cursor_for_probe())
}

fn focused(cx: &mut VisualTestContext, tile: &Entity<TileView>) -> bool {
    cx.update(|window, cx| tile.read(cx).transcript_focus_handle().is_focused(window))
}

fn assert_near(actual: f32, expected: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= 0.5,
        "{what}: expected {expected}px, got {actual}px"
    );
}

#[gpui::test]
fn transcript_keys_scroll_the_real_window(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("transcript-keys");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });

    let workspace = home.home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace folder");
    let store = cx.update(Store::for_probe);
    let folder = workspace.to_string_lossy().to_string();
    let items: Vec<ChatItem> = (0..MESSAGES)
        .map(|key| ChatItem::UserMessage {
            key,
            text: format!(
                "probe message {key}: this transcript is long enough that its rows run \
                 past the bottom of the window and the offset has somewhere to go."
            ),
            created_ms: 0,
        })
        .collect();
    let key = store.update(cx, |store, cx| {
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
        let key = store.seed_probe_tile(&folder, "probe-chat", "Probe chat", items, false, cx);
        // the real window renders a seeded tile only once it is the open chat
        store.focus_tile(&key, cx);
        key
    });

    let (window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);
    draw(cx);
    let tile = window
        .read_with(cx, |window, _| window.tile_view_for_probe(&key))
        .expect("the seeded tile renders a live view");

    // A click inside the transcript's left padding (away from the message
    // bubbles, whose editors take their own focus) hands it the keys.
    let bounds = cx.debug_bounds("transcript").expect("the transcript draws");
    let click_at = point(bounds.left() + px(4.), bounds.center().y);
    cx.simulate_click(click_at, Modifiers::default());
    draw(cx);
    assert!(
        focused(cx, &tile),
        "a click on the transcript gives the transcript focus"
    );

    // The seeded chat opens following its tail, so it is scrollable.
    let tail_offset = scrolled(cx, &tile);
    assert!(
        tail_offset > 0.0,
        "the seeded transcript is taller than the window (offset {tail_offset})"
    );

    cx.simulate_keystrokes("home");
    draw(cx);
    let home_offset = scrolled(cx, &tile);
    assert_eq!(home_offset, 0.0, "Home returns to offset 0");

    cx.simulate_keystrokes("down");
    draw(cx);
    let down_offset = scrolled(cx, &tile);
    assert!(
        down_offset > home_offset,
        "Down increases the offset ({down_offset} from {home_offset})"
    );

    cx.simulate_keystrokes("up");
    draw(cx);
    let up_offset = scrolled(cx, &tile);
    assert!(
        up_offset < down_offset,
        "Up decreases the offset ({up_offset} from {down_offset})"
    );

    cx.simulate_keystrokes("home");
    draw(cx);
    assert_eq!(scrolled(cx, &tile), 0.0);
    cx.simulate_keystrokes("pagedown");
    draw(cx);
    let page_offset = scrolled(cx, &tile);
    assert!(
        page_offset > down_offset,
        "PageDown moves farther than one Down ({page_offset} vs {down_offset})"
    );

    // Paging to the bottom finds the maximum offset on its own, so End can be
    // checked against it instead of against whatever End happens to report.
    let mut page_max = page_offset;
    let mut saturated = false;
    for _ in 0..MESSAGES {
        cx.simulate_keystrokes("pagedown");
        draw(cx);
        let next = scrolled(cx, &tile);
        if (next - page_max).abs() <= 0.5 {
            saturated = true;
            break;
        }
        page_max = next;
    }
    assert!(saturated, "PageDown walks the transcript to its bottom");
    assert!(
        page_max > page_offset,
        "the transcript has more than one page ({page_max} vs {page_offset})"
    );

    cx.simulate_keystrokes("home");
    draw(cx);
    assert_eq!(scrolled(cx, &tile), 0.0);
    cx.simulate_keystrokes("end");
    draw(cx);
    let max_offset = scrolled(cx, &tile);
    assert_near(
        max_offset,
        page_max,
        "End reaches the maximum offset PageDown found",
    );
    // At the maximum, another End or a PageDown cannot move further down.
    cx.simulate_keystrokes("end");
    draw(cx);
    assert_near(
        scrolled(cx, &tile),
        max_offset,
        "End settles at the maximum offset",
    );
    cx.simulate_keystrokes("pagedown");
    draw(cx);
    assert_near(
        scrolled(cx, &tile),
        max_offset,
        "nothing scrolls past the maximum offset",
    );

    // The sidebar owns a cursor of its own: set it on a real row, then hand
    // the keys to the transcript and check the cursor does not follow them.
    let sidebar_focus = window.read_with(cx, |window, _| window.sidebar_focus_handle());
    cx.update(|window, cx| window.focus(&sidebar_focus, cx));
    draw(cx);
    cx.simulate_keystrokes("down");
    draw(cx);
    let sidebar_before = cursor(cx, &window);
    assert!(
        sidebar_before.is_some(),
        "the sidebar cursor starts on one of its rows"
    );

    let bounds = cx
        .debug_bounds("transcript")
        .expect("the transcript still draws");
    cx.simulate_click(
        point(bounds.left() + px(4.), bounds.center().y),
        Modifiers::default(),
    );
    draw(cx);
    assert!(
        focused(cx, &tile),
        "the second click takes the keys back from the sidebar"
    );

    for key in ["down", "up", "pagedown", "home", "end"] {
        cx.simulate_keystrokes(key);
        draw(cx);
        assert_eq!(
            cursor(cx, &window),
            sidebar_before,
            "the sidebar cursor stays put while the transcript has focus ({key})"
        );
    }
    assert!(
        focused(cx, &tile),
        "the transcript keeps focus while its keys scroll it"
    );
}
