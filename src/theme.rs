//! The WarpAgent palette: the family palette, type and scale from
//! warpui-kit-core, re-exported under the app's `theme::` paths, plus the
//! accent-preset swap and the app-only sidebar metrics. Surfaces step chrome <
//! canvas < card < popup < chip; font/icon size tokens ride one factor so the
//! shared text size rescales the whole UI.

pub use warpui_kit_core::fonts::{FontTokens, active_tokens, code_font, field_size, ui_font};
// every shared color const stays reachable as `theme::X`
pub use warpui_kit_core::palette::*;

// ---- gpui-kit theme mapping ------------------------------------------------

/// Map the WarpAgent palette onto gpui-kit's shadcn-style theme tokens so kit
/// components (Input/Textarea/TextView/popovers) render in our colors.
/// The accent family and font tokens are parameters so a settings-page preset
/// swap can re-run the mapping (`Theme::update` refreshes every window). Call
/// after [`gpui_kit::init`], before opening windows.
pub fn apply_kit_theme(accent: AccentColors, fonts: &FontTokens, cx: &mut gpui::App) {
    warpui_kit_core::apply_kit_theme(accent, fonts, WINDOW_CHROME, cx);
}

// ---- metrics ---------------------------------------------------------------

// the tree's rows are short and the grid wants the width, so the list starts
// at the shell's minimum
pub const SIDEBAR_MIN_WIDTH: f32 = warpui_kit_core::shell::LIST_MIN_WIDTH;
pub const SIDEBAR_MAX_WIDTH: f32 = warpui_kit_core::shell::LIST_MAX_WIDTH;
pub const SIDEBAR_DEFAULT_WIDTH: f32 = warpui_kit_core::shell::LIST_MIN_WIDTH;
pub const SPLITTER_WIDTH: f32 = 4.0;
