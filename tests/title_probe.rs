//! The real window's OS title: the titlebar activity rule reaches
//! `VisualTestContext::window_title()` exactly as it reaches the platform.
//!
//! The title is `warpui_kit_core::activity::activity_title`: the running count
//! and the app name joined by the kit's em-dash separator while a chat runs,
//! and the bare app name otherwise. The titlebar chip carries the same count
//! as its accessible name, so the accessibility tree is checked next to the
//! OS title when this test platform builds one.

use gpui::{Entity, TestAppContext, VisualTestContext};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use warp_agent::store::Store;
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

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
}

/// Opens the real `WarpWindow` over a workspace holding one seeded chat tile.
/// `running` decides whether that chat is in the store's running set, which is
/// the only input to both the OS title and the titlebar chip.
fn seed_and_open<'a>(
    cx: &'a mut TestAppContext,
    home: &Path,
    running: bool,
) -> (Entity<WarpWindow>, &'a mut VisualTestContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });

    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace folder");
    let store = cx.update(Store::for_probe);
    let folder = workspace.to_string_lossy().to_string();
    store.update(cx, |store, cx| store.add_workspace(&folder, cx));
    store.update(cx, |store, cx| {
        store.seed_probe_tile(&folder, "probe-chat", "Probe chat", Vec::new(), running, cx);
    });

    let (window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);
    (window, cx)
}

#[gpui::test]
fn a_running_chat_sets_the_counted_title(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _home = HomeGuard::new("title-running");
    let (_window, cx) = seed_and_open(cx, &_home.home, true);

    assert_eq!(
        cx.window_title().as_deref(),
        Some("1 running — Warp Agent"),
        "a running tile puts its count before the app name in the OS title"
    );

    // The chip draws only while something runs; its aria_label is the same
    // "<n> running" line. The debug tree retains the last frame's a11y update,
    // which this headless platform may never build (accessibility activates
    // from the platform adapter, and the test adapter is a no-op).
    let a11y = cx.update(|window, _| window.debug_a11y_tree_json());
    match a11y.as_deref() {
        Some(json) => assert!(
            json.contains("1 running"),
            "the running chip's accessible name is in the a11y tree: {json}"
        ),
        None => eprintln!(
            "warp-agent title_probe: Window::debug_a11y_tree_json() is None on this headless \
             test platform, so the chip's accessible name is not observable here; the OS title \
             assertion above is the proof of the running state"
        ),
    }
}

#[gpui::test]
fn a_chat_that_is_not_running_keeps_the_plain_title(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _home = HomeGuard::new("title-idle");
    let (_window, cx) = seed_and_open(cx, &_home.home, false);

    assert_eq!(
        cx.window_title().as_deref(),
        Some("Warp Agent"),
        "an open chat that is not running leaves the OS title plain"
    );
}
