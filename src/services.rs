//! Platform services: folder opening, the desktop text-scale follower, and
//! the panic net.

use crate::store::Store;
use gpui::{App, Entity};

/// Rust's fs::canonicalize hands back verbatim `\\?\`-prefixed paths on
/// Windows; GUI consumers (explorer, `wt -d`, cmd's working directory) reject
/// or ignore that form, so unwrap it back to the plain spelling — drive paths
/// and UNC shares alike. Identity off-Windows.
pub(crate) fn plain_path(path: &std::path::Path) -> std::path::PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return std::path::PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// Opens a folder in the platform file manager: explorer on Windows, xdg-open elsewhere.
pub fn open_folder(path: &str) -> Result<(), String> {
    #[cfg(windows)]
    let mut command = std::process::Command::new("explorer");
    #[cfg(not(windows))]
    let mut command = std::process::Command::new("xdg-open");
    command
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Launches a sibling Warp app by the binary name the suite installs. The
/// window is the other app's; this process only hands it over.
pub fn launch_sibling(binary: &str) -> Result<(), String> {
    std::process::Command::new(binary)
        .spawn()
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Hooks the store up to scale changes; each one re-reads the ladder and
/// re-maps the kit.
pub fn init_font_scale(store: &Entity<Store>, cx: &mut App) {
    let store = store.downgrade();
    warpui_kit_core::follow_ui_scale(cx, move |cx| {
        if let Some(store) = store.upgrade() {
            store.update(cx, |s, cx| s.rescale_fonts(cx));
        }
    });
}

/// The crash net, terminal-side: panics are logged with their payload so the
/// rolling log always carries the reason. gpui unwinds handler panics per
/// frame; the app keeps running and the rest of the sessions stay alive.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(move |info| {
        let message = info.to_string();
        tracing::error!("panic: {message}");
        eprintln!("[warp-agent] panic: {message}");
    }));
}

#[cfg(test)]
mod plain_path_tests {
    use super::plain_path;

    #[test]
    fn unwraps_verbatim_drive_paths() {
        assert_eq!(
            plain_path(std::path::Path::new(r"\\?\C:\Users\dev\proj")).to_string_lossy(),
            r"C:\Users\dev\proj"
        );
    }

    #[test]
    fn unwraps_verbatim_unc_paths() {
        assert_eq!(
            plain_path(std::path::Path::new(r"\\?\UNC\box\share\dir")).to_string_lossy(),
            r"\\box\share\dir"
        );
    }

    #[test]
    fn leaves_plain_paths_alone() {
        assert_eq!(
            plain_path(std::path::Path::new("/home/dev/proj")).to_string_lossy(),
            "/home/dev/proj"
        );
    }
}
