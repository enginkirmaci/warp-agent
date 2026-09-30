//! WarpAgent — a tile-grid chat client for opencode serve, on GPUI.

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
use gpui::{div, px, size, App, Bounds, WindowBounds, WindowOptions};
use gpui_platform::application;
use store::Store;
use ui::window::WarpWindow;

fn main() {
    let _log_guard = logging::initialize();
    services::install_panic_hook();
    application()
        .with_assets(assets::EmbedAssets)
        .run(|cx: &mut App| {
            let (_net_handle, net_sender, event_rx) = oc::spawn_net_thread();
            let store = Store::new(cx, net_sender);

            // network events → store
            let event_store = store.downgrade();
            cx.spawn(async move |cx| {
                while let Ok(event) = event_rx.recv().await {
                    if let Some(store) = event_store.upgrade() {
                        store.update(cx, |s, cx| s.on_net_event(event, cx));
                    } else {
                        break;
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
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Warp Agent".into()),
                        appears_transparent: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
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
