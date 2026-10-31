//! Platform services: folder opening, the gsettings system text-scale
//! follower, and the panic net.

use crate::store::Store;
use async_channel::bounded;
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

static FACTOR_TX: std::sync::OnceLock<async_channel::Sender<f32>> = std::sync::OnceLock::new();

/// The spawned `gsettings monitor`: kept reachable so [`shutdown`] can kill
/// and reap it — a child owned only by the reader thread would outlive every
/// run as an orphan (the spawn itself fails on Windows, so nothing is stored
/// there).
#[cfg(unix)]
static GSETTINGS_CHILD: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);

/// Best-effort teardown of the platform services: kills the gsettings
/// monitor (an already-dead child kill+waits fine, a live one dies now).
pub fn shutdown() {
    #[cfg(unix)]
    if let Ok(mut slot) = GSETTINGS_CHILD.lock()
        && let Some(mut child) = slot.take()
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn apply_factor(store: &Entity<Store>, factor: f32, cx: &mut App) {
    store.update(cx, |s, cx| {
        if (s.system_font_factor - factor).abs() > 0.001 {
            s.system_font_factor = factor;
            s.fonts = crate::theme::font_tokens(s.settings.font_scale.as_deref(), factor);
            // the kit sizes Input/Textarea in rem off the theme's font_size,
            // so it has to re-map for kit components to rescale too
            crate::theme::apply_kit_theme(s.accent, &s.fonts, cx);
            cx.notify();
        }
    });
}

/// Follows the desktop text scaling GTK apps honor — gsettings
/// org.gnome.desktop.interface text-scaling-factor. The initial value is read
/// synchronously so the first frame is already sized right; a gsettings monitor
/// feeds live changes through a channel pumped on the main thread.
pub fn init_font_scale(store: &Entity<Store>, cx: &mut App) {
    if let Some(factor) = read_factor() {
        apply_factor(store, factor, cx);
    } else {
        tracing::debug!("system text scaling unreadable; keeping factor 1.0");
    }
    let (tx, rx) = bounded::<f32>(16);
    if FACTOR_TX.set(tx).is_err() {
        return; // already initialized
    }
    match std::process::Command::new("gsettings")
        .args(["monitor", "org.gnome.desktop.interface"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // stdout is all the reader needs; the handle itself stays in
            // GSETTINGS_CHILD for shutdown() to kill and reap
            let stdout = child.stdout.take();
            #[cfg(unix)]
            if let Ok(mut slot) = GSETTINGS_CHILD.lock() {
                *slot = Some(child);
            }
            #[cfg(not(unix))]
            drop(child);
            if let Some(stdout) = stdout {
                std::thread::Builder::new()
                    .name("gsettings".into())
                    .spawn(move || {
                        use std::io::BufRead;
                        let reader = std::io::BufReader::new(stdout);
                        for line in reader.lines().map_while(Result::ok) {
                            if let Some(factor) = parse_factor(&line)
                                && let Some(tx) = FACTOR_TX.get()
                            {
                                // sync thread: send_blocking; plain send() would
                                // just create and drop a future
                                let _ = tx.send_blocking(factor);
                            }
                        }
                        // a dead-but-unreaped monitor stays a zombie until
                        // shutdown()'s wait() — nothing to do here
                    })
                    .ok();
            }
        }
        Err(_) => {
            tracing::debug!(
                "gsettings monitor unavailable; system text scale frozen at startup value"
            );
        }
    }
    let weak = store.downgrade();
    cx.spawn(async move |cx| {
        while let Ok(factor) = rx.recv().await {
            if let Some(store) = weak.upgrade() {
                store.update(cx, |s, cx| {
                    if (s.system_font_factor - factor).abs() > 0.001 {
                        s.system_font_factor = factor;
                        s.fonts =
                            crate::theme::font_tokens(s.settings.font_scale.as_deref(), factor);
                        crate::theme::apply_kit_theme(s.accent, &s.fonts, cx);
                        cx.notify();
                    }
                });
            }
        }
    })
    .detach();
}

fn read_factor() -> Option<f32> {
    let output = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "text-scaling-factor"])
        .output()
        .ok()?;
    parse_factor(&String::from_utf8_lossy(&output.stdout))
}

/// Accepts bare "1.1667" (gsettings get) and "key: 1.1667" (monitor) lines.
fn parse_factor(text: &str) -> Option<f32> {
    let key = "text-scaling-factor";
    let text = match text.find(key) {
        Some(idx) => &text[idx + key.len()..],
        None => text,
    };
    let text = text.trim_start_matches([':', ' ', '\t']).trim_end();
    text.parse::<f32>().ok().filter(|v| *v > 0.0)
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
