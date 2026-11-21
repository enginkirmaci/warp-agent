//! The Flow sheet: warp-flow's runs that wait on a person, and the runs that
//! move — raised over the chat grid like the keyboard card, dismissed the same
//! way.
//!
//! This is deliberately not a second kanban. ADR-0012's reason for warp-flow's
//! door is that both UIs send their answers to one writer; a second board would
//! be a second surface to keep in step. So the sheet has two bands:
//!
//! - **waiting on you** — every run with an open question, newest first. The
//!   question's markdown body is a read-only `LiveEditor` (the same content
//!   surface the transcript renders question text on), and the answer controls
//!   follow the question's kind: `approve` yes/no, `select` one row per option,
//!   `multi` a checkbox per option and a Submit, `text`/unknown a field and a
//!   Submit. Answering sends `answer` and prints the door's outcome
//!   (`resumed`/`recorded`); the run leaves the band when the next `runs` poll
//!   says it no longer waits.
//! - **moving** — every host run in a moving phase, with the phase word, the
//!   newest engine log lines and the verbs the phase allows (Pause, Resume,
//!   Abandon). A refusal is the door's own sentence, shown as one line.
//!
//! A run is never started here: starting is warp-flow's own action, and the
//! contract has no verb for it.
//!
//! The card owns the keyboard while it is up (its own `FocusHandle`, the pane
//! key context `Flow`, an Escape binding) and the list carries its own cursor:
//! the arrows belong to this pane while it is focused, exactly as they belong
//! to the sidebar or a transcript while those are focused. A focused answer
//! field keeps the arrows and the letters — the card's handler stands down for
//! an `Input` context.
//!
//! The sheet never reads a file, opens a socket or waits on a lock: it draws
//! what the net thread already put in the store (`Store::flow`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable as _, FontWeight, KeyDownEvent,
    MouseButton, Render, Rgba, Role, ScrollHandle, SharedString, Toggled, Window, actions, div, px,
    relative,
};
use gpui_kit::component::input::{InputEvent, InputState};
use warpui_kit_core::field;
use warpui_kit_core::palette;
use warpui_kit_core::shell;
use warpui_kit_markdown::{LiveEditorState, read_only_document, render_read_only_document_in_card};

use crate::flow::{
    self, DriveAction, FlowStatus, HostRun, Offer, Phase, Question, QuestionKind, Run,
};
use crate::store::{AnswerState, FLOW_LOG_LINES, Store};
use crate::theme;
use crate::ui::{BtnKind, button};

// The sheet's one action outside the list: Escape is bound in the `Flow` key
// context (and over `Flow > Input`, so a focused answer field does not keep
// it), and dispatched to the card.
actions!(warp_agent, [FlowClose]);

/// The sheet card's width: the question body is the widest content and a
/// markdown measure wants room; the window still caps it.
const FLOW_CARD_WIDTH: f32 = 860.;
/// How far PageUp/PageDown step the cursor.
const FLOW_PAGE: usize = 6;

/// One question's key: the question file's path, the identity the door's
/// answer verb and this surface share.
type QuestionKey = String;

/// The sheet's own, per-frame state: the cursor, the drafts and the read-only
/// editors. The data itself lives in the store (fed by the net thread), so a
/// render never touches a socket or a file.
pub struct FlowSheet {
    store: Entity<Store>,
    /// The window root's handle: closing hands the keys back, as the keyboard
    /// sheet's close does.
    root_focus: FocusHandle,
    /// The card's focus; the `Flow` key context hangs off the card it tracks.
    focus: FocusHandle,
    open: bool,
    /// The keyboard cursor into [`FlowSheet::rows`].
    cursor: usize,
    /// The action rows drawn last frame, in visual order.
    rows: Vec<Row>,
    /// The read-only markdown editor per question body, created on first sight.
    editors: HashMap<QuestionKey, Entity<LiveEditorState>>,
    /// The free-text answer field per question, created on first sight.
    text_fields: HashMap<QuestionKey, Entity<InputState>>,
    /// What a field's answer would send: the issue folder and the question
    /// file, per question key.
    text_targets: HashMap<QuestionKey, (PathBuf, PathBuf)>,
    /// The typed value per question key, so Submit and Enter can send it
    /// without reading the widget in a click path.
    text_drafts: HashMap<QuestionKey, String>,
    /// A `multi` question's checkboxes per question key.
    multi: HashMap<QuestionKey, Vec<bool>>,
    /// The body's scroller, so a cursor step brings its card into view.
    scroll: ScrollHandle,
}

impl FlowSheet {
    pub fn new(store: Entity<Store>, root_focus: FocusHandle, cx: &mut Context<Self>) -> Self {
        Self {
            store,
            root_focus,
            focus: cx.focus_handle(),
            open: false,
            cursor: 0,
            rows: Vec::new(),
            editors: HashMap::new(),
            text_fields: HashMap::new(),
            text_targets: HashMap::new(),
            text_drafts: HashMap::new(),
            multi: HashMap::new(),
            scroll: ScrollHandle::default(),
        }
    }

    /// Whether the sheet is up.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Raise the sheet and give it the keyboard.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            return;
        }
        self.open = true;
        self.cursor = 0;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Dismiss the sheet and hand the keyboard back to the window root. A
    /// focused element that leaves the tree would otherwise leave the
    /// window's chords dead until the next click.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        window.focus(&self.root_focus, cx);
        cx.notify();
    }

    /// The card's focus handle; the sheet probe proves it holds the keys.
    #[doc(hidden)]
    pub fn focus_for_probe(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// The cursor's row, for the sheet probe: one stable word per action.
    #[doc(hidden)]
    pub fn cursor_action_for_probe(&self) -> Option<String> {
        self.rows.get(self.cursor).map(|row| row.action.label())
    }

    // ---- keyboard ---------------------------------------------------------

    /// The list's key map, on the card: arrows move its cursor, Home/End and
    /// PageUp/PageDown go to the ends and by a page, Enter runs the row and `r`
    /// retries an absent door. A focused field keeps every letter and arrow,
    /// and a modified key (Ctrl+C for a read-only selection, Escape's own
    /// binding) is not the list's.
    fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let modifiers = event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        let in_field = window
            .context_stack()
            .iter()
            .any(|context| context.contains("Input"));
        if in_field || modifiers.modified() {
            return false;
        }
        let handled = match key {
            "up" => {
                self.step_cursor(-1);
                true
            }
            "down" => {
                self.step_cursor(1);
                true
            }
            "home" => {
                self.move_cursor(0);
                true
            }
            "end" => {
                self.move_cursor(self.rows.len().saturating_sub(1));
                true
            }
            "pageup" => {
                self.step_cursor(-(FLOW_PAGE as isize));
                true
            }
            "pagedown" => {
                self.step_cursor(FLOW_PAGE as isize);
                true
            }
            "enter" => self.activate_cursor(window, cx),
            "r" => {
                let absent = matches!(
                    self.store.read(cx).flow().status(),
                    FlowStatus::Absent { .. }
                );
                if absent {
                    self.store.update(cx, |store, cx| store.flow_refresh(cx));
                }
                absent
            }
            "escape" => {
                // The `FlowClose` binding normally takes this before the raw
                // handler; this is the fallback when the key context is not
                // what the binding expects.
                self.close(window, cx);
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
        handled
    }

    fn step_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let next = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs())
        } else {
            self.cursor
                .saturating_add(delta as usize)
                .min(self.rows.len() - 1)
        };
        self.move_cursor(next);
    }

    fn move_cursor(&mut self, next: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.cursor = next.min(self.rows.len() - 1);
        if let Some(row) = self.rows.get(self.cursor) {
            self.scroll.scroll_to_item(row.card);
        }
    }

    fn activate_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return false;
        };
        self.run_row(row.action, window, cx);
        true
    }

    /// Run one row: a pointer click and Enter go through the same path, so the
    /// two can never disagree about what a row does.
    fn run_row(&mut self, action: RowAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            RowAction::Answer {
                issue,
                question,
                text,
            } => self.send_answer(&issue, &question, text, cx),
            RowAction::Toggle { key, index } => {
                if let Some(picked) = self.multi.get_mut(&key)
                    && let Some(slot) = picked.get_mut(index)
                {
                    *slot = !*slot;
                    cx.notify();
                }
            }
            RowAction::SubmitMulti {
                key,
                issue,
                question,
                choices,
            } => {
                let picked = self.multi.get(&key).cloned().unwrap_or_default();
                let text = flow::multi_answer(&choices, &picked);
                if !text.trim().is_empty() {
                    self.send_answer(&issue, &question, text, cx);
                }
            }
            RowAction::SubmitText { key } => self.submit_text(&key, cx),
            RowAction::Drive { issue, action } => {
                self.store
                    .update(cx, |store, cx| store.flow_drive(&issue, action, cx));
            }
            RowAction::Retry => self.store.update(cx, |store, cx| store.flow_refresh(cx)),
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// A pointer click lands the cursor on the row it hit, then runs it, so the
    /// keyboard continues from where the pointer was.
    fn clicked_row(&mut self, action: RowAction, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.rows.iter().position(|row| row.action == action) {
            self.cursor = index;
        }
        self.run_row(action, window, cx);
    }

    fn send_answer(&mut self, issue: &Path, question: &Path, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.store.update(cx, |store, cx| {
            store.flow_answer(issue, question, text, cx);
        });
    }

    fn submit_text(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some((issue, question)) = self.text_targets.get(key).cloned() else {
            return;
        };
        let text = self.text_drafts.get(key).cloned().unwrap_or_default();
        self.send_answer(&issue, &question, text, cx);
    }

    /// Create the free-text answer field for one question and wire it: a
    /// keystroke repaints the card only when the field flips between empty and
    /// filled (the Submit button's enabled state), and Enter sends.
    fn text_field(
        &mut self,
        key: &str,
        issue: &Path,
        question: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        self.text_targets.insert(
            key.to_owned(),
            (issue.to_path_buf(), question.to_path_buf()),
        );
        if let Some(field) = self.text_fields.get(key) {
            return field.clone();
        }
        let state = cx.new(|cx| InputState::new(window, cx).placeholder("Type an answer…"));
        let draft_key = key.to_owned();
        let submit_key = key.to_owned();
        cx.subscribe(
            &state,
            move |this: &mut Self, field, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    let text = field.read(cx).value().to_string();
                    let filled = !text.trim().is_empty();
                    let was_filled = this
                        .text_drafts
                        .get(&draft_key)
                        .is_some_and(|draft| !draft.trim().is_empty());
                    this.text_drafts.insert(draft_key.clone(), text);
                    if filled != was_filled {
                        cx.notify();
                    }
                }
                InputEvent::PressEnter { shift: false, .. } => {
                    this.submit_text(&submit_key, cx);
                }
                _ => {}
            },
        )
        .detach();
        self.text_fields.insert(key.to_owned(), state.clone());
        state
    }

    // ---- render helpers ---------------------------------------------------

    /// The card's row set for a status, with its cursor and scroll mapping.
    fn body(
        &mut self,
        data: &BodyData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Vec<AnyElement>, Vec<Row>) {
        let (status, waiting, moving, fonts, accent, cursor) = (
            data.status,
            data.waiting,
            data.moving,
            data.fonts,
            data.accent,
            data.cursor,
        );
        let repo_count = data.repo_count;
        let mut children: Vec<AnyElement> = Vec::new();
        let mut rows: Vec<Row> = Vec::new();
        match status {
            FlowStatus::Absent { .. } => {
                children.push(
                    div()
                        .debug_selector(|| "flow-absent".into())
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(fonts.font_note))
                                .text_color(theme::text())
                                .child(format!(
                                    "{} is not running. Start {}, then retry.",
                                    flow::APP,
                                    flow::APP
                                )),
                        )
                        .into_any_element(),
                );
                let retry = div()
                    .debug_selector(|| "flow-retry".into())
                    .child(button(
                        gpui::ElementId::Name("flow-retry-btn".into()),
                        "Retry",
                        BtnKind::Chip,
                        accent,
                        fonts.font_control,
                        true,
                        {
                            let sheet = cx.entity();
                            move |_, window, cx| {
                                sheet.update(cx, |this, cx| {
                                    this.clicked_row(RowAction::Retry, window, cx);
                                });
                            }
                        },
                    ))
                    .into_any_element();
                record_row(&mut children, &mut rows, retry, RowAction::Retry, cursor);
            }
            FlowStatus::Looking => {
                children.push(
                    div()
                        .debug_selector(|| "flow-looking".into())
                        .text_size(px(fonts.font_note))
                        .text_color(theme::text_dim())
                        .child("Looking for warp-flow…")
                        .into_any_element(),
                );
            }
            FlowStatus::Ready => {
                if !waiting.is_empty() {
                    children.push(section_head("Waiting on you", waiting.len(), fonts));
                    for run in waiting {
                        let card_index = children.len();
                        let first_row = rows.len();
                        let card = self.waiting_card(run, data, &mut rows, window, cx);
                        for row in &mut rows[first_row..] {
                            row.card = card_index;
                        }
                        children.push(card);
                    }
                }
                if !moving.is_empty() {
                    children.push(section_head("Moving", moving.len(), fonts));
                    for run in moving {
                        let card_index = children.len();
                        let first_row = rows.len();
                        let card = self.moving_card(run, data, &mut rows, cx);
                        for row in &mut rows[first_row..] {
                            row.card = card_index;
                        }
                        children.push(card);
                    }
                }
                if waiting.is_empty() && moving.is_empty() {
                    children.push(
                        div()
                            .debug_selector(|| "flow-empty".into())
                            .flex()
                            .flex_col()
                            .gap_2()
                            .py_2()
                            .child(
                                div()
                                    .text_size(px(fonts.font_note))
                                    .text_color(theme::text_dim())
                                    .child("Nothing needs you."),
                            )
                            .child(
                                div()
                                    .text_size(px(fonts.font_meta))
                                    .text_color(theme::text_muted())
                                    .child(SharedString::from(watching_line(repo_count))),
                            )
                            .into_any_element(),
                    );
                }
            }
        }
        (children, rows)
    }

    /// One run that waits on a person: its title, its repository and folder,
    /// and every open question with the controls its kind asks for.
    fn waiting_card(
        &mut self,
        run: &Run,
        data: &BodyData,
        rows: &mut Vec<Row>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fonts = data.fonts;
        let mut column = div().flex().flex_col().gap_2().w_full().min_w_0();
        column = column
            .child(
                div()
                    .id("flow-waiting-run")
                    .debug_selector(|| "flow-waiting-run".into())
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(crate::ui::icon(
                        "icons/chat-outline.svg",
                        px(fonts.icon_small),
                        theme::CHANGES,
                    ))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(fonts.font_name))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(SharedString::from(run.title.clone())),
                    ),
            )
            .child(
                div()
                    .text_size(px(fonts.font_mono_meta))
                    .text_color(theme::text_muted())
                    .child(SharedString::from(format!(
                        "{} · {}",
                        run.repo_name(),
                        run.folder_id()
                    ))),
            );
        for (question_index, question) in run.questions().into_iter().enumerate() {
            if question_index > 0 {
                column = column.child(div().h(px(1.)).w_full().bg(theme::hairline()));
            }
            column = column
                .child(
                    div()
                        .text_size(px(fonts.font_meta))
                        .text_color(theme::text_faint())
                        .child(format!("asked by {}", question.asked_by_label())),
                )
                .child(self.question_body(question, window, cx));
            for control in self.answer_controls(run, question, data, rows, window, cx) {
                column = column.child(control);
            }
            if let Some(line) = self.answer_line(run, question, cx) {
                column = column.child(line);
            }
        }
        div()
            .debug_selector(|| "flow-waiting-card".into())
            .w_full()
            .rounded(px(8.))
            .border_1()
            .border_color(theme::hairline())
            .bg(theme::panel())
            .px_3p5()
            .py_3()
            .flex()
            .child(column)
            .into_any_element()
    }

    /// The answer controls for one question, in the question's kind. Each
    /// control records its row so the cursor and the pointer share one map.
    fn answer_controls(
        &mut self,
        run: &Run,
        question: &Question,
        data: &BodyData,
        rows: &mut Vec<Row>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (fonts, accent, cursor) = (data.fonts, data.accent, data.cursor);
        let key = question_key(question);
        let issue = run.dir.clone();
        let path = question.path.clone();
        let folder = run.folder_id();
        let kind = question.control_kind();
        let choices = question.choices();
        let mut elements = Vec::new();
        match kind {
            QuestionKind::Approve | QuestionKind::Select => {
                for (option_index, choice) in choices.iter().enumerate() {
                    let answer = flow::choice_answer(choice);
                    let label = display_choice(choice);
                    // `approve` is a one-click single choice as much as
                    // `select` is: both read as one round marker, and only
                    // `multi` reads as a checkbox.
                    let radio = matches!(kind, QuestionKind::Approve | QuestionKind::Select);
                    let selector = format!("flow-answer-{folder}-{option_index}");
                    let control = div()
                        .id(gpui::ElementId::Name(selector.clone().into()))
                        .debug_selector(move || selector.clone())
                        .role(if radio {
                            Role::RadioButton
                        } else {
                            Role::Button
                        })
                        .aria_label(SharedString::from(label.clone()))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme::line_strong())
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::raised()))
                        .child(
                            div()
                                .size(px(15.))
                                .flex_shrink_0()
                                .when(radio, |style| style.rounded_full())
                                .when(!radio, |style| style.rounded(px(4.)))
                                .border_1()
                                .border_color(theme::text_muted()),
                        )
                        .child(
                            div()
                                .text_size(px(fonts.font_body))
                                .text_color(theme::text())
                                .child(SharedString::from(label)),
                        )
                        .into_any_element();
                    let sheet = cx.entity();
                    let click_issue = issue.clone();
                    let click_path = path.clone();
                    let click_answer = answer.clone();
                    let element = div()
                        .id(gpui::ElementId::Name(
                            format!("flow-answer-row-{folder}-{option_index}").into(),
                        ))
                        .w_full()
                        .on_click(move |_, window, cx| {
                            sheet.update(cx, |this, cx| {
                                this.clicked_row(
                                    RowAction::Answer {
                                        issue: click_issue.clone(),
                                        question: click_path.clone(),
                                        text: click_answer.clone(),
                                    },
                                    window,
                                    cx,
                                );
                            });
                        })
                        .child(control)
                        .into_any_element();
                    let action = RowAction::Answer {
                        issue: issue.clone(),
                        question: path.clone(),
                        text: answer,
                    };
                    record_row(&mut elements, rows, element, action, cursor);
                }
            }
            QuestionKind::Multi => {
                let picked = self
                    .multi
                    .entry(key.clone())
                    .or_insert_with(|| vec![false; choices.len()]);
                picked.resize(choices.len(), false);
                let picked_now = picked.clone();
                for (option_index, choice) in choices.iter().enumerate() {
                    let checked = picked_now.get(option_index).copied().unwrap_or(false);
                    let selector = format!("flow-multi-{folder}-{option_index}");
                    let control = div()
                        .id(gpui::ElementId::Name(selector.clone().into()))
                        .debug_selector(move || selector.clone())
                        .role(Role::CheckBox)
                        .aria_label(SharedString::from(display_choice(choice)))
                        .aria_toggled(if checked {
                            Toggled::True
                        } else {
                            Toggled::False
                        })
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme::line_strong())
                        .when(checked, |style| style.bg(theme::q_opt_selected()))
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::raised()))
                        .child(check_box(checked, fonts.icon_tiny, accent))
                        .child(
                            div()
                                .text_size(px(fonts.font_body))
                                .text_color(theme::text())
                                .child(SharedString::from(display_choice(choice))),
                        )
                        .into_any_element();
                    let sheet = cx.entity();
                    let toggle_key = key.clone();
                    let element = div()
                        .id(gpui::ElementId::Name(
                            format!("flow-multi-row-{folder}-{option_index}").into(),
                        ))
                        .w_full()
                        .on_click(move |_, window, cx| {
                            sheet.update(cx, |this, cx| {
                                this.clicked_row(
                                    RowAction::Toggle {
                                        key: toggle_key.clone(),
                                        index: option_index,
                                    },
                                    window,
                                    cx,
                                );
                            });
                        })
                        .child(control)
                        .into_any_element();
                    let action = RowAction::Toggle {
                        key: key.clone(),
                        index: option_index,
                    };
                    record_row(&mut elements, rows, element, action, cursor);
                }
                let any_picked = picked_now.iter().any(|picked| *picked);
                let submit = button(
                    gpui::ElementId::Name(format!("flow-submit-{folder}-btn").into()),
                    "Submit",
                    BtnKind::Chip,
                    accent,
                    fonts.font_control,
                    any_picked,
                    {
                        let sheet = cx.entity();
                        let key = key.clone();
                        let issue = issue.clone();
                        let path = path.clone();
                        let choices = choices.clone();
                        move |_, window, cx| {
                            sheet.update(cx, |this, cx| {
                                this.clicked_row(
                                    RowAction::SubmitMulti {
                                        key: key.clone(),
                                        issue: issue.clone(),
                                        question: path.clone(),
                                        choices: choices.clone(),
                                    },
                                    window,
                                    cx,
                                );
                            });
                        }
                    },
                );
                let action = RowAction::SubmitMulti {
                    key: key.clone(),
                    issue: issue.clone(),
                    question: path.clone(),
                    choices: choices.clone(),
                };
                let submit = div()
                    .debug_selector({
                        let selector = format!("flow-submit-{folder}");
                        move || selector.clone()
                    })
                    .child(submit)
                    .into_any_element();
                record_row(
                    &mut elements,
                    rows,
                    div().pt_1().child(submit).into_any_element(),
                    action,
                    cursor,
                );
            }
            QuestionKind::Text => {
                let text_field = self.text_field(&key, &issue, &path, window, cx);
                let filled = self
                    .text_drafts
                    .get(&key)
                    .is_some_and(|draft| !draft.trim().is_empty());
                elements.push(
                    div()
                        .id(gpui::ElementId::Name(
                            format!("flow-text-box-{folder}").into(),
                        ))
                        .debug_selector({
                            let selector = format!("flow-text-box-{folder}");
                            move || selector.clone()
                        })
                        .w_full()
                        .child(field::text(
                            gpui::ElementId::Name(format!("flow-text-input-{folder}").into()),
                            &text_field,
                            window,
                            cx,
                        ))
                        .into_any_element(),
                );
                let submit = button(
                    gpui::ElementId::Name(format!("flow-text-submit-{folder}-btn").into()),
                    "Submit",
                    BtnKind::Chip,
                    accent,
                    fonts.font_control,
                    filled,
                    {
                        let sheet = cx.entity();
                        let key = key.clone();
                        move |_, window, cx| {
                            sheet.update(cx, |this, cx| {
                                this.clicked_row(
                                    RowAction::SubmitText { key: key.clone() },
                                    window,
                                    cx,
                                );
                            });
                        }
                    },
                );
                let submit = div()
                    .debug_selector({
                        let selector = format!("flow-text-submit-{folder}");
                        move || selector.clone()
                    })
                    .child(submit)
                    .into_any_element();
                let element = div().pt_1().child(submit).into_any_element();
                record_row(
                    &mut elements,
                    rows,
                    element,
                    RowAction::SubmitText { key: key.clone() },
                    cursor,
                );
            }
        }
        elements
    }

    /// The line under a question: in flight, the door's outcome, or its
    /// refusal. Only the question that was answered shows it.
    fn answer_line(
        &self,
        run: &Run,
        question: &Question,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let state: &AnswerState = self.store.read(cx).flow().answer(&run.dir)?;
        if state.question() != Some(question.path.as_path()) {
            return None;
        }
        let line = state.line()?;
        let failed = state.failed();
        let fonts = self.store.read(cx).fonts;
        Some(
            div()
                .id("flow-answer-outcome")
                .debug_selector(|| "flow-answer-outcome".into())
                .pt_0p5()
                .text_size(px(fonts.font_meta))
                .text_color(if failed {
                    theme::DANGER
                } else {
                    theme::text_dim()
                })
                .child(SharedString::from(line))
                .into_any_element(),
        )
    }

    /// The question's markdown body as the app's one content surface: a
    /// read-only `LiveEditor`, created once per question file.
    fn question_body(
        &mut self,
        question: &Question,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = question_key(question);
        let text: SharedString = if question.body.trim().is_empty() {
            SharedString::from(question.title.clone())
        } else {
            SharedString::from(question.body.clone())
        };
        let editor = match self.editors.get(&key) {
            Some(editor) => editor.clone(),
            None => {
                let editor = read_only_document(text, window, cx);
                self.editors.insert(key.clone(), editor.clone());
                editor
            }
        };
        div()
            .debug_selector(|| "flow-question-body".into())
            .w_full()
            .child(render_read_only_document_in_card(&editor, cx))
            .into_any_element()
    }

    /// One run that moves: its phase and log, and the verbs the phase allows.
    fn moving_card(
        &mut self,
        run: &HostRun,
        data: &BodyData,
        rows: &mut Vec<Row>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (fonts, accent, cursor) = (data.fonts, data.accent, data.cursor);
        let folder = run.folder_id();
        let mut column = div().flex().flex_col().gap_2().w_full().min_w_0();
        column = column
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(fonts.font_name))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(SharedString::from(if run.title.is_empty() {
                                folder.clone()
                            } else {
                                run.title.clone()
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .debug_selector(|| "flow-phase".into())
                            .flex_shrink_0()
                            .text_size(px(fonts.font_meta))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(phase_color(run.phase))
                            .child(SharedString::from(phase_word(run.phase))),
                    ),
            )
            .child(
                div()
                    .text_size(px(fonts.font_mono_meta))
                    .text_color(theme::text_muted())
                    .child(SharedString::from(folder.clone())),
            );
        if let Some(error) = run.error.as_deref().filter(|error| !error.is_empty()) {
            column = column.child(
                div()
                    .text_size(px(fonts.font_meta))
                    .text_color(theme::DANGER)
                    .child(SharedString::from(error.to_owned())),
            );
        }
        if !run.log.is_empty() {
            let mut log = div()
                .debug_selector(|| "flow-log".into())
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .rounded(px(6.))
                .bg(theme::well())
                .px_2()
                .py_1p5()
                .font_family(SharedString::from(theme::code_font()))
                .text_size(px(fonts.font_mono_meta))
                .text_color(theme::text_dim());
            for line in run
                .log
                .iter()
                .skip(run.log.len().saturating_sub(FLOW_LOG_LINES))
            {
                log = log.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(line.clone())),
                );
            }
            column = column.child(log);
        }
        let offered: Vec<DriveAction> = [
            DriveAction::Pause,
            DriveAction::Resume,
            DriveAction::Abandon,
        ]
        .into_iter()
        .filter(|action| action.offer(run.phase) == Offer::Yes)
        .collect();
        if !offered.is_empty() {
            let mut buttons = div().flex().items_center().gap_2().pt_1();
            for action in offered {
                let element = button(
                    gpui::ElementId::Name(format!("flow-action-{}-{folder}", action.verb()).into()),
                    action.label(),
                    BtnKind::Chip,
                    accent,
                    fonts.font_control,
                    true,
                    {
                        let sheet = cx.entity();
                        let issue = run.issue.clone();
                        move |_, window, cx| {
                            sheet.update(cx, |this, cx| {
                                this.clicked_row(
                                    RowAction::Drive {
                                        issue: issue.clone(),
                                        action,
                                    },
                                    window,
                                    cx,
                                );
                            });
                        }
                    },
                );
                let element = div()
                    .debug_selector({
                        let selector = format!("flow-action-{}-{folder}", action.verb());
                        move || selector.clone()
                    })
                    .child(element)
                    .into_any_element();
                buttons = buttons.child(record(
                    rows,
                    element,
                    RowAction::Drive {
                        issue: run.issue.clone(),
                        action,
                    },
                    cursor,
                ));
            }
            column = column.child(buttons);
        }
        if let Some(state) = self.store.read(cx).flow().action(&run.issue)
            && let Some(line) = state.line()
        {
            let failed = state.failed();
            column = column.child(
                div()
                    .debug_selector(|| "flow-action-line".into())
                    .text_size(px(fonts.font_meta))
                    .text_color(if failed {
                        theme::DANGER
                    } else {
                        theme::text_dim()
                    })
                    .child(SharedString::from(line)),
            );
        }
        div()
            .debug_selector(|| "flow-moving-card".into())
            .w_full()
            .rounded(px(8.))
            .border_1()
            .border_color(theme::hairline())
            .bg(theme::panel())
            .px_3p5()
            .py_3()
            .flex()
            .child(column)
            .into_any_element()
    }
}

/// The inputs one frame of the sheet's body is built from: the door's
/// standing, both bands, and the type and accent every card draws with. Grouped
/// so the card builders stay readable and the cursor stays one value for the
/// whole frame.
struct BodyData<'a> {
    status: &'a FlowStatus,
    waiting: &'a [Run],
    moving: &'a [HostRun],
    repo_count: usize,
    fonts: theme::FontTokens,
    accent: Rgba,
    cursor: usize,
}

impl Render for FlowSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        // The card owns the keyboard while it is up, but an answer field
        // inside it owns it more narrowly: refocus the card only when focus
        // left the sheet entirely. A field's handle is the only other focus
        // this sheet puts in the tree, so the check is "the card or one of its
        // fields".
        //
        // This cannot ask the window's key context stack: a child entity
        // renders before the frame's dispatch tree exists, and the stack
        // needs it.
        let focused = window.focused(cx);
        let ours = focused.as_ref().is_some_and(|handle| {
            *handle == self.focus
                || self
                    .text_fields
                    .values()
                    .any(|field| field.read(cx).focus_handle(cx) == *handle)
        });
        if !ours {
            window.focus(&self.focus, cx);
        }
        let (status, waiting, moving, repo_count, fonts, accent) = {
            let store = self.store.read(cx);
            let flow = store.flow();
            (
                flow.status().clone(),
                flow.waiting().into_iter().cloned().collect::<Vec<Run>>(),
                flow.moving().into_iter().cloned().collect::<Vec<HostRun>>(),
                flow.repo_count(),
                store.fonts,
                store.accent.accent,
            )
        };
        // Question drafts and editors that left the board go with it.
        let live: std::collections::HashSet<QuestionKey> = waiting
            .iter()
            .flat_map(|run| run.questions().into_iter().map(question_key))
            .collect();
        self.editors.retain(|key, _| live.contains(key));
        self.text_fields.retain(|key, _| live.contains(key));
        self.text_targets.retain(|key, _| live.contains(key));
        self.multi.retain(|key, _| live.contains(key));

        let data = BodyData {
            status: &status,
            waiting: &waiting,
            moving: &moving,
            repo_count,
            fonts,
            accent,
            cursor: self.cursor,
        };
        let (children, rows) = self.body(&data, window, cx);
        self.rows = rows;
        if self.rows.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len() - 1;
        }

        let mut body = div()
            .id("flow-body")
            .debug_selector(|| "flow-body".into())
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px_4()
            .py_3();
        for child in children {
            body = body.child(child);
        }

        let (status_word, status_color) = match &status {
            FlowStatus::Looking => ("looking for warp-flow…".to_owned(), theme::text_muted()),
            FlowStatus::Ready => (
                format!("{} waiting · {} moving", waiting.len(), moving.len()),
                theme::text_muted(),
            ),
            FlowStatus::Absent { .. } => ("not running".to_owned(), theme::WARN),
        };

        div()
            .id("flow-sheet")
            .debug_selector(|| "flow-sheet".into())
            .role(Role::Dialog)
            .aria_label("Flow — warp-flow runs")
            .absolute()
            .inset_0()
            .occlude()
            .bg(palette::SCRIM)
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .on_click(cx.listener(|this, _, window, cx| this.close(window, cx)))
            .child(
                div()
                    .id("flow-card")
                    .debug_selector(|| "flow-card".into())
                    .key_context("Flow")
                    .track_focus(&self.focus)
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_key_down(cx.listener(|this, event, window, cx| {
                        if this.handle_key(event, window, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .on_action(cx.listener(|this, _: &FlowClose, window, cx| {
                        this.close(window, cx);
                    }))
                    .w(px(FLOW_CARD_WIDTH))
                    .max_w(relative(1.))
                    .max_h(relative(0.92))
                    .flex()
                    .flex_col()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(palette::line_strong())
                    .bg(palette::popup())
                    .shadow_lg()
                    .child(
                        div()
                            .flex_shrink_0()
                            .h(px(shell::scaled(56.)))
                            .px_4()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .border_b_1()
                            .border_color(palette::hairline())
                            .child(
                                div()
                                    .text_size(px(fonts.font_list_title))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child("Flow"),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .debug_selector(|| "flow-status".into())
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_size(px(fonts.font_meta))
                                    .text_color(status_color)
                                    .children(matches!(status, FlowStatus::Looking).then(|| {
                                        warpui_kit_core::motion::spinner(
                                            "flow-status-spinner",
                                            "icons/spinner.svg",
                                            px(fonts.icon_tiny),
                                            theme::text_muted(),
                                        )
                                    }))
                                    .child(SharedString::from(status_word)),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "flow-close".into())
                                    .flex_shrink_0()
                                    .child(button(
                                        gpui::ElementId::Name("flow-close-btn".into()),
                                        "Close",
                                        BtnKind::Chip,
                                        palette::accent(),
                                        fonts.font_control,
                                        true,
                                        cx.listener(|this, _, window, cx| this.close(window, cx)),
                                    )),
                            ),
                    )
                    .child(body.flex_1().min_h(px(0.)))
                    .child(
                        div()
                            .flex_shrink_0()
                            .h(px(shell::scaled(44.)))
                            .px_4()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .border_t_1()
                            .border_color(palette::hairline())
                            .text_size(px(fonts.font_meta))
                            .text_color(theme::text_faint())
                            .child(
                                div()
                                    .flex_1()
                                    .child("Starting a run stays warp-flow's own action."),
                            )
                            .child(warpui_kit_core::field::kbd("↑ ↓"))
                            .child(warpui_kit_core::field::kbd("Enter"))
                            .child(warpui_kit_core::field::kbd("Esc")),
                    ),
            )
            .into_any_element()
    }
}

/// One row the cursor can land on, and the body child its card is, so a step
/// scrolls the card into view.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    action: RowAction,
    card: usize,
}

/// What a row does when it is clicked or Enter lands on it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowAction {
    /// Write an answer (`approve`/`select` answer a click at once).
    Answer {
        issue: PathBuf,
        question: PathBuf,
        text: String,
    },
    /// Flip one `multi` checkbox.
    Toggle { key: QuestionKey, index: usize },
    /// Send the picked `multi` options.
    SubmitMulti {
        key: QuestionKey,
        issue: PathBuf,
        question: PathBuf,
        choices: Vec<String>,
    },
    /// Send the free-text field's value.
    SubmitText { key: QuestionKey },
    /// Pause, resume or abandon a moving run.
    Drive { issue: PathBuf, action: DriveAction },
    /// Look for warp-flow again.
    Retry,
}

impl RowAction {
    /// A stable word for the probe's cursor.
    fn label(&self) -> String {
        match self {
            Self::Answer { text, .. } => format!("answer:{text}"),
            Self::Toggle { index, .. } => format!("toggle:{index}"),
            Self::SubmitMulti { .. } => "submit:multi".to_owned(),
            Self::SubmitText { .. } => "submit:text".to_owned(),
            Self::Drive { action, .. } => format!("drive:{}", action.verb()),
            Self::Retry => "retry".to_owned(),
        }
    }
}

/// Record one action row and wrap its control in the cursor's emphasis when it
/// is the current one. `rows.len()` before the push is the row's index, so the
/// caller never has to count.
fn record_row(
    elements: &mut Vec<AnyElement>,
    rows: &mut Vec<Row>,
    element: AnyElement,
    action: RowAction,
    cursor: usize,
) {
    elements.push(record(rows, element, action, cursor));
}

/// [`record_row`] for a caller that seats the wrapped element itself (the
/// moving card's action row).
fn record(
    rows: &mut Vec<Row>,
    element: AnyElement,
    action: RowAction,
    cursor: usize,
) -> AnyElement {
    let current = rows.len() == cursor;
    rows.push(Row {
        action,
        card: usize::MAX,
    });
    row_wrap(element, current)
}

/// Wrap one control in the cursor's emphasis when it is the current row. The
/// kit button carries no focus ring of its own, so the cursor paints one
/// around whichever control it is on.
fn row_wrap(element: AnyElement, current: bool) -> AnyElement {
    div()
        .w_full()
        .p(px(2.))
        .when(current, |style| style.rounded(px(8.)).bg(theme::raised()))
        .child(element)
        .into_any_element()
}

/// One section heading with its count.
fn section_head(title: &str, count: usize, fonts: theme::FontTokens) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .pt_1()
        .pb_0p5()
        .child(
            div()
                .text_size(px(fonts.font_meta))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_dim())
                .child(SharedString::from(title.to_owned())),
        )
        .child(
            div()
                .text_size(px(fonts.font_meta))
                .text_color(theme::text_muted())
                .child(SharedString::from(count.to_string())),
        )
        .into_any_element()
}

/// The small checkbox a `multi` row leads with.
fn check_box(checked: bool, size: f32, accent: Rgba) -> AnyElement {
    div()
        .flex_shrink_0()
        .size(px(15.))
        .rounded(px(4.))
        .border_1()
        .border_color(if checked {
            accent
        } else {
            theme::line_strong()
        })
        .when(checked, |style| style.bg(accent))
        .flex()
        .items_center()
        .justify_center()
        .children(
            checked.then(|| crate::ui::icon("icons/check.svg", px(size), palette::on_accent())),
        )
        .into_any_element()
}

/// A question's stable key: its file path, the one identity the door and the
/// answer verb share.
fn question_key(question: &Question) -> QuestionKey {
    question.path.to_string_lossy().into_owned()
}

/// An option label in the card's sentence case: `yes` reads as `Yes`, an
/// engine-authored label keeps its spelling.
fn display_choice(choice: &str) -> String {
    let trimmed = choice.trim();
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(first) if first.is_lowercase() => {
            format!("{}{}", first.to_uppercase(), chars.as_str())
        }
        _ => trimmed.to_owned(),
    }
}

/// The quiet empty line's second half: what warp-flow is watching.
fn watching_line(repo_count: usize) -> String {
    match repo_count {
        0 => format!("{} is watching no repositories yet.", flow::APP),
        1 => format!("{} is watching 1 repository.", flow::APP),
        count => format!("{} is watching {count} repositories.", flow::APP),
    }
}

/// The moving card's phase in one word, with an ellipsis where the engine is
/// moving.
fn phase_word(phase: Phase) -> &'static str {
    match phase {
        Phase::Starting => "Starting…",
        Phase::Running => "Running",
        Phase::Pausing => "Pausing…",
        Phase::Paused => "Paused",
        Phase::Waiting => "Waiting on you",
        Phase::Finishing => "Finishing…",
        Phase::Done => "Done",
        Phase::Blocked => "Blocked",
        Phase::Abandoned => "Abandoned",
        Phase::Failed => "Failed",
        Phase::Unknown => "Unknown phase",
    }
}

/// The phase's colour, from the run screen's own state palette.
fn phase_color(phase: Phase) -> Rgba {
    match phase {
        Phase::Starting | Phase::Running | Phase::Pausing => theme::accent_ink(),
        Phase::Paused => theme::accent_ink(),
        Phase::Waiting => theme::WARN,
        Phase::Finishing => theme::text_muted(),
        Phase::Done => theme::ADD,
        Phase::Blocked | Phase::Abandoned | Phase::Failed | Phase::Unknown => theme::DANGER,
    }
}
