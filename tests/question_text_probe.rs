//! The question card's text is a read-only markdown surface; its custom
//! answer field keeps the keyboard while a selection exists in that text.

use gpui::{Modifiers, TestAppContext, VisualTestContext};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use warp_agent::store::{ChatItem, QuestionEntry, QuestionItem, Store};
use warp_agent::ui::window::WarpWindow;
use warpui_kit_markdown::read_only_selection_text;

static HOME_LOCK: Mutex<()> = Mutex::new(());

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

#[gpui::test]
fn a_question_selection_survives_typing_in_the_custom_answer(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("question-text");
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
    });

    let workspace = home.home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace folder");
    let store = cx.update(Store::for_probe);
    let folder = workspace.to_string_lossy().to_string();
    let question = "Which color is your favorite?";
    let key = store.update(cx, |store, cx| {
        let items = vec![ChatItem::Question(QuestionItem {
            id: "qprobe-1".into(),
            session_id: "probe-chat".into(),
            questions: vec![QuestionEntry {
                question: question.into(),
                header: String::new(),
                multiple: false,
                options: Vec::new(),
                custom_text: String::new(),
                locked: false,
            }],
            resolution: None,
        })];
        let key = store.seed_probe_tile(&folder, "probe-chat", "Probe chat", items, false, cx);
        store.focus_tile(&key, cx);
        key
    });

    let (window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);
    draw(cx);

    let tile = window
        .read_with(cx, |window, _| window.tile_view_for_probe(&key))
        .expect("the seeded tile renders a live view");
    let editor = tile
        .read_with(cx, |tile, _| tile.markdown_editor_for_probe("qqprobe-1#0"))
        .expect("the question text has an editor");
    let start = question.find("favorite").expect("the probe word");
    cx.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            editor.set_selected_range(start..start + "favorite".len(), cx)
        })
    });
    draw(cx);
    let selected = cx.update(|_, cx| read_only_selection_text(cx).unwrap_or_default());
    assert_eq!(selected, "favorite", "the question text selects like prose");

    let field = tile
        .read_with(cx, |tile, cx| {
            tile.question_field_focus_for_probe("qprobe-1", 0, cx)
        })
        .expect("the custom answer field exists");
    cx.update(|window, cx| window.focus(&field, cx));
    draw(cx);
    cx.simulate_input("blue");
    draw(cx);
    cx.simulate_keystrokes("space");
    cx.simulate_input("green");
    draw(cx);

    let answer = store.read_with(cx, |store, _| {
        store
            .open_tiles
            .iter()
            .find(|tile| tile.key == key)
            .and_then(|tile| {
                tile.items.iter().find_map(|item| match item.as_ref() {
                    ChatItem::Question(question) => Some(question.questions[0].custom_text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default()
    });
    assert_eq!(answer, "blue green", "typing reaches the store");
    assert!(
        cx.update(|window, _| field.is_focused(window)),
        "the custom answer field keeps the keyboard"
    );
    assert_eq!(
        cx.update(|_, cx| read_only_selection_text(cx).unwrap_or_default()),
        "favorite",
        "typing in the field does not disturb the question selection"
    );
    assert!(
        !cx.update(|window, cx| editor.read(cx).focus_handle().is_focused(window)),
        "the read-only question text never takes focus from the field"
    );

    // The selection is still usable through the editor's own Copy menu.
    let b = cx
        .debug_bounds("text-question")
        .expect("the question text renders");
    cx.simulate_mouse_down(b.center(), gpui::MouseButton::Right, Modifiers::default());
    cx.simulate_mouse_up(b.center(), gpui::MouseButton::Right, Modifiers::default());
    draw(cx);
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    draw(cx);
    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default();
    assert_eq!(copied, "favorite");
}
