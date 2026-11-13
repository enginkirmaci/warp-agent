//! The real window's pane row: the sidebar and each mounted tile's transcript
//! are joined by `warpui_kit_core::pane_nav`, so `←`/`→` and `Tab`/`Shift+Tab`
//! move between them in grid order and stop at the row's ends.
//!
//! The probe drives the real focus handles, not a copy of the key map: it
//! proves the sidebar hands off to the first tile, the transcripts walk in
//! `open_tiles` order, the row does not wrap, a hidden sidebar is not a stop,
//! and the composer keeps its own keys.

use gpui::{Entity, FocusHandle, TestAppContext, VisualTestContext};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use warp_agent::store::{ChatItem, Store};
use warp_agent::ui::tile::TileView;
use warp_agent::ui::window::WarpWindow;

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Enough messages that the first transcript is taller than its tile.
const MESSAGES: usize = 80;

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

fn is_focused(cx: &mut VisualTestContext, handle: &FocusHandle) -> bool {
    cx.update(|window, _| handle.is_focused(window))
}

fn step(cx: &mut VisualTestContext, keys: &str) {
    cx.simulate_keystrokes(keys);
    draw(cx);
}

#[gpui::test]
fn pane_moves_walk_the_sidebar_and_the_grid_in_order(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("pane-moves");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });

    let workspace = home.home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace folder");
    let folder = workspace.to_string_lossy().to_string();
    let store = cx.update(Store::for_probe);
    let items: Vec<ChatItem> = (0..MESSAGES)
        .map(|key| ChatItem::UserMessage {
            key,
            text: format!(
                "probe message {key}: this transcript is long enough that its rows run \
                 past the bottom of the tile and the offset has somewhere to go."
            ),
            created_ms: 0,
        })
        .collect();
    let (first, second, third) = store.update(cx, |store, cx| {
        store.add_workspace(&folder, cx);
        let first = store.seed_probe_tile(&folder, "probe-1", "Probe one", items, false, cx);
        let second = store.seed_probe_tile(&folder, "probe-2", "Probe two", Vec::new(), false, cx);
        let third = store.seed_probe_tile(&folder, "probe-3", "Probe three", Vec::new(), false, cx);
        // the real window renders a tile only once it is the open chat
        store.focus_tile(&first, cx);
        (first, second, third)
    });

    let (window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);
    draw(cx);

    let sidebar = window.read_with(cx, |window, _| window.sidebar_focus_handle());
    let first_view = window
        .read_with(cx, |window, _| window.tile_view_for_probe(&first))
        .expect("the first tile renders");
    let second_view = window
        .read_with(cx, |window, _| window.tile_view_for_probe(&second))
        .expect("the second tile renders");
    let third_view = window
        .read_with(cx, |window, _| window.tile_view_for_probe(&third))
        .expect("the third tile renders");
    let first_focus = first_view.read_with(cx, |tile, _| tile.transcript_focus_handle());
    let second_focus = second_view.read_with(cx, |tile, _| tile.transcript_focus_handle());
    let third_focus = third_view.read_with(cx, |tile, _| tile.transcript_focus_handle());

    // The sidebar is the row's start. `Tab` hands the keys to the first tile
    // in `open_tiles` order, not to whichever tile happens to look nearest.
    cx.update(|window, cx| window.focus(&sidebar, cx));
    draw(cx);
    assert!(is_focused(cx, &sidebar), "the sidebar starts focused");
    step(cx, "tab");
    assert!(
        is_focused(cx, &first_focus),
        "Tab moves from the sidebar to the first tile"
    );
    assert!(!is_focused(cx, &sidebar), "the sidebar gives the keys up");

    // The transcript keeps its scroll keys while its pane moves work.
    let before = scrolled(cx, &first_view);
    step(cx, "home");
    assert_eq!(scrolled(cx, &first_view), 0.0, "Home keeps its scroll job");
    step(cx, "down");
    let after = scrolled(cx, &first_view);
    assert!(
        after > 0.0 && before >= 0.0,
        "Down still scrolls the transcript ({after} from {before})"
    );
    assert!(
        is_focused(cx, &first_focus),
        "scrolling does not hand the keys away"
    );

    // `→`/`Tab` walk the grid in `open_tiles` order: first, second, third.
    step(cx, "right");
    assert!(
        is_focused(cx, &second_focus),
        "Right moves to the second tile"
    );
    step(cx, "tab");
    assert!(is_focused(cx, &third_focus), "Tab moves to the third tile");
    step(cx, "right");
    assert!(
        is_focused(cx, &third_focus),
        "the rightmost pane has no next stop"
    );
    step(cx, "tab");
    assert!(
        is_focused(cx, &third_focus),
        "Tab at the row end is a no-op"
    );

    // `←`/`Shift+Tab` walk back, then land on the sidebar.
    step(cx, "left");
    assert!(
        is_focused(cx, &second_focus),
        "Left moves to the previous tile"
    );
    step(cx, "shift-tab");
    assert!(
        is_focused(cx, &first_focus),
        "Shift+Tab moves to the previous tile"
    );
    step(cx, "left");
    assert!(
        is_focused(cx, &sidebar),
        "Left from the first tile reaches the sidebar"
    );
    step(cx, "left");
    assert!(
        is_focused(cx, &sidebar),
        "the sidebar is the row start: Left goes nowhere"
    );
    step(cx, "shift-tab");
    assert!(
        is_focused(cx, &sidebar),
        "Shift+Tab at the row start is a no-op"
    );

    // A hidden sidebar is not a stop: the first tile becomes the row start,
    // and the rest of the row still walks.
    store.update(cx, |store, cx| store.set_sidebar_visible(false, cx));
    draw(cx);
    cx.update(|window, cx| window.focus(&first_focus, cx));
    draw(cx);
    step(cx, "left");
    assert!(
        is_focused(cx, &first_focus),
        "with no sidebar drawn, the first tile has no left stop"
    );
    step(cx, "tab");
    assert!(
        is_focused(cx, &second_focus),
        "the rest of the row still moves"
    );
    store.update(cx, |store, cx| store.set_sidebar_visible(true, cx));
    draw(cx);

    // The composer and every text field keep their own keys: the transcript's
    // handler is on the transcript, so the focused composer never consults the
    // pane row.
    let composer = first_view.read_with(cx, |tile, cx| tile.composer_focus_for_probe(cx));
    cx.update(|window, cx| window.focus(&composer, cx));
    draw(cx);
    assert!(
        is_focused(cx, &composer),
        "the composer takes the keys from a click"
    );
    for key in ["left", "right", "tab", "shift-tab"] {
        step(cx, key);
        assert!(
            is_focused(cx, &composer),
            "{key} in the focused composer does not move a pane"
        );
        assert!(
            !is_focused(cx, &first_focus) && !is_focused(cx, &sidebar),
            "{key} leaves the transcript and sidebar unfocused"
        );
    }
    step(cx, "?");
    assert!(
        !window.read_with(cx, |window, _| window.shortcuts_open_for_probe()),
        "a focused composer keeps `?` as text instead of opening the sheet"
    );
}

/// `ListState` reports its scrollbar offset as a negative y (how far the
/// content moved up); the probe compares the positive distance scrolled.
fn scrolled(cx: &VisualTestContext, tile: &Entity<TileView>) -> f32 {
    -tile.read_with(cx, |tile, _| {
        tile.transcript_scroll_offset_for_probe().as_f32()
    })
}
