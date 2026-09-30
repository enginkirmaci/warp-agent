//! WarpAgent — a tile-grid chat client for opencode serve, on GPUI.
// no console window behind the packaged Windows exe; debug builds keep it for logs
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod assets;
mod logging;
mod oc;
mod paths;
mod services;
mod settings;
mod store;
mod theme;
mod ui;

use gpui::prelude::*;
use gpui::{px, size, App, Bounds, WindowBounds, WindowOptions};
use gpui_platform::application;
use store::Store;
use ui::window::WarpWindow;

fn main() {
    let _log_guard = logging::initialize();
    services::install_panic_hook();
    application()
        .with_assets(assets::EmbedAssets::new())
        .run(|cx: &mut App| {
            // kit components (input decorations, popovers) + our palette mapped
            // onto their theme tokens — must run before any kit component renders
            gpui_kit::init(cx);
            theme::apply_kit_theme(cx);

            // composer key overrides: bound after gpui_kit::init so our later
            // bindings win the "Input" context ties (up/down history, shift-tab)
            cx.bind_keys([
                gpui::KeyBinding::new("up", ui::tile::ComposerUp, Some("Input")),
                gpui::KeyBinding::new("down", ui::tile::ComposerDown, Some("Input")),
                gpui::KeyBinding::new("shift-tab", ui::tile::ComposerCycleAgent, Some("Input")),
            ]);

            let (_net_handle, net_sender, event_rx) = oc::spawn_net_thread();
            let store = Store::new(cx, net_sender);

            // network events → store. Polled with a timer fallback: this gpui rev
            // does not service cross-thread channel wakeups on foreground tasks,
            // so a plain recv().await would sleep forever (async_channel recv is
            // cancel-safe, re-creating the future each lap is fine)
            let event_store = store.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    let timeout = cx
                        .background_executor()
                        .timer(std::time::Duration::from_millis(200));
                    let mut recv = Box::pin(event_rx.recv());
                    match futures_util::future::select(recv.as_mut(), timeout).await {
                        futures_util::future::Either::Left((received, _)) => {
                            let Ok(event) = received else { break };
                            if let Some(store) = event_store.upgrade() {
                                store.update(cx, |s, cx| s.on_net_event(event, cx));
                            } else {
                                break;
                            }
                        }
                        futures_util::future::Either::Right((_, _)) => continue,
                    }
                }
            })
            .detach();

            // count down auto-allow timers once per second
            let countdown_store = store.downgrade();
            cx.spawn(async move |cx| loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if let Some(store) = countdown_store.upgrade() {
                    store.update(cx, |s, cx| s.tick_countdowns(cx));
                } else {
                    break;
                }
            })
            .detach();

            // system text scale (gsettings) with live updates
            services::init_font_scale(&store, cx);

            let bounds = Bounds::centered(None, size(px(1180.), px(800.)), cx);
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
