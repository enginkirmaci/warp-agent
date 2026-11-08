//! Headless probe for the chat transcript's virtualization: a tile with
//! hundreds of messages must build and lay out only the rows its viewport
//! covers, not the whole transcript. That property is what keeps a long chat
//! responsive while a reply streams — before the list, every repaint rebuilt
//! every message's markdown editor.
//!
//! The tile view itself is binary-only (no `lib.rs` boundary), so this probe
//! drives the same container chain, `list` element and re-entrant
//! `WeakEntity::update` the tile wires up, with rows tall enough to make the
//! difference obvious.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::gpui::prelude::*;
use gpui_kit::gpui::{
    Context, Entity, FollowMode, IntoElement, ListAlignment, ListOffset, ListState, Render,
    TestAppContext, VisualTestContext, WeakEntity, Window, div, list, px,
};

/// A transcript long enough that rendering it whole would be visible.
const MESSAGES: usize = 600;
/// Height of one probe row, in logical pixels.
const ROW_HEIGHT: f32 = 60.;

/// What one draw built: the row count and the first/last row index.
type Built = (usize, Option<(usize, usize)>);

struct TranscriptProbe {
    state: ListState,
    /// Rows built in the current draw, recorded inside `render_row`.
    rendered: Rc<Cell<Built>>,
    /// Rows the re-entrant `update` asked the view to build.
    rows_built: usize,
}

impl TranscriptProbe {
    /// The tile's row builder: the list's render closure re-enters the view
    /// that is being drawn to build one row.
    fn render_row(&mut self, ix: usize, rendered: &Cell<Built>) -> gpui::AnyElement {
        self.rows_built += 1;
        let (count, span) = rendered.get();
        let span = match span {
            Some((lo, hi)) => (lo.min(ix), hi.max(ix)),
            None => (ix, ix),
        };
        rendered.set((count + 1, Some(span)));
        div()
            .w_full()
            .h(px(ROW_HEIGHT))
            .child(format!("message {ix}"))
            .into_any_element()
    }
}

impl Render for TranscriptProbe {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rendered = self.rendered.clone();
        let tile: WeakEntity<Self> = cx.entity().downgrade();
        // the tile's container chain: a header, the transcript that takes the
        // rest, and the composer below it
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(div().h(px(44.)).child("header"))
            .child(
                div()
                    .id("transcript")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .child(
                        list(self.state.clone(), move |ix, _window, cx| {
                            let rendered = rendered.clone();
                            tile.update(cx, |this, _cx| this.render_row(ix, &rendered))
                                .unwrap_or_else(|_| div().into_any_element())
                        })
                        .flex_1()
                        .min_h_0()
                        .w_full(),
                    ),
            )
            .child(div().h(px(80.)).child("composer"))
    }
}

/// One draw of the probe; returns how many rows it built and their span.
fn draw(cx: &mut VisualTestContext, rendered: &Rc<Cell<Built>>) -> Built {
    rendered.set((0, None));
    cx.update(|window, cx| drop(window.draw(cx)));
    rendered.get()
}

fn probe(
    cx: &mut TestAppContext,
    count: usize,
) -> (
    Entity<TranscriptProbe>,
    &mut VisualTestContext,
    ListState,
    Rc<Cell<Built>>,
) {
    let state = ListState::new(count, ListAlignment::Top, px(800.));
    state.set_follow_mode(FollowMode::Tail);
    let rendered = Rc::new(Cell::new((0, None)));
    let (probe, cx) = cx.add_window_view({
        let state = state.clone();
        let rendered = rendered.clone();
        move |_, _| TranscriptProbe {
            state,
            rendered,
            rows_built: 0,
        }
    });
    (probe, cx, state, rendered)
}

#[gpui_kit::gpui::test]
fn a_long_transcript_builds_only_its_viewport(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (probe, cx, state, rendered) = probe(cx, MESSAGES);
    cx.run_until_parked();

    let viewport = cx.update(|window, _| window.viewport_size().height.as_f32());
    // the header and composer take 124px of it
    let fits = ((viewport - 124.) / ROW_HEIGHT).ceil() as usize;

    cx.update(|_, cx| probe.update(cx, |probe, _| probe.rows_built = 0));
    let (count, span) = draw(cx, &rendered);
    assert!(count > 0, "the visible rows must render");
    // the rows really came through the view's own re-entrant update
    let built = cx.update(|_, cx| probe.read(cx).rows_built);
    assert_eq!(built, count, "every list row goes through render_row");
    assert!(
        count <= fits + 4,
        "only the rows that fit may be built: {count} rows for a {fits}-row viewport \
         (of {MESSAGES})"
    );
    // following the tail like a chat: the first draw shows the newest messages
    let (lo, hi) = span.expect("rows were built");
    assert_eq!(hi, MESSAGES - 1, "the list opens at the newest message");
    assert_eq!(lo, MESSAGES - fits, "the window starts at the bottom");
    assert!(
        count < MESSAGES / 10,
        "the transcript must not be built whole ({count} of {MESSAGES})"
    );
    // the list got a real viewport: its content is taller than the space it has
    assert!(
        state.max_offset_for_scrollbar().y > px(0.),
        "the transcript must be scrollable in the probe window"
    );

    // jumping to the top, the other end of the transcript, is just as cheap
    state.scroll_to(ListOffset {
        item_ix: 0,
        offset_in_item: px(0.),
    });
    cx.update(|window, cx| drop(window.draw(cx)));
    let (count, span) = draw(cx, &rendered);
    let (lo, _) = span.expect("rows were built at the top");
    assert_eq!(lo, 0, "the top of the transcript renders its first row");
    assert!(
        count <= fits + 4,
        "scrolling to the top still builds a viewport, got {count}"
    );
}

/// A chat shorter than its tile keeps its first message at the top, the way
/// the flex column did; the tail follow must not park it above the composer.
#[gpui_kit::gpui::test]
fn a_short_transcript_stays_at_the_top(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    const SHORT: usize = 3;
    let (_probe, cx, state, rendered) = probe(cx, SHORT);
    cx.run_until_parked();

    let (count, span) = draw(cx, &rendered);
    assert_eq!(count, SHORT, "every row of a short chat is on screen");
    assert_eq!(span, Some((0, SHORT - 1)));
    let top = state
        .bounds_for_item(0)
        .expect("the first row was laid out")
        .top()
        .as_f32();
    // below the 44px header, inside the transcript's 12px padding
    assert!(
        top < 100.,
        "a short chat starts at the top of the transcript, got {top}px"
    );
}

/// A tile opens with an empty list, installs tail-follow, and splices the
/// loaded history in on its first render. The list must land on the newest
/// message, not at the top of the history it just received.
#[gpui_kit::gpui::test]
fn history_spliced_into_a_fresh_list_opens_at_the_tail(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let state = ListState::new(0, ListAlignment::Top, px(800.));
    state.set_follow_mode(FollowMode::Tail);
    let rendered = Rc::new(Cell::new((0, None)));
    let (_probe, cx) = cx.add_window_view({
        let state = state.clone();
        let rendered = rendered.clone();
        move |_, _| TranscriptProbe {
            state,
            rendered,
            rows_built: 0,
        }
    });
    // the tile's first render: rows arrive after the list was built
    state.splice(0..0, MESSAGES);
    cx.run_until_parked();

    let (count, span) = draw(cx, &rendered);
    assert!(count > 0);
    assert_eq!(
        span.expect("rows were built").1,
        MESSAGES - 1,
        "loaded history opens at its newest message"
    );
    assert!(count < MESSAGES / 10, "still only a viewport of rows");
}

/// Each streamed message lands as an appended row. While the chat follows the
/// tail, that append must stay in view instead of leaving the newest message
/// below the fold.
#[gpui_kit::gpui::test]
fn an_append_while_following_stays_at_the_tail(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let state = ListState::new(MESSAGES, ListAlignment::Top, px(800.));
    state.set_follow_mode(FollowMode::Tail);
    let rendered = Rc::new(Cell::new((0, None)));
    let (_probe, cx) = cx.add_window_view({
        let state = state.clone();
        let rendered = rendered.clone();
        move |_, _| TranscriptProbe {
            state,
            rendered,
            rows_built: 0,
        }
    });
    cx.run_until_parked();
    let (_, span) = draw(cx, &rendered);
    assert_eq!(span.expect("rows were built").1, MESSAGES - 1);

    // the splice `TileView::sync_rows` applies for an appended item
    state.splice(MESSAGES..MESSAGES, 1);
    cx.run_until_parked();
    let (count, span) = draw(cx, &rendered);
    assert_eq!(
        span.expect("rows were built").1,
        MESSAGES,
        "the appended row must be in view"
    );
    assert!(count < MESSAGES / 10, "the append still builds a viewport");
}
