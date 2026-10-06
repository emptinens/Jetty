use gpui::{
    App, AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window,
    WindowOptions, div, px, rgb, size,
};
use gpui_platform::application;

struct Root;

impl Render for Root {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(rgb(0x111111))
    }
}

fn main() {
    application().run(|cx: &mut App| {
        cx.open_window(
            WindowOptions {
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Jetty".into()),
                    ..Default::default()
                }),
                window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(700.)),
                    cx,
                ))),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| Root),
        )
        .expect("open window");
    });
}