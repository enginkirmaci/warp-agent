//! The filtered picker: a select-style trigger over a filtered, virtualized
//! list of the server catalog. A standalone entity so filter typing repaints
//! only the picker, never the settings page around it.

use crate::store::Store;
use crate::theme;
use crate::ui::controls::popover_card;
use gpui::prelude::*;
use gpui::{div, px, uniform_list, App, Context, Entity, Role, SharedString, Window};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::Sizable as _;

const ROW_HEIGHT: f32 = 28.;
const VISIBLE_ROWS: usize = 10;

enum PickerRow {
    Default,
    Provider(String),
    Model(String),
    Message(String),
}

fn model_provider(model: &str) -> &str {
    model.split_once('/').map(|(provider, _)| provider).unwrap_or("")
}

fn model_name(model: &str) -> &str {
    model.split_once('/').map(|(_, name)| name).unwrap_or(model)
}

pub struct FilteredPicker {
    state: Entity<InputState>,
    filter: Entity<InputState>,
    store: Entity<Store>,
    open: bool,
    _subscriptions: Vec<gpui::Subscription>,
}

impl FilteredPicker {
    pub fn new(
        state: Entity<InputState>,
        store: Entity<Store>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter models"));
        let sub_filter = cx.observe(&filter, |_, _, cx| cx.notify());
        let sub_state = cx.observe(&state, |_, _, cx| cx.notify());
        let sub_store = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            state,
            filter,
            store,
            open: false,
            _subscriptions: vec![sub_filter, sub_state, sub_store],
        }
    }

    fn rows(&self, cx: &App) -> Vec<PickerRow> {
        let filter = self.filter.read(cx).value().trim().to_lowercase();
        let mut models: Vec<String> = self
            .store
            .read(cx)
            .models
            .iter()
            .filter(|model| {
                let model = model.trim();
                !model.is_empty()
                    && !model.starts_with('(')
                    && (filter.is_empty() || model.to_lowercase().contains(&filter))
            })
            .cloned()
            .collect();
        models.sort_by(|a, b| {
            model_provider(a)
                .to_lowercase()
                .cmp(&model_provider(b).to_lowercase())
                .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
        });
        models.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

        let mut rows = Vec::with_capacity(models.len() + 4);
        rows.push(PickerRow::Default);
        if models.is_empty() {
            rows.push(PickerRow::Message(
                "No models yet. Start a workspace to load models.".to_string(),
            ));
        } else {
            let mut last_provider = String::new();
            for model in models {
                let provider = model_provider(&model).to_string();
                if !provider.is_empty() && provider != last_provider {
                    rows.push(PickerRow::Provider(provider.to_uppercase()));
                    last_provider = provider;
                }
                rows.push(PickerRow::Model(model));
            }
        }
        rows
    }

    fn provider_row(id: gpui::ElementId, label: String, font_size: f32) -> gpui::AnyElement {
        div()
            .id(id)
            .role(Role::Label)
            .aria_label(SharedString::from(label.clone()))
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_2()
            .flex()
            .items_center()
            .text_size(px(font_size - 1.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(theme::TEXT_FAINT)
            .child(label)
            .into_any_element()
    }

    fn message_row(id: gpui::ElementId, label: String, font_size: f32) -> gpui::AnyElement {
        div()
            .id(id)
            .role(Role::Label)
            .aria_label(SharedString::from(label.clone()))
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_2()
            .flex()
            .items_center()
            .text_size(px(font_size))
            .text_color(theme::TEXT_FAINT)
            .child(label)
            .into_any_element()
    }

    fn model_row(
        id: gpui::ElementId,
        label: String,
        selected: bool,
        value: String,
        state: Entity<InputState>,
        accent: gpui::Rgba,
        font_size: f32,
        icon_size: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id(id)
            .role(Role::ListBoxOption)
            .aria_label(SharedString::from(label.clone()))
            .aria_selected(selected)
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_2()
            .rounded_md()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .when(selected, |row| row.bg(theme::HOVER))
            .hover(|s| s.bg(theme::HOVER))
            .on_click(cx.listener(move |this, _, window, cx| {
                state.update(cx, |state, cx| state.set_value(value.as_str(), window, cx));
                this.open = false;
                cx.notify();
            }))
            .child(
                div()
                    .size(px(icon_size))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(selected, |marker| {
                        marker.child(crate::ui::icon("icons/check.svg", px(icon_size), accent))
                    }),
            )
            .child(
                div()
                    .truncate()
                    .text_size(px(font_size))
                    .text_color(theme::TEXT)
                    .child(label),
            )
            .into_any_element()
    }
}

impl Render for FilteredPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fonts = self.store.read(cx).fonts;
        let accent = self.store.read(cx).accent.accent;
        let selected = self.state.read(cx).value().trim().to_string();
        let selected_label = if selected.is_empty() {
            "Server default".to_string()
        } else {
            model_name(&selected).to_string()
        };
        let selected_provider = model_provider(&selected).to_string();
        let rows = self.rows(cx);
        let list_height = rows.len().min(VISIBLE_ROWS) as f32 * ROW_HEIGHT;
        let state = self.state.clone();
        let rows_selected = selected.clone();
        let font_small = fonts.font_small;
        let icon_small = fonts.icon_small;
        let list = uniform_list(
            "model-dd-list",
            rows.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|ix| match &rows[ix] {
                        PickerRow::Default => Self::model_row(
                            gpui::ElementId::Name("model-default".into()),
                            "Server default".to_string(),
                            rows_selected.is_empty(),
                            String::new(),
                            state.clone(),
                            accent,
                            font_small,
                            icon_small,
                            cx,
                        ),
                        PickerRow::Provider(provider) => Self::provider_row(
                            gpui::ElementId::Name(
                                SharedString::from(format!("model-provider-{provider}")),
                            ),
                            provider.clone(),
                            font_small,
                        ),
                        PickerRow::Model(model) => Self::model_row(
                            gpui::ElementId::Name(SharedString::from(format!("model-{model}"))),
                            model_name(model).to_string(),
                            model.eq_ignore_ascii_case(&rows_selected),
                            model.clone(),
                            state.clone(),
                            accent,
                            font_small,
                            icon_small,
                            cx,
                        ),
                        PickerRow::Message(message) => Self::message_row(
                            gpui::ElementId::Name("model-message".into()),
                            message.clone(),
                            font_small,
                        ),
                    })
                    .collect()
            }),
        )
        .w_full()
        .h(px(list_height));

        div()
            .id("model-dd-root")
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_w_0()
            .when(self.open, |root| {
                root.on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.open = false;
                    cx.notify();
                }))
            })
            .child(
                div()
                    .id("model-dd-toggle")
                    .role(Role::ComboBox)
                    .aria_label("Default model")
                    .aria_value(SharedString::from(selected_label.clone()))
                    .aria_expanded(self.open)
                    .w_full()
                    .min_h(px(26.))
                    .cursor_pointer()
                    .rounded_lg()
                    .border_1()
                    .border_color(if self.open { accent } else { theme::HAIRLINE })
                    .bg(theme::INPUT)
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .hover(|s| s.border_color(theme::TEXT_MUTED))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open = !this.open;
                        if this.open {
                            let filter = this.filter.clone();
                            filter.update(cx, |state, cx| {
                                state.set_value("", window, cx);
                                state.focus(window, cx);
                            });
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(!selected_provider.is_empty(), |row| {
                                row.child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded_sm()
                                        .bg(theme::CHIP)
                                        .text_size(px(fonts.font_micro))
                                        .text_color(theme::TEXT_MUTED)
                                        .child(selected_provider.clone()),
                                )
                            })
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(fonts.font_body))
                                    .text_color(if selected.is_empty() {
                                        theme::TEXT_MUTED
                                    } else {
                                        theme::TEXT
                                    })
                                    .child(selected_label.clone()),
                            ),
                    )
                    .child(crate::ui::icon(
                        "icons/chevron-down.svg",
                        px(fonts.icon_small),
                        theme::TEXT_MUTED,
                    )),
            )
            .when(self.open, |root| {
                // the kit Input has no a11y of its own — the wrapper carries the
                // role; an AT click lands at its center, the input itself
                root.child(
                    popover_card()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .id("model-dd-filter")
                                .role(Role::SearchInput)
                                .aria_label("Filter models")
                                .aria_placeholder("Filter models")
                                .child(Input::new(&self.filter).small()),
                        )
                        .child(list),
                )
            })
    }
}
