//! Headless probe for chat-transcript text selection: replicates the tile's
//! custom scroll container (overflow_hidden + track_scroll + handle-driven
//! offset) around selectable kit TextViews, then drags over them with
//! simulated mouse events the way gpui-kit's own selection tests do.

use gpui_kit::base::text::TextView;
use gpui_kit::base::{TextSelection, TextSelectionLayer};
use gpui_kit::gpui::prelude::*;
use gpui_kit::gpui::{
    Modifiers, MouseButton, Pixels, Point, ScrollHandle, TestAppContext, Window, div, point, px,
};

fn tall_text() -> String {
    // tall enough to overflow any test window bounds many times over
    (0..2000)
        .map(|i| format!("line number {} of the tall probe document", i))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn short_text(seed: usize) -> String {
    // tall enough that any drag band inside the tile crosses this message's
    // lines, and distinct so readouts are attributable
    (0..60)
        .map(|i| format!("message {seed} line {i} with plain words to select",))
        .collect::<Vec<_>>()
        .join("\n\n")
}

struct TileSpec {
    messages: Vec<String>,
    scroll_to: Option<Pixels>,
}

struct Probe {
    tiles: Vec<(TileSpec, ScrollHandle)>,
    /// render messages as TextView::markdown (internal state) instead of
    /// TextView::new over external states
    use_markdown: bool,
    /// give every markdown message the SAME element id (the old
    /// render_markdown bug shape) instead of per-message ids
    shared_markdown_id: bool,
}

impl Probe {
    fn single(message: String, scrolled: bool) -> Self {
        Self {
            tiles: vec![(
                TileSpec {
                    messages: vec![message],
                    scroll_to: scrolled.then_some(px(-500.)),
                },
                ScrollHandle::new(),
            )],
            use_markdown: false,
            shared_markdown_id: false,
        }
    }
}

impl Render for Probe {
    fn render(
        &mut self,
        _: &mut Window,
        _: &mut gpui_kit::gpui::Context<Self>,
    ) -> impl IntoElement {
        let mut root = div()
            .size_full()
            .flex()
            .flex_row()
            .child(TextSelectionLayer);
        for (ix, (spec, scroll)) in self.tiles.iter().enumerate() {
            let mut transcript = div()
                .id(("transcript", ix))
                .flex_1()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .track_scroll(scroll)
                .p_1()
                .flex()
                .flex_col();
            if let Some(offset) = spec.scroll_to {
                scroll.set_offset(point(px(0.), offset));
            }
            for (m, message) in spec.messages.iter().enumerate() {
                let id = if self.shared_markdown_id {
                    gpui_kit::gpui::ElementId::NamedInteger("md".into(), 0)
                } else {
                    gpui_kit::gpui::ElementId::NamedInteger("md".into(), m as u64)
                };
                let el = if self.use_markdown {
                    TextView::markdown(id, gpui_kit::gpui::SharedString::from(message.clone()))
                        .selectable(true)
                        .into_any_element()
                } else {
                    TextView::markdown(
                        gpui_kit::gpui::ElementId::NamedInteger("md".into(), m as u64),
                        gpui_kit::gpui::SharedString::from(message.clone()),
                    )
                    .selectable(true)
                    .into_any_element()
                };
                transcript = transcript.child(div().id(("message", m)).flex().flex_col().child(el));
            }
            root = root.child(transcript);
        }
        root
    }
}

fn drag(cx: &mut gpui_kit::gpui::VisualTestContext, from: Point<Pixels>, to: Point<Pixels>) {
    cx.simulate_mouse_move(from, None, Modifiers::default());
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    for i in 1..=8 {
        let t = i as f32 / 8.;
        let p = Point::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
        cx.simulate_mouse_move(p, MouseButton::Left, Modifiers::default());
    }
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    cx.update(|window, cx| drop(window.draw(cx)));
}

#[gpui_kit::gpui::test]
fn selection_works_in_single_scrolled_tile(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (_root, cx) = cx.add_window_view(|_, _| Probe::single(tall_text(), true));
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));

    drag(
        cx,
        Point::new(px(20.), px(300.)),
        Point::new(px(380.), px(330.)),
    );

    let selected = cx.update(TextSelection::selected_text);
    assert!(
        !selected.trim().is_empty(),
        "expected a selection, got none"
    );
}

#[gpui_kit::gpui::test]
fn duplicate_markdown_ids_break_selection(cx: &mut TestAppContext) {
    // regression: render_markdown used one constant ElementId for every
    // assistant message, so sibling TextViews shared a global element id and
    // their selection participants collided.
    for (distinct, label) in [(false, "same-id"), (true, "distinct-ids")] {
        cx.update(gpui_kit::init);
        let (_root, cx) = cx.add_window_view(|_, _| Probe {
            tiles: vec![(
                TileSpec {
                    messages: vec![short_text(0), short_text(1)],
                    scroll_to: None,
                },
                ScrollHandle::new(),
            )],
            use_markdown: true,
            shared_markdown_id: !distinct,
        });
        cx.run_until_parked();
        cx.update(|window, cx| drop(window.draw(cx)));

        // vertical drags so the band must cross text lines, not paragraph gaps
        for (drag_label, y0, y1) in [
            ("msg0-only", 60., 260.),
            ("msg1-only", 1600., 1800.),
            ("cross-msg0-to-msg1", 100., 1600.),
        ] {
            drag(cx, Point::new(px(60.), px(y0)), Point::new(px(70.), px(y1)));
            let win = cx.update(TextSelection::selected_text);
            println!("{label}/{drag_label}: window-len={}", win.trim().len());
        }
    }
}

#[gpui_kit::gpui::test]
fn selection_works_in_second_of_two_tiles(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (_root, cx) = cx.add_window_view(|_, _| Probe {
        tiles: vec![
            (
                TileSpec {
                    messages: vec![short_text(10)],
                    scroll_to: None,
                },
                ScrollHandle::new(),
            ),
            (
                TileSpec {
                    messages: vec![short_text(20), short_text(21)],
                    scroll_to: None,
                },
                ScrollHandle::new(),
            ),
        ],
        use_markdown: true,
        shared_markdown_id: false,
    });
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
    let width = cx.update(|window, _| window.bounds().size.width);

    // vertical drags: the right tile's first message, and a cross-message
    // drag spanning both of its TextViews (both must yield text)
    let x_mid = width / 2. + px(60.);
    for (label, needle, y0, y1) in [
        ("right-msg0", "20 line", 60., 260.),
        ("right-cross", "line", 60., 690.),
    ] {
        drag(
            cx,
            Point::new(x_mid, px(y0)),
            Point::new(x_mid + px(10.), px(y1)),
        );
        let win = cx.update(TextSelection::selected_text);
        assert!(
            win.contains(needle),
            "{label}: expected readout containing {needle}, got {win:?}"
        );
    }
}
