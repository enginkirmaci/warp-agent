use gpui::prelude::*;
use gpui::{div, px, size, App, Bounds, Context, Window, WindowBounds, WindowOptions};
struct H;
impl Render for H {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(gpui::rgb(0x121212)).flex().items_center().justify_center().text_color(gpui::rgb(0xffffff)).child("hello gpui")
    }
}
fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(400.), px(300.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Hello".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| H),
        ).unwrap();
        cx.activate(true);
    });
}
