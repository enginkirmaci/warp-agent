//! The WarpAgent palette: the family palette and fonts from warpui-kit-core,
//! re-exported under the app's `theme::` paths, plus the accent-preset swap
//! and the app-only sidebar metrics. Surfaces step canvas < titlebar < card
//! < popup < field; font/icon size tokens ride one factor so the settings
//! text-size variant rescales the whole UI.

pub use warpui_kit_core::fonts::{
    FONT_COMPACT, FONT_LARGE, FONT_NORMAL, FontTokens, code_font, font_tokens,
    normalize_font_scale, ui_font,
};
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

pub const SIDEBAR_MIN_WIDTH: f32 = 180.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 480.0;
pub const SIDEBAR_DEFAULT_WIDTH: f32 = 250.0;
pub const SPLITTER_WIDTH: f32 = 12.0;
