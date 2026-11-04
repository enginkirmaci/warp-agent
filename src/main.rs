//! WarpAgent — a tile-grid chat client for opencode serve, on GPUI.
// no console window behind the packaged Windows exe; debug builds keep it for logs
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod assets;
mod dsh;
mod ipc;
mod logging;
mod oc;
mod paths;
mod services;
mod settings;
mod store;
mod theme;
mod ui;

use gpui::prelude::*;
use gpui::{App, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;
use store::Store;
use ui::window::WarpWindow;

/// Folder args from the launch command, opened once the store exists.
static LAUNCH_FOLDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

fn main() {
    let _log_guard = logging::initialize();
    services::install_panic_hook();

    // single instance: a live door owner takes the launch folders (a bare
    // liveness probe when there are none) and this launch exits
    let folders: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-') && std::path::Path::new(arg).is_dir())
        .collect();
    if ipc::try_forward(&folders) {
        return;
    }
    let _ = LAUNCH_FOLDERS.set(folders);

    application()
        .with_assets(assets::EmbedAssets::new())
        .run(|cx: &mut App| {
            let (_net_handle, net_sender, event_rx) = oc::spawn_net_thread();
            let store = Store::new(cx, net_sender);

            // the shared startup: kit init, the shared text size (seeded from
            // this app's old setting) and palette, follow hooks, the first
            // kit-theme pass and reduced motion. The retheme closure re-derives
            // the font tokens first — the text size adopted inside may have
            // rescaled the type.
            let (text_size, reduce_motion) = {
                let s = store.read(cx);
                (s.settings.font_scale.clone(), s.settings.reduce_motion)
            };
            warpui_kit_core::bootstrap::init(cx, text_size.as_deref(), reduce_motion, {
                let store = store.clone();
                move |cx: &mut gpui::App| {
                    store.update(cx, |s, _| s.fonts = theme::active_tokens());
                    let (accent, fonts) = {
                        let s = store.read(cx);
                        (s.accent, s.fonts)
                    };
                    theme::apply_kit_theme(accent, &fonts, cx);
                }
            });

            // composer key overrides: bound after gpui_kit::init (inside the
            // bootstrap) so our later bindings win the "Input" context ties
            // (up/down history, shift-tab)
            cx.bind_keys([
                gpui::KeyBinding::new("up", ui::tile::ComposerUp, Some("Input")),
                gpui::KeyBinding::new("down", ui::tile::ComposerDown, Some("Input")),
                gpui::KeyBinding::new("shift-tab", ui::tile::ComposerCycleAgent, Some("Input")),
                // slash-menu keys: the descendant predicate matches only the
                // composer frame's textarea (the frame sets "Composer", the
                // textarea is "Input" below it) — at the same depth as the
                // plain "Input" bindings above, winning as the later
                // registration, while every other input keeps the stock
                // editor up/down/enter/escape behavior
                gpui::KeyBinding::new("up", ui::tile::ComposerSlashUp, Some("Composer > Input")),
                gpui::KeyBinding::new(
                    "down",
                    ui::tile::ComposerSlashDown,
                    Some("Composer > Input"),
                ),
                gpui::KeyBinding::new(
                    "enter",
                    ui::tile::ComposerSlashEnter,
                    Some("Composer > Input"),
                ),
                gpui::KeyBinding::new(
                    "escape",
                    ui::tile::ComposerSlashEscape,
                    Some("Composer > Input"),
                ),
            ]);

            // workspaces named on the launch command open right away
            if let Some(folders) = LAUNCH_FOLDERS.get() {
                let folders = folders.clone();
                let launch_store = store.downgrade();
                cx.spawn(async move |cx| {
                    let Some(store) = launch_store.upgrade() else {
                        return;
                    };
                    store.update(cx, |store, cx| {
                        for folder in &folders {
                            let _ = store.open_workspace_at(folder, cx);
                        }
                    });
                })
                .detach();
            }

            // network events → store; the timer-fallback cadence, the burst
            // cap and the gpui-rev caveat live in the pump
            warpui_kit_core::pump::spawn(cx, event_rx, {
                let store = store.downgrade();
                move |cx, event| {
                    let Some(store) = store.upgrade() else {
                        return false;
                    };
                    store.update(cx, |s, cx| s.on_net_event(event, cx));
                    true
                }
            });

            // count down auto-allow timers once per second
            let countdown_store = store.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    if let Some(store) = countdown_store.upgrade() {
                        store.update(cx, |s, cx| s.tick_countdowns(cx));
                    } else {
                        break;
                    }
                }
            })
            .detach();

            // system text scale (gsettings) with live updates
            services::init_font_scale(&store, cx);

            let bounds = Bounds::centered(None, size(px(1500.), px(960.)), cx);
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // empty by default in GPUI — needed so Hyprland/launchers match
                    // StartupWMClass=warp-agent in the desktop entry
                    app_id: Some("warp-agent".into()),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Warp Agent".into()),
                        appears_transparent: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cx,
                |_, cx| {
                    let window_store = store.clone();
                    cx.new(|cx| WarpWindow::new(window_store, cx))
                },
            )
            .unwrap();

            // closing the window stops every spawned server, then quits
            let shutdown_store = store.downgrade();
            cx.on_window_closed(move |cx, _window_id| {
                if let Some(store) = shutdown_store.upgrade() {
                    store.update(cx, |s, _| s.shutdown());
                }
                cx.quit();
            })
            .detach();

            cx.activate(true);
        });
}
