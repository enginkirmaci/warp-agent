//! The WarpAgent palette: the family palette, type and scale from
//! warpui-kit-core, re-exported under the app's `theme::` paths, plus the
//! accent-preset swap and the app-only sidebar metrics. Surfaces step chrome <
//! canvas < card < popup < chip; font/icon size tokens ride one factor so the
//! shared text size rescales the whole UI.

pub use warpui_kit_core::fonts::{FontTokens, active_tokens, code_font, ui_font};
// every shared color const stays reachable as `theme::X`; the fixed semantic
// colors stay constants (they never follow the palette), while surfaces and
// the accent family come from the live palette ramp
pub use warpui_kit_core::palette::{
    ADD, AccentColors, BEHIND, BRANCH, CHANGES, DANGER, DEL, HOVER, RUNNING, SUCCESS_BRIGHT, WARN,
    WORKING, tint,
};
// ---- gpui-kit theme mapping ------------------------------------------------

/// Map the WarpAgent palette onto gpui-kit's shadcn-style theme tokens so kit
/// components (Input/Textarea/TextView/popovers) render in our colors.
/// The accent family and font tokens are parameters so a palette or text-size
/// change can re-run the mapping (`Theme::update` refreshes every window).
/// Call after [`gpui_kit::init`], before opening windows.
pub fn apply_kit_theme(accent: AccentColors, fonts: &FontTokens, cx: &mut gpui::App) {
    warpui_kit_core::apply_kit_theme(accent, fonts, window_chrome(), cx);
}

/// The live accent family: the palette ramp's own accent, so switching the
/// shared palette recolors fills, rings and selection in the same click. The
/// app keeps no accent preset of its own.
pub fn accent_colors() -> AccentColors {
    warpui_kit_core::palette::current_ramp().accent_colors()
}

// ---- metrics ---------------------------------------------------------------

// the tree's rows are short and the grid wants the width, so the list starts
// at the shell's minimum
pub const SIDEBAR_MIN_WIDTH: f32 = warpui_kit_core::shell::LIST_MIN_WIDTH;
pub const SIDEBAR_MAX_WIDTH: f32 = warpui_kit_core::shell::LIST_MAX_WIDTH;
pub const SIDEBAR_DEFAULT_WIDTH: f32 = warpui_kit_core::shell::LIST_MIN_WIDTH;
pub const SPLITTER_WIDTH: f32 = 4.0;

// ---- the live palette ------------------------------------------------------
// Every surface comes through here, so switching the palette in one settings
// page moves all of them. These were constants; they are functions now for
// exactly that reason, and the names stay the apps' own vocabulary — the
// readers themselves are the kit's, re-exported.

pub use warpui_kit_core::palette::{
    accent_ink, accent_soft, border_light, hairline, line_strong, panel, popup, raised, surface,
    text, text_dim, text_muted, well, window_chrome,
};

// the app's older spellings, one-liners over the readers above
pub fn brand_bg() -> gpui::Rgba {
    accent_soft()
}
pub fn bubble_assistant() -> gpui::Rgba {
    panel()
}
pub fn bubble_user() -> gpui::Rgba {
    raised()
}
pub fn chip() -> gpui::Rgba {
    raised()
}
pub fn overlay() -> gpui::Rgba {
    popup()
}
pub fn q_opt_selected() -> gpui::Rgba {
    raised()
}
pub fn text_faint() -> gpui::Rgba {
    text_muted()
}
