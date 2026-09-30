//! Embedded asset source: icons + spark branding ship inside the binary.

use gpui::AssetSource;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets"]
struct Assets;

pub struct EmbedAssets;

impl AssetSource for EmbedAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        Assets::get(path)
            .map(|file| Ok(Some(file.data)))
            .unwrap_or_else(|| Ok(None))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<gpui::SharedString>> {
        Ok(Assets::iter()
            .filter(|p| p.starts_with(path))
            .map(gpui::SharedString::from)
            .collect())
    }
}
