//! Headless probe for transcript text selection: a drag over a read-only
//! markdown answer must move the selection *while the button is still down*,
//! and the styled runs (bold, inline code) must be selectable like the plain
//! text around them.
//!
//! The tile view is binary-only (no `lib.rs` boundary), so the probe mirrors
//! the wiring that matters: a **cached** host view (the tile in the grid)
//! whose transcript is a `list` whose rows embed `LiveEditor`s built during
//! prepaint, with the same observer the tile installs to repaint on a
//! selection move.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::gpui::prelude::*;
use gpui_kit::gpui::{
    Bounds, Context, Entity, IntoElement, ListAlignment, ListState, Modifiers, MouseButton, Pixels,
    Render, StyleRefinement, Styled, TestAppContext, VisualTestContext, WeakEntity, Window, div,
    list, point, px,
};
use warpui_kit_markdown::{LiveEditor, LiveEditorState};

/// A sentence with a plain prefix, a bold run and a plain tail: enough to tell
/// styled runs from prose and to hit a specific character by bounds.
const TEXT: &str = "alpha **beta** gamma delta";
/// The `b` of "beta" in `TEXT` (source offsets include the `**` markers).
const BETA: std::ops::Range<usize> = 8..9;
/// The space just past "gamma": dragging here selects through the word.
const PAST_GAMMA: std::ops::Range<usize> = 20..21;

struct Host {
    editor: Entity<LiveEditorState>,
    state: ListState,
    /// Renders of the cached host: a selection move must reach this.
    renders: usize,
}

impl Host {
    fn new(editor: Entity<LiveEditorState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // the tile's contract: only a moved selection or a moved content height
        // repaints the tile, because the editor is not a view and its notify
        // never invalidates the cached host on its own
        let seen = Rc::new(Cell::new((usize::MAX, usize::MAX, f32::NAN)));
        cx.observe(&editor, move |_, editor, cx| {
            let editor = editor.read(cx);
            let range = editor.selection_range();
            let next = (range.start, range.end, editor.content_height());
            if seen.get() != next {
                seen.set(next);
                cx.notify();
            }
        })
        .detach();
        let _ = window;
        Self {
            editor,
            // one message row plus a filler row below it
            state: ListState::new(2, ListAlignment::Top, px(200.)),
            renders: 0,
        }
    }

    /// One transcript row, built during the list's prepaint — the same shape
    /// as the tile's `render_row`.
    fn row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        match ix {
            0 => {
                let height = self.editor.read(cx).content_height().max(1.);
                div()
                    .debug_selector(|| "probe-editor".into())
                    .w_full()
                    .h(px(height))
                    .child(
                        LiveEditor::new(&self.editor)
                            .read_only(true)
                            .fit_content(true)
                            .text_size(px(14.)),
                    )
                    .into_any_element()
            }
            _ => div().w_full().h(px(40.)).child("filler").into_any_element(),
        }
    }
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        let tile: WeakEntity<Self> = cx.entity().downgrade();
        div().size_full().child(
            list(self.state.clone(), move |ix, _window, cx| {
                tile.update(cx, |this, cx| this.row(ix, cx))
                    .unwrap_or_else(|_| div().into_any_element())
            })
            .size_full(),
        )
    }
}

/// The tile in the grid: a cached child view inside the window's own tree.
struct Root {
    host: Entity<Host>,
}

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let style = StyleRefinement::default().size_full();
        div().size_full().child(self.host.clone().cached(style))
    }
}

fn build(cx: &mut TestAppContext) -> (Entity<Root>, Entity<Host>, &mut VisualTestContext) {
    let (root, cx) = cx.add_window_view(|window, cx| {
        let editor = cx.new(|cx| {
            let mut editor = LiveEditorState::new(window, cx);
            editor.set_read_only(true, cx);
            editor.set_value(TEXT, window, cx);
            editor
        });
        let host = cx.new(|cx| Host::new(editor, window, cx));
        Root { host }
    });
    let host = root.read_with(cx, |root, _| root.host.clone());
    (root, host, cx)
}

#[gpui_kit::gpui::test]
fn a_drag_selects_before_the_button_comes_up(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (_root, host, cx) = build(cx);
    cx.run_until_parked();
    // settle the first layout (the editor reports its height one frame late)
    // so the render count below only moves for the selection
    for _ in 0..3 {
        cx.update(|window, cx| drop(window.draw(cx)));
    }

    // the dragged span: from inside the bold run to inside the plain tail
    let (from, to) = cx.update(|_, cx| {
        let editor = host.read(cx).editor.clone();
        let editor = editor.read(cx);
        (
            editor.range_to_bounds(&BETA),
            editor.range_to_bounds(&PAST_GAMMA),
        )
    });
    // the left edge of a character's box, so the hit lands *on* it
    let at_left = |bounds: Bounds<Pixels>| {
        point(
            bounds.origin.x + px(1.),
            bounds.origin.y + bounds.size.height / 2.,
        )
    };
    let from = at_left(from.expect("the bold run is laid out"));
    let to = at_left(to.expect("the tail is laid out"));

    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    // the down already repaints (the caret moves); the drag itself is what
    // must keep repainting the cached host
    let after_down = cx.update(|_, cx| host.read(cx).renders);

    // still holding the button: the selection must already have moved
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
    let after_move = cx.update(|_, cx| host.read(cx).renders);
    assert!(
        after_move > after_down,
        "a drag move must repaint the cached host ({after_down} -> {after_move})"
    );

    let selected = cx
        .update(|_, cx| host.read(cx).editor.read(cx).selected_text())
        .expect("a drag selects text before the release");
    assert!(
        selected.contains("beta") && selected.contains("gamma"),
        "the drag must cover the styled run and the plain tail, got {selected:?}"
    );

    // release keeps the same selection
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    let after = cx
        .update(|_, cx| host.read(cx).editor.read(cx).selected_text())
        .expect("the selection survives the release");
    assert_eq!(after, selected);
}
