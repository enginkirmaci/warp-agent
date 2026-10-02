//! The palette port of App.xaml: neutral gray surfaces, green accent, salmon red,
//! amber, deliberately no blue. Font/icon size tokens ride one factor so the
//! settings text-size variant rescales the whole UI.

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
/// The accent family is a parameter so a settings-page preset swap can re-run
/// the mapping (`Theme::update` refreshes every window). Call after
/// [`gpui_kit::init`], before opening windows.
pub fn apply_kit_theme(accent: AccentColors, cx: &mut App) {
    use gpui_kit::component::{Theme, ThemeMode};

    resolve_fonts(cx);
    Theme::update(cx, |t| {
        t.mode = ThemeMode::Dark;
        t.background = WINDOW_CHROME.into();
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
        t.danger_foreground = TEXT.into();
        t.border = HAIRLINE.into();
        t.input = INPUT.into();
        t.ring = accent.accent.into();
        t.selection = selection(&accent).into();
        t.caret = TEXT.into();
        t.link = accent.accent.into();
        t.popover = SETTINGS_CARD.into();
        t.popover_foreground = TEXT.into();
        t.sidebar = WINDOW_CHROME.into();
        t.sidebar_foreground = TEXT.into();
        t.sidebar_border = HAIRLINE.into();

        // our tiles/cards sit on 8–12px corners
        t.radius = gpui::px(8.);
        t.radius_lg = gpui::px(12.);

        t.font_family = gpui::SharedString::from(ui_font());
        t.mono_font_family = gpui::SharedString::from(code_font());
    });
}

// ---- surfaces ------------------------------------------------------------

pub const SURFACE: Rgba = c(0x121212FF); // main-area card + tiles
pub const CARD: Rgba = c(0x121212FF); // tile surface, restore bar, error dialog
pub const WINDOW_CHROME: Rgba = c(0x1E1E1EFF); // titlebar, sidebar, ring around the main card
pub const SETTINGS_BG: Rgba = c(0x202020FF); // settings sheet
pub const SETTINGS_CARD: Rgba = c(0x1A1A1AFF);
pub const INPUT: Rgba = c(0x2A2A2AFF);
pub const HAIRLINE: Rgba = c(0x3E3E3EFF);
pub const TILE_HEADER_SEPARATOR: Rgba = c(0x333333FF);
pub const CHIP: Rgba = c(0x343434FF);
pub const DIFF_BG: Rgba = c(0x171717FF); // inset code/diff/fold body
pub const HOVER: Hsla = hsla(0.0, 0.0, 1.0, 0.08); // #14FFFFFF

pub const TEXT: Rgba = c(0xE4E4E4FF);
pub const TEXT_DIM: Rgba = c(0xB4B4B4FF); // secondary body text
pub const TEXT_MUTED: Rgba = c(0x9A9A9AFF); // labels, metadata
pub const TEXT_FAINT: Rgba = c(0x707070FF); // hints, quietest meta

/// Text selection in fields: the accent at 25% alpha.
pub fn selection(accent: &AccentColors) -> Rgba {
    let a = accent.accent;
    Rgba { r: a.r, g: a.g, b: a.b, a: 0.25 }
}

// ---- chat surfaces -------------------------------------------------------

pub const BUBBLE_USER: Rgba = c(0x343434FF);
pub const BUBBLE_ASSISTANT: Rgba = c(0x282828FF);
pub const Q_OPT_SELECTED: Rgba = c(0x333333FF);
pub const BUBBLE_QUEUED: Rgba = c(0x272727FF);
pub const QUEUED_BORDER: Rgba = c(0x4E4E4EFF);
pub const FLYOUT_SURFACE: Rgba = c(0x1C1C1CB3); // dark gray at 0.7 alpha over the chat

// ---- fixed semantic colors (never follow the accent preset) --------------

pub const DANGER: Rgba = c(0xF47067FF);
pub const DANGER_DIM: Rgba = c(0x3B2523FF);
pub const WARN: Rgba = c(0xE8A348FF);
pub const WARN_BG: Rgba = c(0x3A2D1AFF);
pub const ADD: Rgba = c(0x56C272FF);
pub const DEL: Rgba = c(0xF47067FF);
pub const WORKING: Rgba = c(0x4AC265FF); // pulse dot + sidebar spinner stay green

// ---- code ----------------------------------------------------------------

pub const CODE_BLOCK: Rgba = c(0x171717FF);
pub const CODE_FG: Rgba = c(0xE4E4E4FF);
pub const SYN_KEYWORD: Rgba = c(0xA9A9A9FF);
pub const SYN_STRING: Rgba = c(0x93B585FF);
pub const SYN_COMMENT: Rgba = c(0x6F6F6FFF);
pub const SYN_NUMBER: Rgba = c(0xC9986EFF);

// ---- diffs (the renderer in ui/diff.rs draws from here) -------------------

pub const DIFF_GUTTER: Rgba = c(0x9A9A9A55); // line-number gutter text
pub const DIFF_ADD_BG: Hsla = hsla(0.36, 0.44, 0.55, 0.15);
pub const DIFF_DEL_BG: Hsla = hsla(0.02, 0.83, 0.68, 0.15);
pub const DIFF_HUNK_BG: Hsla = hsla(0.0, 0.0, 0.6, 0.12);

// ---- overlays ------------------------------------------------------------

pub const OVERLAY_SCRIM: Hsla = hsla(0.0, 0.0, 0.0, 0.65); // #A6000000

// ---- accent presets ------------------------------------------------------

pub struct AccentPreset {
    pub key: &'static str,
    pub label: &'static str,
    /// rgb hex, one shared by accent + success
    pub accent: u32,
    pub dim: u32,
    pub on: u32,
    pub success_dim: u32,
}

pub const ACCENT_PRESETS: [AccentPreset; 8] = [
    AccentPreset { key: "green", label: "Green", accent: 0x4AC265, dim: 0x223322, on: 0x0C140C, success_dim: 0x1D3122 },
    AccentPreset { key: "blue", label: "Blue", accent: 0x4C8EFF, dim: 0x1E2C42, on: 0x0C1219, success_dim: 0x1C2E45 },
    AccentPreset { key: "amber", label: "Amber", accent: 0xE8A348, dim: 0x33270F, on: 0x171006, success_dim: 0x382B12 },
    AccentPreset { key: "salmon", label: "Salmon", accent: 0xF47067, dim: 0x38201D, on: 0x1A0B09, success_dim: 0x3B2523 },
    AccentPreset { key: "gray", label: "Gray", accent: 0xA8A8A8, dim: 0x262626, on: 0x101010, success_dim: 0x2F2F2F },
    AccentPreset { key: "silver", label: "Silver", accent: 0xC9C9C9, dim: 0x303030, on: 0x141010, success_dim: 0x383838 },
    AccentPreset { key: "graphite", label: "Graphite", accent: 0x7A7A7A, dim: 0x1F1F1F, on: 0x0E0E0E, success_dim: 0x292929 },
    AccentPreset { key: "charcoal", label: "Charcoal", accent: 0x505050, dim: 0x1A1A1A, on: 0x0C0C0C, success_dim: 0x232323 },
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

pub const GREEN_ACCENT: AccentColors = AccentColors {
    accent: c(0x4AC265FF),
    dim: c(0x223322FF),
    on: c(0x0C140CFF),
    success_dim: c(0x1D3122FF),
};

// ---- font scale ----------------------------------------------------------

/// Text-size variant tokens (exact App.xaml values); everything multiplies the
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

const BASE_TOKENS: FontTokens = FontTokens {
    font_micro: 10.5,
    font_xsmall: 11.0,
    font_detail: 11.5,
    font_small: 12.0,
    font_body: 12.5,
    font_body_large: 13.0,
    font_h1: 18.0,
    font_h2: 16.0,
    font_h3: 14.5,
    font_title: 19.0,
    // icon ladder runs ~+2 over the 2026-09 baseline (user found the old
    // sizes too quiet); nano only +1 so the check glyph fits its 13px box
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
pub const ON_ACCENT: Rgba = c(0x0C140CFF);
