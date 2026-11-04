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
            // kit components (input decorations, popovers) + our palette mapped
            // onto their theme tokens — must run before any kit component renders
            gpui_kit::init(cx);

            // composer key overrides: bound after gpui_kit::init so our later
            // bindings win the "Input" context ties (up/down history, shift-tab)
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

            let (_net_handle, net_sender, event_rx) = oc::spawn_net_thread();
            let store = Store::new(cx, net_sender);

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

            // the shared text size, seeded once from this app's old setting
            warpui_kit_core::adopt_text_size(store.read(cx).settings.font_scale.as_deref());
            // the shared palette, before the first kit theme pass so the
            // first frame already draws in it
            warpui_kit_core::adopt_palette(None);
            // a palette switch re-maps the kit and repaints every window; the
            // store entity carries the app context, so the hook never has to
            // hold the `&mut App` from this closure
            warpui_kit_core::follow_palette({
                let store = store.clone();
                move |cx: &mut gpui::App| {
                    let (accent, fonts) = {
                        let s = store.read(cx);
                        (s.accent, s.fonts)
                    };
                    theme::apply_kit_theme(accent, &fonts, cx);
                    cx.refresh_windows();
                }
            });
            // the family motion setting, or the desktop's preference, applies
            // before the first frame
            warpui_kit_core::motion::set_reduced(
                store.read(cx).settings.reduce_motion
                    || warpui_kit_core::motion::system_prefers_reduced(),
            );
            store.update(cx, |s, _| s.fonts = theme::active_tokens());

            // kit theme after the store so the saved accent preset (not the
            // blue default) drives input selection/ring/primary from launch
            let (accent, fonts) = {
                let snap = store.read(cx);
                (snap.accent, snap.fonts)
            };
            theme::apply_kit_theme(accent, &fonts, cx);

            // network events → store. Polled with a timer fallback: this gpui rev
            // does not service cross-thread channel wakeups on foreground tasks,
            // so a plain recv().await would sleep forever (async_channel recv is
            // cancel-safe, re-creating the future each lap is fine). A wake drains
            // the whole pending burst in one pass — one timer and one frame per
            // burst instead of one per event.
            let event_store = store.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    let timeout = cx
                        .background_executor()
                        .timer(std::time::Duration::from_millis(200));
                    let mut recv = Box::pin(event_rx.recv());
                    match futures_util::future::select(recv.as_mut(), timeout).await {
                        futures_util::future::Either::Left((received, _)) => {
                            let Ok(first) = received else { break };
                            let Some(store) = event_store.upgrade() else {
                                break;
                            };
                            let mut batch = 1;
                            store.update(cx, |s, cx| s.on_net_event(first, cx));
                            while batch < 64 {
                                match event_rx.try_recv() {
                                    Ok(event) => {
                                        store.update(cx, |s, cx| s.on_net_event(event, cx));
                                        batch += 1;
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                        futures_util::future::Either::Right((_, _)) => continue,
                    }
                }
            })
            .detach();

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
