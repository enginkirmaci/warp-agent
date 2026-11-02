//! The WarpAgent palette: the family palette, type and scale from
//! warpui-kit-core, re-exported under the app's `theme::` paths, plus the
//! accent-preset swap and the app-only sidebar metrics. Surfaces step chrome <
//! canvas < card < popup < chip; font/icon size tokens ride one factor so the
//! shared text size rescales the whole UI.

pub use warpui_kit_core::fonts::{FontTokens, active_tokens, code_font, field_size, ui_font};
// every shared color const stays reachable as `theme::X`
// the fixed semantic colors stay constants (they never follow the palette),
// and the accent preset system rides along
// the fixed semantic colors stay constants (they never follow the palette),
// and the accent preset system rides along
pub use warpui_kit_core::palette::{
    ADD, AccentColors, BEHIND, BRAND_BORDER, CHANGES, DANGER, DEL, HOVER, RUNNING, SUCCESS_BRIGHT,
    WARN, WORKING, current_ramp, normalize_accent, tint,
};
// ---- gpui-kit theme mapping ------------------------------------------------

/// Map the WarpAgent palette onto gpui-kit's shadcn-style theme tokens so kit
/// components (Input/Textarea/TextView/popovers) render in our colors.
/// The accent family and font tokens are parameters so a settings-page preset
/// swap can re-run the mapping (`Theme::update` refreshes every window). Call
/// after [`gpui_kit::init`], before opening windows.
pub fn apply_kit_theme(accent: AccentColors, fonts: &FontTokens, cx: &mut gpui::App) {
    warpui_kit_core::apply_kit_theme(accent, fonts, window_chrome(), cx);
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
// exactly that reason, and the names stay the apps' own vocabulary.

/// The primary fill.
pub fn accent() -> gpui::Rgba {
    current_ramp().accent
}
/// The accent as ink: links, focus rings, active pills.
pub fn accent_ink() -> gpui::Rgba {
    current_ramp().accent_ink
}
/// The tinted tile behind the brand mark.
pub fn accent_soft() -> gpui::Rgba {
    current_ramp().accent_soft
}
/// Quiet dividers.
pub fn border_light() -> gpui::Rgba {
    current_ramp().line_soft
}
/// The default separator.
pub fn hairline() -> gpui::Rgba {
    current_ramp().line
}
/// Control borders.
pub fn line_strong() -> gpui::Rgba {
    current_ramp().line_strong
}
/// Text on a filled `accent()`.
pub fn on_accent() -> gpui::Rgba {
    current_ramp().on_accent
}
/// A card, a tile, the detail pane.
pub fn panel() -> gpui::Rgba {
    current_ramp().panel
}
/// Menus, dialogs and tooltips.
pub fn popup() -> gpui::Rgba {
    current_ramp().overlay
}
/// The selected row, hover fills, active segments.
pub fn raised() -> gpui::Rgba {
    current_ramp().raised
}
/// The window canvas.
pub fn surface() -> gpui::Rgba {
    current_ramp().base
}
/// Names and values.
pub fn text() -> gpui::Rgba {
    current_ramp().text
}
/// Secondary copy.
pub fn text_dim() -> gpui::Rgba {
    current_ramp().text_dim
}
/// Timestamps and hints.
pub fn text_muted() -> gpui::Rgba {
    current_ramp().text_muted
}
/// Fields, diffs and code: the recessed step.
pub fn well() -> gpui::Rgba {
    current_ramp().well
}
/// The titlebar, the rail and the chrome.
pub fn window_chrome() -> gpui::Rgba {
    current_ramp().chrome
}

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
pub fn settings_card() -> gpui::Rgba {
    panel()
}
pub fn text_faint() -> gpui::Rgba {
    text_muted()
}
