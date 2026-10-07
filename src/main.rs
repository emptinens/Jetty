use std::collections::HashMap;

use gpui::{
    AnyElement, App, AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowOptions, div, px, rgb,
    rgba, size,
};
use gpui_platform::application;
use gpui_terminal::{TerminalConfig, TerminalView};

mod engine;
mod protocol;
mod session;
mod terminal;

use crate::protocol::{Event, Request};
use crate::terminal::{EngineWriter, EventReader, Feed};

struct Root {
    sessions: Vec<session::Session>,
    selected: Option<usize>,
    engine: engine::EngineHandle,
    quit_dialog: bool,
    terminals: HashMap<u64, Entity<TerminalView>>,
    feeds: HashMap<u64, Feed>,
}

impl Root {
    fn new(engine: engine::EngineHandle) -> Self {
        let this = Self {
            sessions: Vec::new(),
            selected: None,
            engine,
            quit_dialog: false,
            terminals: HashMap::new(),
            feeds: HashMap::new(),
        };
        this.engine.send(Request::Snapshot);
        this
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Sessions(list) => {
                self.sessions = list;
                if self.sessions.is_empty() {
                    self.selected = None;
                } else if self.selected.is_none() {
                    self.selected = Some(0);
                } else if self.selected.is_some_and(|s| s >= self.sessions.len()) {
                    self.selected = Some(self.sessions.len() - 1);
                }
                let live: Vec<u64> = self.sessions.iter().map(|s| s.id).collect();
                self.terminals.retain(|id, _| live.contains(id));
                self.feeds.retain(|id, _| live.contains(id));
                if let Some(session) = self.selected.and_then(|i| self.sessions.get(i)).cloned() {
                    self.ensure_terminal(&session, cx);
                }
                cx.notify();
            }
            Event::Output(id, bytes) => {
                if let Some(feed) = self.feeds.get(&id) {
                    let _ = feed.send(bytes);
                }
            }
            Event::Exited(id) => {
                // Dropping the feed ends the terminal reader (EOF).
                self.feeds.remove(&id);
                cx.notify();
            }
            Event::Error(msg) => {
                eprintln!("jetty: engine error: {msg}");
            }
        }
    }

    fn select(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(i);
        if let Some(session) = self.sessions.get(i).cloned() {
            self.ensure_terminal(&session, cx);
            let focus = self
                .terminals
                .get(&session.id)
                .map(|view| view.read(cx).focus_handle().clone());
            if let Some(focus) = focus {
                focus.focus(window, cx);
            }
        }
        cx.notify();
    }

    /// Creates the terminal view for a session (once) and attaches its pty.
    fn ensure_terminal(&mut self, session: &session::Session, cx: &mut Context<Self>) {
        let id = session.id;
        if self.terminals.contains_key(&id) {
            return;
        }
        let (feed, rx) = std::sync::mpsc::channel();
        let writer = EngineWriter::new(self.engine.clone(), id);
        let reader = EventReader::new(rx);
        let engine = self.engine.clone();
        let view = cx.new(|cx| {
            TerminalView::new(writer, reader, TerminalConfig::default(), cx).with_resize_callback(
                move |cols, rows| engine.send(Request::Resize(id, cols as u16, rows as u16)),
            )
        });
        self.feeds.insert(id, feed);
        self.terminals.insert(id, view);
        self.engine.send(Request::Attach(id));
    }

    fn add(&mut self, cx: &mut Context<Self>) {
        self.engine.send(Request::Add);
        self.selected = Some(self.sessions.len());
        cx.notify();
    }

    fn remove(&mut self, i: usize) {
        if let Some(session) = self.sessions.get(i) {
            self.engine.send(Request::Remove(session.id));
        }
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
                    .bg(if selected {
                        rgb(0x2b3a55)
                    } else {
                        rgb(0x1b1b1b)
                    })
                    .child(
                        div()
                            .id(("session-row", i))
                            .flex_1()
                            .overflow_hidden()
                            .cursor_pointer()
                            .child(self.sessions[i].name.clone())
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.select(i, window, cx)),
                            ),
                    )
                    .child(
                        div()
                            .id(("session-delete", i))
                            .px_1()
                            .text_color(rgb(0x777777))
                            .cursor_pointer()
                            .child("x")
                            .on_click(cx.listener(move |this, _, _, _cx| this.remove(i))),
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

        let main_pane: AnyElement = match self
            .selected
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.id)
            .and_then(|id| self.terminals.get(&id).cloned())
        {
            Some(view) => div().flex_1().h_full().child(view).into_any_element(),
            None => div()
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .gap_1()
                .p_4()
                .children(detail)
                .into_any_element(),
        };

        let dialog: Vec<AnyElement> = if self.quit_dialog {
            vec![
                div()
                    .absolute()
                    .size_full()
                    .bg(rgba(0x000000bb))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .bg(rgb(0x2a2a2a))
                            .child(div().text_lg().child("Quit Jetty?"))
                            .child(
                                div().text_sm().text_color(rgb(0x9a9a9a)).child(
                                    "Sessions will keep running if you choose 'Leave Running'.",
                                ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .id("quit-kill")
                                            .px_3()
                                            .py_1()
                                            .rounded_md()
                                            .bg(rgb(0x552222))
                                            .cursor_pointer()
                                            .child("Kill Sessions")
                                            .on_click(cx.listener(move |this, _e, window, _cx| {
                                                this.quit_dialog = false;
                                                window.remove_window();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id("quit-leave-running")
                                            .px_3()
                                            .py_1()
                                            .rounded_md()
                                            .bg(rgb(0x224422))
                                            .cursor_pointer()
                                            .child("Leave Running")
                                            .on_click(cx.listener(move |this, _e, window, _cx| {
                                                this.engine.send(Request::SetKillOnDrop(false));
                                                this.quit_dialog = false;
                                                window.remove_window();
                                            })),
                                    ),
                            ),
                    )
                    .into_any_element(),
            ]
        } else {
            vec![]
        };

        div()
            .flex()
            .relative()
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
            .child(main_pane)
            .children(dialog)
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("headless") {
        if let Err(e) = engine::run_headless() {
            eprintln!("jetty: engine error: {e}");
            std::process::exit(1);
        }
        return;
    }
    run_gui();
}

fn run_gui() {
    application().run(|cx: &mut App| {
        gpui_tokio::init(cx);
        let (engine, mut events) = engine::start_in_process(cx);
        let root = cx.new(|_| Root::new(engine));
        let root_for_close = root.clone();
        let weak = root.downgrade();
        let _window = cx
            .open_window(
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
                move |window, cx| {
                    window.on_window_should_close(cx, {
                        let root = root_for_close.clone();
                        move |_window, app| {
                            root.update(app, |root, cx| {
                                if root.quit_dialog {
                                    // dialog already resolved, allow close
                                    true
                                } else {
                                    // first close attempt, show dialog
                                    root.quit_dialog = true;
                                    cx.notify();
                                    false
                                }
                            })
                        }
                    });
                    root
                },
            )
            .expect("open window");
        cx.spawn(async move |cx| {
            while let Some(event) = events.recv().await {
                weak.update(cx, |root, cx| root.on_event(event, cx)).ok();
            }
        })
        .detach();
    });
}
