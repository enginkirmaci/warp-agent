//! The DevTools palette: the SukiUI dark theme ramp plus the ChipPalette hues,
//! shared with the DevTools app so both desktop clients read as one family.
//! Surfaces step canvas < titlebar < card < popup < field; font/icon size
//! tokens ride one factor so the settings text-size variant rescales the whole
//! UI.

use gpui::{hsla, rgba, App, Hsla, Rgba};

/// const-friendly rgba: gpui's `rgba()` is not const in this rev.
const fn c(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 24) & 0xFF) as f32 / 255.0,
        g: ((hex >> 16) & 0xFF) as f32 / 255.0,
        b: ((hex >> 8) & 0xFF) as f32 / 255.0,
        a: (hex & 0xFF) as f32 / 255.0,
    }
}

// ---- font resolution -------------------------------------------------------
// The brand families (Inter, DejaVu Sans Mono) are not installed everywhere;
// naming a missing family makes gpui fall back per text run and skips the
// kit's own probe (an explicitly-set family is used as-is). Resolve once at
// startup against the real font list so window, kit and code blocks agree.

const UI_FONT_CANDIDATES: &[&str] = &["Inter", "Adwaita Sans", "Segoe UI", "SF Pro Text", "Cantarell", "DejaVu Sans"];
const CODE_FONT_CANDIDATES: &[&str] = &["DejaVu Sans Mono", "Noto Sans Mono", "JetBrainsMono Nerd Font", "Liberation Mono", "Cascadia Mono", "Menlo", "Consolas", "Monaco"];
/// Virtual family gpui always resolves; the last resort for both stacks.
const SYSTEM_FONT: &str = ".SystemUIFont";

static UI_FONT_RESOLVED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static CODE_FONT_RESOLVED: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Pick the first installed family per stack; call once at startup, before
/// any text is laid out.
pub fn resolve_fonts(cx: &App) {
    let installed = cx.text_system().all_font_names();
    let pick = |candidates: &[&str]| {
        candidates
            .iter()
            .find(|c| installed.iter().any(|name| name == *c))
            .map(|c| c.to_string())
            .unwrap_or_else(|| SYSTEM_FONT.to_string())
    };
    let _ = UI_FONT_RESOLVED.set(pick(UI_FONT_CANDIDATES));
    let _ = CODE_FONT_RESOLVED.set(pick(CODE_FONT_CANDIDATES));
    tracing::info!(
        "UI font {:?}, code font {:?}",
        ui_font(),
        code_font()
    );
}

/// The UI family to lay everything out in.
pub fn ui_font() -> &'static str {
    UI_FONT_RESOLVED.get().map(|s| s.as_str()).unwrap_or(FONT_UI)
}

/// The mono family for code chips, details folds and diffs.
pub fn code_font() -> &'static str {
    CODE_FONT_RESOLVED.get().map(|s| s.as_str()).unwrap_or(CODE_FONT)
}

// ---- gpui-kit theme mapping ------------------------------------------------

/// Map the WarpAgent palette onto gpui-kit's shadcn-style theme tokens so kit
/// components (Input/Textarea/TextView/popovers) render in our colors.
/// The accent family and font tokens are parameters so a settings-page preset
/// swap can re-run the mapping (`Theme::update` refreshes every window). Call
/// after [`gpui_kit::init`], before opening windows.
pub fn apply_kit_theme(accent: AccentColors, fonts: &FontTokens, cx: &mut App) {
    use gpui_kit::component::{Theme, ThemeMode};

    resolve_fonts(cx);
    // A mode flip inside the color-edit closure makes the kit reload the
    // registered theme's colors AFTER the closure (Theme::update docs),
    // wiping every override — flip first, then edit.
    Theme::update(cx, |t| t.mode = ThemeMode::Dark);
    Theme::update(cx, |t| {
        t.background = SURFACE.into();
        t.foreground = TEXT.into();
        t.primary = accent.accent.into();
        t.primary_foreground = accent.on.into();
        t.secondary = CHIP.into();
        t.secondary_foreground = TEXT.into();
        t.muted = INPUT.into();
        t.muted_foreground = TEXT_MUTED.into();
        t.accent = HOVER;
        t.accent_foreground = TEXT.into();
        t.danger = DANGER.into();
        t.danger_foreground = ON_ACCENT.into();
        t.border = HAIRLINE.into();
        // the kit paints an input's border with `input` and its fill with
        // `input` mixed over the surface, so a fill-coloured token made the
        // border vanish on popups; the hairline gives fields a real edge
        t.input = HAIRLINE.into();
        t.ring = accent.accent.into();
        t.selection = selection(&accent).into();
        t.caret = TEXT.into();
        t.link = accent.accent.into();
        t.popover = POPUP.into();
        t.popover_foreground = TEXT.into();
        t.sidebar = WINDOW_CHROME.into();
        t.sidebar_foreground = TEXT.into();
        t.sidebar_border = HAIRLINE.into();

        // our tiles/cards sit on 8–12px corners
        t.radius = gpui::px(8.);
        t.radius_lg = gpui::px(12.);

        t.font_family = gpui::SharedString::from(ui_font());
        t.mono_font_family = gpui::SharedString::from(code_font());
        // the rem base: kit components size text/padding in rem — ride the
        // same factor as our own tokens (font_small is the 12px base)
        let factor = fonts.font_small / 12.0;
        t.font_size = gpui::px(16. * factor);
        t.mono_font_size = gpui::px(13. * factor);

        t.highlight_theme = highlight_theme();
    });
}

// ---- highlight theme -------------------------------------------------------

/// Syntax token colors for markdown code fences and diff lines: the DevTools
/// mapping (comment faint italic, string green, number amber, keyword purple,
/// type amber-bright, function blue, attribute gold). The kit's HighlightTheme
/// deserializes from the Zed theme JSON format — ThemeStyle fields are private,
/// so building the struct by hand is not possible.
const HIGHLIGHT_THEME_JSON: &str = r##"{
  "name": "devtools",
  "appearance": "dark",
  "style": {
    "editor.foreground": "#EDEDF0",
    "syntax": {
      "attribute": { "color": "#F5B400" },
      "boolean": { "color": "#FA8C16" },
      "comment": { "color": "#85878F", "font_style": "italic" },
      "comment.doc": { "color": "#85878F", "font_style": "italic" },
      "constant": { "color": "#FA8C16" },
      "constructor": { "color": "#FFAB40" },
      "function": { "color": "#6E9BF7" },
      "keyword": { "color": "#A78BFA" },
      "number": { "color": "#FA8C16" },
      "operator": { "color": "#A5A7AF" },
      "preproc": { "color": "#A78BFA" },
      "property": { "color": "#EDEDF0" },
      "punctuation": { "color": "#A5A7AF" },
      "punctuation.bracket": { "color": "#A5A7AF" },
      "punctuation.delimiter": { "color": "#A5A7AF" },
      "punctuation.list_marker": { "color": "#A5A7AF" },
      "string": { "color": "#4EC94E" },
      "string.escape": { "color": "#FFAB40" },
      "tag": { "color": "#F5B400" },
      "type": { "color": "#FFAB40" },
      "variable": { "color": "#EDEDF0" },
      "variable.special": { "color": "#FFAB40" }
    }
  }
}"##;

static HIGHLIGHT_THEME: std::sync::OnceLock<std::sync::Arc<gpui_kit::component::highlighter::HighlightTheme>> =
    std::sync::OnceLock::new();

/// The installed token theme; fixed hues, so built once and pointer-stable for
/// the process lifetime (the highlight caches key on the Arc).
fn highlight_theme() -> std::sync::Arc<gpui_kit::component::highlighter::HighlightTheme> {
    HIGHLIGHT_THEME
        .get_or_init(|| {
            serde_json::from_str(HIGHLIGHT_THEME_JSON).unwrap_or_else(|e| {
                tracing::warn!("highlight theme JSON unparsable ({e}); using kit default");
                gpui_kit::component::highlighter::HighlightTheme::default_dark()
            })
        })
        .clone()
}

// ---- surfaces --------------------------------------------------------------
// One neutral ramp from the window canvas up to fields, every step a few
// points apart so panels read as raised layers:
// main card < chrome < tile/card < popup < field.

pub const SURFACE: Rgba = c(0x1C1D20FF); // main-area card
pub const CARD: Rgba = c(0x26272BFF); // tile surface, restore bar, error dialog
pub const WINDOW_CHROME: Rgba = c(0x232428FF); // titlebar, sidebar, ring around the main card
pub const SETTINGS_BG: Rgba = c(0x232428FF); // settings sheet
pub const SETTINGS_CARD: Rgba = c(0x26272BFF);
pub const POPUP: Rgba = c(0x2B2C31FF); // popovers, menus, tooltips
pub const INPUT: Rgba = c(0x2E2F35FF);
pub const HAIRLINE: Rgba = c(0x3A3B42FF);
pub const BORDER_LIGHT: Rgba = c(0x2A2B30FF); // quiet dividers
pub const TILE_HEADER_SEPARATOR: Rgba = BORDER_LIGHT;
pub const CHIP: Rgba = c(0x32333AFF);
pub const DIFF_BG: Rgba = c(0x202125FF); // inset code/diff/fold body (16% black over CARD)
pub const HOVER: Hsla = hsla(0.0, 0.0, 1.0, 0.07);

pub const TEXT: Rgba = c(0xEDEDF0FF);
pub const TEXT_DIM: Rgba = c(0xC6C7CEFF); // secondary body text
pub const TEXT_MUTED: Rgba = c(0xA5A7AFFF); // labels, metadata
pub const TEXT_FAINT: Rgba = c(0x85878FFF); // hints, quietest meta

/// Text selection in fields: the accent at 25% alpha.
pub fn selection(accent: &AccentColors) -> Rgba {
    let a = accent.accent;
    Rgba { r: a.r, g: a.g, b: a.b, a: 0.25 }
}

// ---- chat surfaces -------------------------------------------------------
// DevTools has no chat; these derive from the same ramp (user bubble = chip
// tone, assistant bubble = popup tone raised off the tile card).

pub const BUBBLE_USER: Rgba = CHIP;
pub const BUBBLE_ASSISTANT: Rgba = POPUP;
pub const Q_OPT_SELECTED: Rgba = CHIP;
pub const BUBBLE_QUEUED: Rgba = WINDOW_CHROME;
pub const QUEUED_BORDER: Rgba = HAIRLINE;
pub const FLYOUT_SURFACE: Rgba = c(0x1C1D20B3); // canvas dark at 0.7 alpha over the chat

// ---- fixed semantic colors (never follow the accent preset) --------------
// The DevTools ChipPalette hues.

pub const DANGER: Rgba = c(0xF5222DFF);
pub const WARN: Rgba = c(0xFA8C16FF);
pub const ADD: Rgba = c(0x4EC94EFF);
pub const DEL: Rgba = DANGER;
pub const WORKING: Rgba = ADD; // working dot + sidebar spinner stay green
pub const RUNNING: Rgba = ADD; // sidebar running indicators

/// Tinted wash for a bright hue — the universal chip background and the
/// destructive/close hover (the ChipPalette TintAlpha).
pub fn tint(color: Rgba) -> Hsla {
    let h: Hsla = color.into();
    Hsla { a: 0.18, ..h }
}

// ---- code ----------------------------------------------------------------

pub const CODE_FG: Rgba = TEXT;

// ---- diffs (the renderer in ui/diff.rs draws from here) -------------------

pub const DIFF_GUTTER: Rgba = c(0xB4B4B49E); // line-number gutter text
/// Full-width bands; the gutter cell over the line numbers paints the same
/// shade so an edited row reads as one flat band.
pub const DIFF_ADD_BG: Hsla = hsla(0.28, 0.50, 0.45, 0.22);
pub const DIFF_DEL_BG: Hsla = hsla(0.012, 0.70, 0.48, 0.24);
pub const DIFF_HUNK_BG: Hsla = hsla(0.0, 0.0, 0.6, 0.10);

// ---- accent presets ------------------------------------------------------

pub struct AccentPreset {
    pub key: &'static str,
    /// rgb hex, one shared by accent + success
    pub accent: u32,
    pub dim: u32,
    pub on: u32,
    pub success_dim: u32,
}

/// The first preset is the default (`normalize_accent` fallback): the DevTools
/// Suki blue.
pub const ACCENT_PRESETS: [AccentPreset; 8] = [
    AccentPreset { key: "blue", accent: 0x7090CF, dim: 0x1E2C42, on: 0xFFFFFF, success_dim: 0x1C2E45 },
    AccentPreset { key: "green", accent: 0x4AC265, dim: 0x223322, on: 0x0C140C, success_dim: 0x1D3122 },
    AccentPreset { key: "amber", accent: 0xE8A348, dim: 0x33270F, on: 0x171006, success_dim: 0x382B12 },
    AccentPreset { key: "salmon", accent: 0xF47067, dim: 0x38201D, on: 0x1A0B09, success_dim: 0x3B2523 },
    AccentPreset { key: "gray", accent: 0xA8A8A8, dim: 0x262626, on: 0x101010, success_dim: 0x2F2F2F },
    AccentPreset { key: "silver", accent: 0xC9C9C9, dim: 0x303030, on: 0x141010, success_dim: 0x383838 },
    AccentPreset { key: "graphite", accent: 0x7A7A7A, dim: 0x1F1F1F, on: 0x0E0E0E, success_dim: 0x292929 },
    AccentPreset { key: "charcoal", accent: 0x505050, dim: 0x1A1A1A, on: 0x0C0C0C, success_dim: 0x232323 },
];

pub fn normalize_accent(accent: Option<&str>) -> &'static AccentPreset {
    ACCENT_PRESETS
        .iter()
        .find(|p| Some(p.key) == accent)
        .unwrap_or(&ACCENT_PRESETS[0])
}

/// rgb hex (0xRRGGBB) → fully opaque Rgba. gpui's rgba() takes 0xRRGGBBAA,
/// so the alpha goes in the LOW byte — OR-ing it high scrambles the channels.
pub fn rgba_u32(rgb: u32) -> Rgba {
    rgba((rgb << 8) | 0xFF)
}

/// The live accent family; swapped by the settings page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccentColors {
    pub accent: Rgba,
    pub dim: Rgba,
    pub on: Rgba,
    pub success_dim: Rgba,
}

impl AccentColors {
    pub fn for_preset(preset: &AccentPreset) -> Self {
        Self {
            accent: c((preset.accent << 8) | 0xFF),
            dim: c((preset.dim << 8) | 0xFF),
            on: c((preset.on << 8) | 0xFF),
            success_dim: c((preset.success_dim << 8) | 0xFF),
        }
    }
}

/// White — text on filled danger and on the default blue accent.
pub const ON_ACCENT: Rgba = c(0xFFFFFFFF);

// ---- font scale ----------------------------------------------------------

/// Text-size variant tokens (the DevTools ladder); everything multiplies the
/// user factor (settings) and the system factor (gsettings text-scaling).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontTokens {
    pub font_micro: f32,
    pub font_xsmall: f32,
    pub font_detail: f32,
    pub font_small: f32,
    pub font_body: f32,
    pub font_body_large: f32,
    pub font_h1: f32,
    pub font_h2: f32,
    pub font_h3: f32,
    pub font_title: f32,
    pub icon_nano: f32,
    pub icon_tiny: f32,
    pub icon_xsmall: f32,
    pub icon_small: f32,
    pub icon_body: f32,
    pub icon_medium: f32,
    pub icon_large: f32,
    pub icon_xl: f32,
}

// The DevTools ladder: 10 / 11 / 12 / 13 / 14 / 15 — small is the workhorse,
// title stays 15 semibold. Icons keep the +2-bumped ladder (user preference).
const BASE_TOKENS: FontTokens = FontTokens {
    font_micro: 10.0,
    font_xsmall: 11.0,
    font_detail: 11.0,
    font_small: 12.0,
    font_body: 13.0,
    font_body_large: 14.0,
    font_h1: 16.0,
    font_h2: 15.0,
    font_h3: 14.0,
    font_title: 15.0,
    icon_nano: 10.0,
    icon_tiny: 12.0,
    icon_xsmall: 13.0,
    icon_small: 14.0,
    icon_body: 15.0,
    icon_medium: 16.0,
    icon_large: 17.0,
    icon_xl: 20.0,
};

pub const FONT_COMPACT: &str = "compact";
pub const FONT_NORMAL: &str = "normal";
pub const FONT_LARGE: &str = "large";

pub fn normalize_font_scale(scale: Option<&str>) -> &str {
    match scale {
        Some(s) if s == FONT_COMPACT || s == FONT_LARGE => match s {
            FONT_COMPACT => FONT_COMPACT,
            _ => FONT_LARGE,
        },
        _ => FONT_NORMAL,
    }
}

pub fn font_tokens(font_scale: Option<&str>, system_factor: f32) -> FontTokens {
    let factor = match normalize_font_scale(font_scale) {
        FONT_COMPACT => 0.85,
        FONT_LARGE => 1.15,
        _ => 1.0,
    } * system_factor;
    let scaled = |v: f32| (v * factor * 100.0).round() / 100.0;
    FontTokens {
        font_micro: scaled(BASE_TOKENS.font_micro),
        font_xsmall: scaled(BASE_TOKENS.font_xsmall),
        font_detail: scaled(BASE_TOKENS.font_detail),
        font_small: scaled(BASE_TOKENS.font_small),
        font_body: scaled(BASE_TOKENS.font_body),
        font_body_large: scaled(BASE_TOKENS.font_body_large),
        font_h1: scaled(BASE_TOKENS.font_h1),
        font_h2: scaled(BASE_TOKENS.font_h2),
        font_h3: scaled(BASE_TOKENS.font_h3),
        font_title: scaled(BASE_TOKENS.font_title),
        icon_nano: scaled(BASE_TOKENS.icon_nano),
        icon_tiny: scaled(BASE_TOKENS.icon_tiny),
        icon_xsmall: scaled(BASE_TOKENS.icon_xsmall),
        icon_small: scaled(BASE_TOKENS.icon_small),
        icon_body: scaled(BASE_TOKENS.icon_body),
        icon_medium: scaled(BASE_TOKENS.icon_medium),
        icon_large: scaled(BASE_TOKENS.icon_large),
        icon_xl: scaled(BASE_TOKENS.icon_xl),
    }
}

// ---- metrics -------------------------------------------------------------

pub const SIDEBAR_MIN_WIDTH: f32 = 180.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 480.0;
pub const SIDEBAR_DEFAULT_WIDTH: f32 = 250.0;
pub const SPLITTER_WIDTH: f32 = 12.0;
pub const CODE_FONT: &str = "DejaVu Sans Mono";
pub const FONT_UI: &str = "Inter";
