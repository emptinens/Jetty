use std::path::PathBuf;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::protocol::{Event, Request};
use crate::session::{self, Session};

pub struct EngineHandle(UnboundedSender<Request>);

impl EngineHandle {
    pub fn send(&self, request: Request) {
        let _ = self.0.send(request);
    }
}

pub struct State {
    path: PathBuf,
    pub sessions: Vec<Session>,
}

impl State {
    pub fn new(path: PathBuf) -> Self {
        let sessions = session::load(&path);
        Self { path, sessions }
    }

    pub fn load() -> Self {
        Self::new(session::config_path())
    }

    fn apply(&mut self, request: Request, events: &UnboundedSender<Event>) {
        match request {
            Request::Snapshot => {}
            Request::Add => {
                let mut s = session::default_session();
                s.name = format!("shell {}", self.sessions.len() + 1);
                self.sessions.push(s);
                if let Err(msg) = session::save(&self.path, &self.sessions) {
                    let _ = events.send(Event::Error(msg));
                }
            }
            Request::Remove(i) => {
                if i < self.sessions.len() {
                    self.sessions.remove(i);
                    if let Err(msg) = session::save(&self.path, &self.sessions) {
                        let _ = events.send(Event::Error(msg));
                    }
                }
            }
        }
    }
}

async fn run(
    mut requests: UnboundedReceiver<Request>,
    events: UnboundedSender<Event>,
    mut state: State,
) {
    let _ = events.send(Event::Sessions(state.sessions.clone()));
    while let Some(request) = requests.recv().await {
        state.apply(request, &events);
        let _ = events.send(Event::Sessions(state.sessions.clone()));
    }
}

pub fn start_in_process(cx: &gpui::App) -> (EngineHandle, UnboundedReceiver<Event>) {
    let (req_tx, req_rx) = unbounded_channel::<Request>();
    let (ev_tx, ev_rx) = unbounded_channel::<Event>();
    gpui_tokio::Tokio::spawn(cx, run(req_rx, ev_tx, State::load())).detach();
    (EngineHandle(req_tx), ev_rx)
}

pub fn run_headless() -> std::io::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let (_req_tx, req_rx) = unbounded_channel::<Request>();
        let (ev_tx, _ev_rx) = unbounded_channel::<Event>();
        let state = State::load();
        let n = state.sessions.len();
        println!("jetty headless: engine up, {n} session(s), no transport yet");
        run(req_rx, ev_tx, state).await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state(name: &str) -> (PathBuf, State) {
        let dir = std::env::temp_dir().join(format!("jetty-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        (path.clone(), State::new(path))
    }

    fn names(event: Event) -> String {
        match event {
            Event::Sessions(list) => list
                .into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
                .join(","),
            Event::Error(e) => panic!("unexpected error event: {e}"),
        }
    }

    #[test]
    fn add_remove_and_out_of_bounds_persist() {
        let (path, mut state) = temp_state("apply");
        assert_eq!(state.sessions.len(), 1);

        let (events, _) = unbounded_channel();
        state.apply(Request::Add, &events);
        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.sessions[1].name, "shell 2");

        state.apply(Request::Remove(999), &events); // out of bounds: no-op, no panic
        assert_eq!(state.sessions.len(), 2);

        state.apply(Request::Remove(0), &events);
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].name, "shell 2");
        assert_eq!(session::load(&path).len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn channel_round_trip_snapshot_add_remove() {
        let (path, state) = temp_state("round-trip");
        let (requests, request_rx) = unbounded_channel::<Request>();
        let (event_tx, mut events) = unbounded_channel::<Event>();
        let engine = tokio::spawn(run(request_rx, event_tx, state));

        assert_eq!(names(events.recv().await.unwrap()), "shell");
        requests.send(Request::Add).unwrap();
        assert_eq!(names(events.recv().await.unwrap()), "shell,shell 2");
        requests.send(Request::Snapshot).unwrap();
        assert_eq!(names(events.recv().await.unwrap()), "shell,shell 2");
        requests.send(Request::Remove(0)).unwrap();
        assert_eq!(names(events.recv().await.unwrap()), "shell 2");
        requests.send(Request::Remove(999)).unwrap();
        assert_eq!(names(events.recv().await.unwrap()), "shell 2");

        drop(requests);
        engine.await.unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn save_failure_surfaces_an_error_event() {
        // a regular file where the config dir should be makes create_dir_all fail
        let dir = std::env::temp_dir().join(format!("jetty-err-{}", std::process::id()));
        std::fs::write(&dir, "block").unwrap();
        let state = State {
            path: dir.join("sessions.json"),
            sessions: vec![session::default_session()],
        };
        let (requests, request_rx) = unbounded_channel::<Request>();
        let (event_tx, mut events) = unbounded_channel::<Event>();
        let engine = tokio::spawn(run(request_rx, event_tx, state));

        let _ = events.recv().await.unwrap();
        requests.send(Request::Add).unwrap();

        let (mut saw_error, mut saw_sessions) = (false, false);
        for _ in 0..2 {
            match events.recv().await.unwrap() {
                Event::Error(_) => saw_error = true,
                Event::Sessions(list) => {
                    saw_sessions = true;
                    assert_eq!(list.len(), 2, "in-memory mutation kept");
                }
            }
        }
        assert!(saw_error, "save failure must surface as an error event");
        assert!(saw_sessions, "registry still echoed");

        drop(requests);
        engine.await.unwrap();
        let _ = std::fs::remove_file(&dir);
    }
}