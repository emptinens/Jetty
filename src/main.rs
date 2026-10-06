use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext, Bounds, Context, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window, WindowOptions, div, px, rgb, size,
};
use gpui_platform::application;

mod session;

struct Root {
    sessions: Vec<session::Session>,
    selected: Option<usize>,
    path: PathBuf,
}

impl Root {
    fn select(&mut self, i: usize, cx: &mut Context<Self>) {
        self.selected = Some(i);
        cx.notify();
    }

    fn add(&mut self, cx: &mut Context<Self>) {
        let mut s = session::default_session();
        s.name = format!("shell {}", self.sessions.len() + 1);
        self.sessions.push(s);
        self.selected = Some(self.sessions.len() - 1);
        session::save(&self.path, &self.sessions);
        cx.notify();
    }

    fn remove(&mut self, i: usize, cx: &mut Context<Self>) {
        self.sessions.remove(i);
        if self.sessions.is_empty() {
            self.selected = None;
        } else if self.selected.is_some_and(|s| s >= self.sessions.len()) {
            self.selected = Some(self.sessions.len() - 1);
        }
        session::save(&self.path, &self.sessions);
        cx.notify();
    }
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows: Vec<AnyElement> = Vec::with_capacity(self.sessions.len());
        for i in 0..self.sessions.len() {
            let selected = self.selected == Some(i);
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(if selected { rgb(0x2b3a55) } else { rgb(0x1b1b1b) })
                    .child(
                        div()
                            .id(("session-row", i))
                            .flex_1()
                            .overflow_hidden()
                            .cursor_pointer()
                            .child(self.sessions[i].name.clone())
                            .on_click(cx.listener(move |this, _, _, cx| this.select(i, cx))),
                    )
                    .child(
                        div()
                            .id(("session-delete", i))
                            .px_1()
                            .text_color(rgb(0x777777))
                            .cursor_pointer()
                            .child("x")
                            .on_click(cx.listener(move |this, _, _, cx| this.remove(i, cx))),
                    )
                    .into_any_element(),
            );
        }

        let detail: Vec<AnyElement> = match self.selected.and_then(|i| self.sessions.get(i)) {
            Some(s) => vec![
                div().text_xl().child(s.name.clone()).into_any_element(),
                div()
                    .text_color(rgb(0x9a9a9a))
                    .child(s.directory.clone())
                    .into_any_element(),
                div()
                    .text_color(rgb(0x9a9a9a))
                    .child(s.command.clone())
                    .into_any_element(),
            ],
            None => vec![div().child("no session selected").into_any_element()],
        };

        div()
            .flex()
            .size_full()
            .bg(rgb(0x111111))
            .text_color(rgb(0xe6e6e6))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(220.))
                    .h_full()
                    .gap_1()
                    .p_2()
                    .border_r_1()
                    .border_color(rgb(0x2a2a2a))
                    .child(div().text_color(rgb(0x777777)).child("sessions"))
                    .children(rows)
                    .child(
                        div()
                            .id("add-session")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(0x243a2a))
                            .cursor_pointer()
                            .child("+ session")
                            .on_click(cx.listener(|this, _, _, cx| this.add(cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .h_full()
                    .gap_1()
                    .p_4()
                    .children(detail),
            )
    }
}

fn main() {
    application().run(|cx: &mut App| {
        let path = session::config_path();
        let sessions = session::load(&path);
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
            move |_window, cx| {
                let selected = (!sessions.is_empty()).then_some(0);
                cx.new(|_| Root {
                    sessions,
                    selected,
                    path,
                })
            },
        )
        .expect("open window");
    });
}