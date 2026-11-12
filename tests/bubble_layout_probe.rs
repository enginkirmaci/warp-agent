//! The transcript's two message bubbles share one measure: the prompt is not
//! a shrink-wrapped sliver, both boxes are the same width, and the markdown
//! fills the bubble's padded content box instead of the editor's own centered
//! column. The probe renders the real window and reads its laid-out bounds.

use gpui::{TestAppContext, VisualTestContext, px};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use warp_agent::store::{ChatItem, SessionItem, Store};
use warp_agent::ui::window::WarpWindow;

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// A scratch home so `Store::new` neither reads nor writes the developer's
/// `~/.warp/agent`.
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

fn assert_near(actual: f32, expected: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= 0.5,
        "{what}: expected {expected}px, got {actual}px"
    );
}

#[gpui_kit::gpui::test]
fn user_and_assistant_bubbles_share_their_measure(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("bubble-layout");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });

    let workspace = home.home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace folder");
    let store = cx.update(Store::for_probe);
    let folder = workspace.to_string_lossy().to_string();
    let items = vec![
        ChatItem::UserMessage {
            key: 0,
            text: "Make the prompt bubble match the answer bubble.".into(),
            created_ms: 0,
        },
        ChatItem::AssistantText {
            key: "1".into(),
            text: "Both bubbles now share one measure, and the markdown fills \
                   the bubble's padded content box instead of the editor's \
                   centered column."
                .into(),
            meta_label: "probe".into(),
            created_ms: 0,
        },
    ];
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
        let key = store.seed_probe_tile(&folder, "probe-chat", "Probe chat", items, false, cx);
        // the real window renders a seeded tile only once it is the open chat
        store.focus_tile(&key, cx);
        key
    });

    let (_window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);
    draw(cx);

    let user = cx
        .debug_bounds("bubble-user")
        .expect("the user bubble draws");
    let assistant = cx
        .debug_bounds("bubble-assistant")
        .expect("the assistant bubble draws");
    assert_near(
        user.size.width.as_f32(),
        assistant.size.width.as_f32(),
        "the prompt bubble is the answer bubble's width",
    );
    assert!(
        user.size.width > px(320.),
        "the prompt bubble is not a shrink-wrapped sliver, got {}px",
        user.size.width
    );

    // Both markdown surfaces start at the bubble's 12px horizontal padding:
    // the editor's own 2em side margin and readable-column centering are gone.
    let user_text = cx.debug_bounds("text-user").expect("the user text draws");
    let assistant_text = cx
        .debug_bounds("text-assistant")
        .expect("the assistant text draws");
    assert_near(
        (user_text.left() - user.left()).as_f32(),
        12.0,
        "the prompt's text starts at the bubble padding",
    );
    assert_near(
        (assistant_text.left() - assistant.left()).as_f32(),
        12.0,
        "the answer's text starts at the bubble padding",
    );
    assert_near(
        user_text.size.width.as_f32(),
        user.size.width.as_f32() - 24.0,
        "the prompt's text fills the padded box",
    );
    assert_near(
        assistant_text.size.width.as_f32(),
        assistant.size.width.as_f32() - 24.0,
        "the answer's text fills the padded box",
    );
}
