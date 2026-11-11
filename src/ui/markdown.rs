//! Chat-answer markdown rendering through the suite's own live markdown
//! engine (warpui-kit-markdown) in read-only mode: the same rendered surface
//! the notes editor shows, with the syntax parked everywhere, no caret, no
//! text input, no formatting — but text selection and copy, clickable links
//! and tree-sitter-highlighted code blocks (enabled per language via the
//! kit's cargo features) stay.
//!
//! The editor entity per message lives in TileView.md_editors, created on
//! first render and fed new text as the message streams. Two pitfalls from
//! the TextView era still apply and are honored here:
//!
//! - element ids must be unique per message: a constant id gave every
//!   assistant message in the transcript the SAME global element id, and
//!   their states collided — selection only worked with a single message.
//!   The overlay copy button keys off "{flash_base}#code{n}" (= item id).
//! - the kit re-lays-out only what changed, so the tile feeds a cached,
//!   pointer-stable SharedString per message (TileView.md_text) and the
//!   editor only sees set_value when the text actually moved.
//!
//! ## The transcript's text surfaces
//!
//! The renderer follows the surface's role. Each decision is recorded here so
//! a rename or a refactor cannot migrate a surface silently:
//!
//! - **Assistant text** uses [`render_markdown`] (read-only `LiveEditor`, one
//!   editor per item). This is the content rule's target shape.
//! - **Tool title** is plain `div`/`SharedString` text: a label, no markdown,
//!   no selection menu.
//! - **User message** uses [`text_view`]. It is one static document, so the
//!   viewer would fit, but the bubble's right-click is the item menu (Copy /
//!   Edit in input / Re-prompt / Fork) and the read-only `LiveEditor` always
//!   installs its own Copy/Select-all menu and stops propagation. The kit has
//!   no host-menu hook, so moving it now would lose the item actions. This is
//!   a content-rule violation until that hook exists.
//! - **Reasoning** uses [`text_view`]. It streams: every update replaces the
//!   whole part text, and `LiveEditorState::set_value` resets the selection to
//!   the document start and the scroll to 0, so a selection made mid-stream
//!   would die on the next chunk. Do not move it until the kit can feed text
//!   while mapping the selection through the unchanged prefix.
//! - **Tool details** uses [`text_view`] for plain payloads; a payload that
//!   parses as JSON goes to `warpui_kit_diff::render_code` instead, so code
//!   keeps its gutter and highlighting. A tool owns up to three such payloads
//!   (input/output/error), they can stream, and one editor per payload is the
//!   design's "streaming tool payload" wrong-cost exception. The plain branch
//!   therefore keeps the guard and `text_menu` deliberately.
//! - **Question** (entry text and resolution) uses [`text_view`]. The card
//!   owns a custom-answer `InputState` and the option rows; the read-only
//!   editor's selection and context menu would compete with that input, and it
//!   would add an editor per question entry to a virtualized row. It waits for
//!   a focus/menu-ownership decision.
//! - **System note** (including error notes) uses [`text_view`]. These are
//!   app-authored status strings (`Context compacted.`, reconnect and failure
//!   messages), so by role they are labels; the guard path also leaves their
//!   error text selectable for copying. That is an open design choice (plain
//!   status, or a copy affordance for errors), not a content migration.
//!
//! Every [`text_view`] surface keeps the parser guard: `TextView` parses on
//! the UI thread while laying out and does not catch the parser, so a text
//! that trips a `markdown` bug aborts the frame. The guard renders such a text
//! as plain prose instead of letting one message take the window down (see
//! [`renders_as_markdown`]). A status chip and every section label stay plain
//! text and never take this path.

use std::cell::RefCell;
use std::collections::VecDeque;

use gpui::prelude::*;
use gpui::{AnyElement, Entity, SharedString, div, px};
use warpui_kit_markdown::{CodeBlock, LiveEditor, LiveEditorState};

use crate::ui::tile::TileView;

/// Renders one assistant message's markdown. The read-only editor sizes
/// itself to its content (the transcript scrolls as a whole; the editor
/// never becomes an inner scroll area), and every fenced code block hosts a
/// copy button in its top-right corner: clicking flashes the check icon in
/// the accent color for a second — the same animation as the message copy
/// buttons, driven through TileView.copied_flash with keys
/// "{flash_base}#code{n}" (n = fenced block order in this message).
#[expect(
    clippy::too_many_arguments,
    reason = "the render inputs are one message's transient state; a struct would be rebuilt per message anyway"
)]
pub fn render_markdown(
    editor: &Entity<LiveEditorState>,
    font_px: f32,
    icon_px: f32,
    flash_base: String,
    flash_active: Option<&str>,
    view: gpui::WeakEntity<TileView>,
    accent: gpui::Rgba,
    cx: &gpui::App,
) -> AnyElement {
    let flash_active = flash_active.map(str::to_string);
    // the overlay closure is re-run on every render of the message, so the
    // flash state reads fresh each time and the per-block counter of the
    // TextView era is replaced by the kit's stable block indices
    let overlay = move |block: &CodeBlock| -> Option<AnyElement> {
        let key = format!("{flash_base}#code{}", block.index);
        let flashed = flash_active.as_deref() == Some(key.as_str());
        let (icon_path, icon_color, border) = if flashed {
            ("icons/check.svg", accent, accent)
        } else {
            (
                "icons/copy.svg",
                crate::theme::text_muted(),
                crate::theme::hairline(),
            )
        };
        let code = block.code.clone();
        let click_view = view.clone();
        let click_key = key.clone();
        Some(
            crate::ui::tooltip::tip(
                div()
                    .id(gpui::ElementId::NamedInteger(
                        "mdcopy".into(),
                        TileView::hash(&key),
                    ))
                    .cursor_pointer()
                    .p_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(border)
                    .bg(crate::theme::chip())
                    .hover(|s| s.bg(crate::theme::HOVER))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.to_string()));
                        // flash_copied owns the one-second revert timer
                        let _ = click_view
                            .update(cx, |this, cx| this.flash_copied(click_key.clone(), cx));
                    }),
                "Copy code",
            )
            .child(crate::ui::icon(icon_path, px(icon_px), icon_color))
            .into_any_element(),
        )
    };
    // the wrapper carries the laid-out content height (fit_content): the
    // editor notifies on moves, and TileView re-renders on that notify
    let height = editor.read(cx).content_height().max(1.);
    div()
        .w_full()
        .h(px(height))
        .child(
            LiveEditor::new(editor)
                .read_only(true)
                .fit_content(true)
                .text_size(px(font_px))
                .code_block_overlay(overlay),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// The guard for the TextView surfaces
// ---------------------------------------------------------------------------

/// Verdicts by text, most recently used at the back, held up to this many
/// bytes. A repaint walks every transcript surface the tile holds; without the
/// memo each one would be parsed again only to re-learn the verdict it already
/// had.
const VERDICT_BYTES: usize = 4 * 1024 * 1024;

thread_local! {
    static VERDICTS: RefCell<VecDeque<(String, bool)>> = const { RefCell::new(VecDeque::new()) };
}

/// One warning per run is enough to name the library bug: a streaming message
/// that carries the offending shape probes a new text every chunk.
static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One transcript text still on the guard path: user message, reasoning, tool
/// details, question text/resolution and system note. gpui-base's Markdown
/// `TextView` when the parser can take it, plain prose when it cannot.
/// `max_lines` is the `TextView`'s single-line clamp and the plain branch
/// truncates to match; no caller passes one today (the tool-title row is a
/// label and never takes this path).
///
/// Do not add a surface here silently: the allowed set and the constraint each
/// member is waiting on are listed in the module docs and in
/// `docs/agents/design-system.md#text-surfaces`.
///
/// The plain branch gives up text selection on that one text. The alternative
/// is giving up the window: `TextView` parses on the UI thread while laying out
/// and does not catch the parser, so a text that trips a `markdown` bug aborts
/// the frame. 1.0.0 has one — a setext heading, then a thematic break, then
/// another heading hits `expect("Cannot push to non-parent")` in `to_mdast` —
/// and that is precisely the shape a diff of deleted Markdown writes line for
/// line, which is how a wand prompt killed the app before its first frame.
pub fn text_view(
    id: gpui::ElementId,
    selector: &'static str,
    text: impl Into<SharedString>,
    max_lines: Option<usize>,
    menu: Option<gpui::FocusHandle>,
) -> AnyElement {
    let text = text.into();
    // Stable across frames, so a real-view probe can right-click the surface
    // whose context menu it is checking.
    let view = if renders_as_markdown(&text) {
        let view = gpui_kit::base::text::TextView::markdown(id.clone(), text);
        let view = match max_lines {
            Some(lines) => view.max_lines(lines),
            None => view,
        }
        .into_any_element();
        match menu {
            Some(focus) => warpui_kit_core::text_menu::attach(id, view, Some(focus)),
            None => view,
        }
    } else {
        div()
            .when(max_lines.is_some(), |row| row.truncate())
            .child(text)
            .into_any_element()
    };
    div()
        .debug_selector(move || selector.to_string())
        .child(view)
        .into_any_element()
}

/// Whether `TextView`'s parser can render `text`, decided by running it under
/// `catch_unwind` with the options the TextView uses.
fn renders_as_markdown(text: &str) -> bool {
    if let Some(known) = remembered(text) {
        return known;
    }
    let verdict = probe(text);
    if !verdict && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            bytes = text.len(),
            "markdown parser panics on this text; rendering it as plain text"
        );
    }
    remember(text, verdict);
    verdict
}

/// The options gpui-base gives `TextView::markdown` (`MarkdownExtensions`
/// defaults in `gpui-base/src/text/markdown_ext.rs`): GFM plus both math
/// constructs, frontmatter off. They are not public there, so they are
/// mirrored here — the probe predicts the TextView only while the two agree,
/// which makes this the place to look when gpui-base moves.
fn parse_options() -> markdown::ParseOptions {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options
}

fn probe(text: &str) -> bool {
    let options = parse_options();
    // the panic is the expected outcome here, so it must not reach the crash
    // net: a backtrace per probe would flood the log as a message streams
    crate::services::quietly(|| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            markdown::to_mdast(text, &options)
        }))
        .is_ok()
    })
}

fn remembered(text: &str) -> Option<bool> {
    VERDICTS.with(|verdicts| {
        let mut verdicts = verdicts.borrow_mut();
        let at = verdicts
            .iter()
            .position(|(known, _)| known.as_str() == text)?;
        let entry = verdicts.remove(at)?;
        let verdict = entry.1;
        verdicts.push_back(entry);
        Some(verdict)
    })
}

fn remember(text: &str, verdict: bool) {
    VERDICTS.with(|verdicts| {
        let mut verdicts = verdicts.borrow_mut();
        let mut held: usize = verdicts.iter().map(|(known, _)| known.len()).sum();
        while held + text.len() > VERDICT_BYTES {
            let Some((dropped, _)) = verdicts.pop_front() else {
                break;
            };
            held -= dropped.len();
        }
        // a text that alone exceeds the budget is still kept: it is the one
        // whose re-parse costs the most
        verdicts.push_back((text.to_string(), verdict));
    });
}

#[cfg(test)]
mod guard_tests {
    use super::renders_as_markdown;

    /// The `markdown` 1.0.0 compiler bug the guard exists for. A diff of
    /// deleted Markdown spells this out exactly: its empty lines read `-`, its
    /// rules `----`.
    const PANICS: &str = "-a\n-\n----\n@@ -1 +0,0 @@\n----";

    #[test]
    fn text_that_panics_the_parser_falls_back_to_plain() {
        // a second call proves the memo answers without parsing (and without
        // panicking) again
        assert!(!renders_as_markdown(PANICS));
        assert!(!renders_as_markdown(PANICS));
    }

    #[test]
    fn ordinary_markdown_still_renders() {
        assert!(renders_as_markdown("# Title\n\n- one\n- two\n\n`code`\n"));
        // a setext heading on its own is fine: the guard is not guessing at
        // headings, it asks the parser
        assert!(renders_as_markdown("a\n-"));
    }
}
