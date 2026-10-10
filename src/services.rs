//! Platform services: terminal launcher (Open in opencode) and the gsettings
//! system text-scale follower.

use crate::store::Store;
use gpui::{App, Entity};
use async_channel::bounded;

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

/// Spawns a terminal emulator running the opencode TUI in a folder.
#[cfg(windows)]
pub fn launch_in_terminal(folder: &str, executable: &str) -> Result<(), String> {
    // Windows Terminal first: proper tab rooted in the folder; the TUI's exit
    // closes it. The store-alias spawn can still fail (stub alias), so fall
    // through to a plain console on any error.
    if resolve_on_path("wt").is_some() {
        let mut command = std::process::Command::new("wt");
        command.args(["-d", folder, "cmd", "/C", executable]);
        match command.spawn() {
            Ok(_) => {
                tracing::info!("launched Windows Terminal running {executable} in {folder}");
                return Ok(());
            }
            Err(err) => tracing::warn!("Windows Terminal spawn failed ({err}); falling back to cmd"),
        }
    }
    // Plain console rooted in the folder; /K keeps it open if the exe exits
    // early so the error stays readable.
    let mut command = std::process::Command::new("cmd");
    command.args(["/K", executable]);
    if std::path::Path::new(folder).exists() {
        command.current_dir(folder);
    }
    command.spawn().map(|_| ()).map_err(|err| err.to_string())?;
    tracing::info!("launched cmd console running {executable} in {folder}");
    Ok(())
}

/// Unix terminal detection probes PATH for the usual suspects and picks the first hit.
#[cfg(not(windows))]
pub fn launch_in_terminal(folder: &str, executable: &str) -> Result<(), String> {
    type ArgsFn = fn(&str, &str) -> Vec<String>;
    let terminals: &[(&str, ArgsFn)] = &[
        ("alacritty", |dir, exe| vec!["--working-directory".into(), dir.into(), "-e".into(), exe.into()]),
        ("ghostty", |dir, exe| vec![format!("--working-directory={dir}"), "-e".into(), exe.into()]),
        ("foot", |dir, exe| vec![format!("--working-directory={dir}"), exe.into()]),
        ("kitty", |dir, exe| vec!["--directory".into(), dir.into(), exe.into()]),
        ("wezterm", |dir, exe| vec!["start".into(), "--cwd".into(), dir.into(), "--".into(), exe.into()]),
        ("gnome-terminal", |dir, exe| vec![format!("--working-directory={dir}"), "--".into(), exe.into()]),
        ("konsole", |dir, exe| vec!["--workdir".into(), dir.into(), "-e".into(), exe.into()]),
        ("xterm", |_dir, exe| vec!["-e".into(), exe.into()]),
    ];
    for (name, args) in terminals {
        if let Some(path) = resolve_on_path(name) {
            let mut command = std::process::Command::new(&path);
            command.args(args(folder, executable));
            if std::path::Path::new(folder).exists() {
                command.current_dir(folder);
            }
            match command.spawn() {
                Ok(_) => {
                    tracing::info!("launched {name} running {executable} in {folder}");
                    return Ok(());
                }
                Err(err) => return Err(err.to_string()),
            }
        }
    }
    Err("No terminal emulator found on PATH.".into())
}

#[cfg(unix)]
fn resolve_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    use std::os::unix::fs::PermissionsExt;
    for dir in std::env::split_paths(&path_env) {
        let full = dir.join(name);
        if let Ok(metadata) = std::fs::metadata(&full) {
            if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                return Some(full);
            }
        }
    }
    None
}

/// Windows has no exec bit; executables are found by their PATHEXT suffix.
#[cfg(windows)]
fn resolve_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    for dir in std::env::split_paths(&path_env) {
        let mut candidates = vec![dir.join(name)];
        candidates.extend(exts.split(';').map(|ext| dir.join(format!("{name}{ext}"))));
        for candidate in candidates {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

static FACTOR_TX: std::sync::OnceLock<async_channel::Sender<f32>> = std::sync::OnceLock::new();

fn apply_factor(store: &Entity<Store>, factor: f32, cx: &mut App) {
    store.update(cx, |s, cx| {
        if (s.system_font_factor - factor).abs() > 0.001 {
            s.system_font_factor = factor;
            s.fonts = crate::theme::font_tokens(s.settings.font_scale.as_deref(), factor);
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
            std::thread::Builder::new()
                .name("gsettings".into())
                .spawn(move || {
                    use std::io::BufRead;
                    let Some(stdout) = child.stdout.take() else { return };
                    let reader = std::io::BufReader::new(stdout);
                    for line in reader.lines().map_while(Result::ok) {
                        if let Some(factor) = parse_factor(&line) {
                            if let Some(tx) = FACTOR_TX.get() {
                                // sync thread: send_blocking; plain send() would
                                // just create and drop a future
                                let _ = tx.send_blocking(factor);
                            }
                        }
                    }
                    let _ = child.wait();
                })
                .ok();
        }
        Err(_) => {
            tracing::debug!("gsettings monitor unavailable; system text scale frozen at startup value");
        }
    }
    let weak = store.downgrade();
    cx.spawn(async move |cx| {
        while let Ok(factor) = rx.recv().await {
            if let Some(store) = weak.upgrade() {
                store.update(cx, |s, cx| {
                    if (s.system_font_factor - factor).abs() > 0.001 {
                        s.system_font_factor = factor;
                        s.fonts = crate::theme::font_tokens(s.settings.font_scale.as_deref(), factor);
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
