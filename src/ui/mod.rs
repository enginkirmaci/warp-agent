//! ui module: shared helpers + the main window.

pub mod code_highlight;
pub mod controls;
pub mod diff;
pub mod markdown;
pub mod settings;
pub mod tile;
pub mod tooltip;
pub mod window;

use gpui::Styled;

/// A tinted svg icon.
pub fn icon(path: &'static str, size: gpui::Pixels, color: impl Into<gpui::Hsla>) -> gpui::Svg {
    gpui::svg().path(path).size(size).text_color(color.into())
}

/// Mime guess mirroring ChatTile's MimeFor.
pub fn mime_for_path(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "pdf" => "image/pdf",
        _ => "text/plain",
    }
}
