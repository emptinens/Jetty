use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::protocol::{Event, Request};
use crate::session::{self, Session};

/// Commands sent to the dedicated write task so PTY writes never block the engine loop.
enum WriteCommand {
    Write(u64, Vec<u8>),
    Register(u64, Box<dyn Write + Send>),
    Unregister(u64),
}

/// Holds the master pty for resize, and the child for kill.
/// The writer is owned by the write-handling task (see WriteCommand::Register).
struct SessionRuntime {
    master: Box<dyn portable_pty::MasterPty>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

/// Ensures a spawned child process is killed on drop unless the guard is disarmed.
/// Prevents orphan processes when an error occurs after spawn but before the child
/// is stored in a long-lived owner.
struct ChildGuard {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
}

impl ChildGuard {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self { child: Some(child) }
    }

    /// Disarm the guard and return the child process, transferring ownership.
    fn disarm(mut self) -> Box<dyn portable_pty::Child + Send + Sync> {
        self.child.take().expect("ChildGuard already disarmed")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Clone)]
pub struct EngineHandle(UnboundedSender<Request>);

impl EngineHandle {
    pub fn send(&self, request: Request) {
        let _ = self.0.send(request);
    }

    #[cfg(test)]
    pub(crate) fn from_sender(sender: UnboundedSender<Request>) -> Self {
        Self(sender)
    }
}

pub struct State {
    path: PathBuf,
    pub sessions: Vec<Session>,
    runtimes: HashMap<u64, SessionRuntime>,
    kill_on_drop: bool,
    write_tx: Option<UnboundedSender<WriteCommand>>,
    next_id: u64,
}

impl Drop for State {
    fn drop(&mut self) {
        if !self.kill_on_drop {
            return;
        }
        for (_, mut rt) in self.runtimes.drain() {
            let _ = rt.child.kill();
        }
    }
}

impl State {
    /// Loads session state from the given JSON path.
    ///
    /// `session::save` writes atomically (tmp + rename) but does not serialize two
    /// engine instances sharing one file: concurrent jetty processes can lose each
    /// other's edits. Accepted for now; the rusqlite/Loro storage layer replaces it.
    pub fn new(path: PathBuf) -> Self {
        let sessions = session::load(&path);
        Self::from_sessions(path, sessions)
    }

    fn from_sessions(path: PathBuf, sessions: Vec<Session>) -> Self {
        Self {
            next_id: sessions.iter().map(|s| s.id).max().unwrap_or(0) + 1,
            path,
            sessions,
            runtimes: HashMap::default(),
            kill_on_drop: true,
            write_tx: None,
        }
    }

    /// Prevents the Drop impl from killing child processes.
    /// Call this before dropping State when the user chooses "leave running"
    /// in the quit dialog.
    pub fn set_kill_on_drop(&mut self, kill: bool) {
        self.kill_on_drop = kill;
    }

    pub fn load() -> Self {
        Self::new(session::config_path())
    }

    fn is_announced(&self, id: u64) -> bool {
        self.sessions.iter().any(|s| s.id == id)
    }

    fn apply(
        &mut self,
        request: Request,
        client: &UnboundedSender<Event>,
        pty_events: &UnboundedSender<Event>,
    ) {
        let mut changed = false;
        match request {
            Request::Snapshot => {}
            Request::Add(new) => {
                let id = self.next_id;
                self.next_id += 1;
                let defaults = session::default_session();
                self.sessions.push(Session {
                    id,
                    name: if new.name.trim().is_empty() {
                        format!("shell {id}")
                    } else {
                        new.name.trim().to_string()
                    },
                    directory: if new.directory.trim().is_empty() {
                        defaults.directory
                    } else {
                        new.directory.trim().to_string()
                    },
                    command: if new.command.trim().is_empty() {
                        defaults.command
                    } else {
                        new.command.trim().to_string()
                    },
                });
                changed = true;
            }
            Request::Remove(id) => {
                if let Some(pos) = self.sessions.iter().position(|s| s.id == id) {
                    self.sessions.remove(pos);
                    changed = true;
                }
                // also kill any running pty for this session
                if let Some(mut rt) = self.runtimes.remove(&id) {
                    let _ = rt.child.kill();
                }
                if let Some(tx) = &self.write_tx {
                    let _ = tx.send(WriteCommand::Unregister(id));
                }
            }
            Request::Attach(id) => {
                let session = match self.sessions.iter().find(|s| s.id == id) {
                    Some(s) => s.clone(),
                    None => return,
                };
                if session.command.is_empty() || self.runtimes.contains_key(&id) {
                    return;
                }
                let write_tx = match &self.write_tx {
                    Some(tx) => tx.clone(),
                    None => return,
                };
                match attach_pty(id, session, pty_events, &write_tx) {
                    Err(err) => {
                        let _ = client.send(Event::Error(err));
                    }
                    Ok(runtime) => {
                        self.runtimes.insert(id, runtime);
                    }
                }
            }
            Request::Input(id, bytes) => {
                if let Some(tx) = &self.write_tx {
                    let _ = tx.send(WriteCommand::Write(id, bytes));
                }
            }
            Request::Resize(id, cols, rows) => {
                if let Some(rt) = self.runtimes.get(&id) {
                    let _ = rt.master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            Request::Kill(id) => {
                if let Some(mut rt) = self.runtimes.remove(&id) {
                    let _ = rt.child.kill();
                    // dropping rt closes master; reader thread sees EOF, emits Exited
                }
                if let Some(tx) = &self.write_tx {
                    let _ = tx.send(WriteCommand::Unregister(id));
                }
            }
            Request::SetKillOnDrop(val) => {
                self.set_kill_on_drop(val);
            }
        }
        if changed && let Err(msg) = session::save(&self.path, &self.sessions) {
            let _ = client.send(Event::Error(msg));
        }
    }
}

/// Split a command line into program plus arguments, honoring single and double
/// quotes so `sh -c 'echo hi'` works. An unclosed quote swallows the rest.
fn split_command(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for ch in command.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    started = true;
                }
                c if c.is_whitespace() => {
                    if started || !current.is_empty() {
                        parts.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                c => {
                    current.push(c);
                    started = true;
                }
            },
        }
    }
    if started || !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Open a pty, spawn the session command on the slave, launch a reader thread,
/// and register the writer with the write-handling task so writes never block
/// the engine loop.
fn attach_pty(
    id: u64,
    session: Session,
    pty_events: &UnboundedSender<Event>,
    write_tx: &UnboundedSender<WriteCommand>,
) -> Result<SessionRuntime, String> {
    let parts = split_command(&session.command);
    let program = parts
        .first()
        .ok_or_else(|| format!("attach {id}: empty command"))?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("attach {id}: openpty: {e}"))?;

    let mut cmd = CommandBuilder::new(program);
    for arg in &parts[1..] {
        cmd.arg(arg);
    }
    cmd.cwd(std::path::PathBuf::from(&session.directory));
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("attach {id}: spawn: {e}"))?;
    drop(pair.slave);

    // Guard: if any step after spawn fails, the child is killed on drop
    // instead of leaking as an orphan process.
    let child_guard = ChildGuard::new(child);

    let writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => return Err(format!("attach {id}: take_writer: {e}")),
    };
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => return Err(format!("attach {id}: clone_reader: {e}")),
    };

    // dedicated reader thread so the engine loop never blocks on pty reads
    let events_tx = pty_events.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF: slave closed or process exited
                Ok(n) => {
                    let _ = events_tx.send(Event::Output(id, buf[..n].to_vec()));
                }
                Err(_) => break,
            }
        }
        let _ = events_tx.send(Event::Exited(id));
    });

    // Hand the writer to the dedicated write task so writes never block the
    // engine loop (they go through spawn_blocking).
    let _ = write_tx.send(WriteCommand::Register(id, writer));

    // All steps succeeded: disarm the guard and take ownership of the child.
    let child = child_guard.disarm();

    Ok(SessionRuntime {
        master: pair.master,
        child,
    })
}

async fn run(
    mut requests: UnboundedReceiver<Request>,
    internal_tx: UnboundedSender<Event>,
    mut internal_rx: UnboundedReceiver<Event>,
    client: UnboundedSender<Event>,
    mut state: State,
) {
    // Dedicated write task: receives WriteCommands (Write, Register, Unregister),
    // owns all PTY writers, and performs each write via spawn_blocking so the
    // engine loop never blocks on synchronous PTY I/O — even for large pastes.
    let (write_tx, mut write_rx) = unbounded_channel::<WriteCommand>();
    tokio::spawn(async move {
        use std::collections::HashMap;
        let mut writers: HashMap<u64, Box<dyn Write + Send>> = HashMap::new();
        while let Some(cmd) = write_rx.recv().await {
            match cmd {
                WriteCommand::Register(id, w) => {
                    writers.insert(id, w);
                }
                WriteCommand::Write(id, bytes) => {
                    if let Some(mut w) = writers.remove(&id) {
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = w.write_all(&bytes);
                            w
                        })
                        .await
                        .map(|w| writers.insert(id, w));
                    }
                }
                WriteCommand::Unregister(id) => {
                    writers.remove(&id);
                }
            }
        }
    });
    state.write_tx = Some(write_tx);

    // Initial announcement: send Sessions and mark all sessions as announced.
    // No buffered events can exist yet because no PTYs are attached.
    let _ = client.send(Event::Sessions(state.sessions.clone()));

    loop {
        tokio::select! {
            request = requests.recv() => {
                match request {
                    Some(req) => {
                        state.apply(req, &client, &internal_tx);

                        // Drain any pty events that arrived during apply().
                        // Buffer Output/Exited until after Sessions is sent, so the
                        // client always learns about sessions before their output.
                        let mut buffered = Vec::new();
                        while let Ok(ev) = internal_rx.try_recv() {
                            match &ev {
                                Event::Output(id, _) if state.is_announced(*id) => {
                                    buffered.push(ev);
                                }
                                Event::Exited(id) if state.is_announced(*id) => {
                                    buffered.push(ev);
                                }
                                // session has been removed — drop the orphaned event
                                Event::Output(..) | Event::Exited(..) => {}
                                _ => {}
                            }
                        }

                        // Announce the current session list.
                        let _ = client.send(Event::Sessions(state.sessions.clone()));

                        // Flush buffered pty events after Sessions.
                        for ev in buffered {
                            let _ = client.send(ev);
                        }
                    }
                    None => break,
                }
            }
            internal_event = internal_rx.recv() => {
                // PTY event arrived between requests: forward if announced, else buffer.
                match internal_event {
                    Some(ev @ Event::Output(id, _)) if state.is_announced(id) => {
                        let _ = client.send(ev);
                    }
                    Some(ev @ Event::Exited(id)) if state.is_announced(id) => {
                        let _ = client.send(ev);
                    }
                    // session has been removed — drop the orphaned event
                    _ => {}
                }
            }
        }
    }
}

pub fn start_in_process(cx: &gpui::App) -> (EngineHandle, UnboundedReceiver<Event>) {
    let (req_tx, req_rx) = unbounded_channel::<Request>();
    let (internal_tx, internal_rx) = unbounded_channel::<Event>();
    let (client_tx, client_rx) = unbounded_channel::<Event>();
    gpui_tokio::Tokio::spawn(
        cx,
        run(req_rx, internal_tx, internal_rx, client_tx, State::load()),
    )
    .detach();
    (EngineHandle(req_tx), client_rx)
}

pub fn run_headless() -> std::io::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let (_req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, _client_rx) = unbounded_channel::<Event>();
        let state = State::load();
        let n = state.sessions.len();
        println!("jetty headless: engine up, {n} session(s), no transport yet");
        run(req_rx, internal_tx, internal_rx, client_tx, state).await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::NewSession;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn temp_state(name: &str) -> (PathBuf, State) {
        let dir = std::env::temp_dir().join(format!("jetty-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        (path.clone(), State::new(path))
    }

    /// Create dummy channels for tests that call apply() directly.
    fn dummy_channels() -> (UnboundedSender<Event>, UnboundedSender<Event>) {
        let (client, _) = unbounded_channel::<Event>();
        let (pty, _) = unbounded_channel::<Event>();
        (client, pty)
    }

    fn names(event: Event) -> String {
        match event {
            Event::Sessions(list) => list
                .into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
                .join(","),
            Event::Error(e) => panic!("unexpected error event: {e}"),
            _ => panic!("unexpected event: {event:?}"),
        }
    }

    #[test]
    fn add_remove_and_missing_id_noop() {
        let (path, mut state) = temp_state("apply");
        assert_eq!(state.sessions.len(), 1);
        let sid = state.sessions[0].id;
        assert_ne!(sid, 0, "seeded session must have a non-zero id");

        let (client, pty) = dummy_channels();
        state.apply(Request::Add(NewSession::default()), &client, &pty);
        assert_eq!(state.sessions.len(), 2);
        assert_eq!(
            state.sessions[1].name,
            format!("shell {}", state.sessions[1].id)
        );

        // Remove by non-existent id: no-op
        state.apply(Request::Remove(0), &client, &pty);
        assert_eq!(state.sessions.len(), 2);

        // Remove by a missing id that was never assigned
        state.apply(Request::Remove(999), &client, &pty);
        assert_eq!(state.sessions.len(), 2);

        // Remove the first session by id
        state.apply(Request::Remove(sid), &client, &pty);
        assert_eq!(state.sessions.len(), 1);
        assert!(state.sessions.iter().all(|s| s.id != sid));
        assert_eq!(session::load(&path).len(), 1);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ids_unique_after_add_remove() {
        let (path, mut state) = temp_state("ids");
        let (client, pty) = dummy_channels();
        let sid1 = state.sessions[0].id;
        assert_ne!(sid1, 0);

        state.apply(Request::Add(NewSession::default()), &client, &pty);
        let sid2 = state.sessions[1].id;
        assert_ne!(sid2, sid1);

        state.apply(Request::Add(NewSession::default()), &client, &pty);
        assert_eq!(state.sessions.len(), 3);

        state.apply(Request::Remove(sid2), &client, &pty);
        assert_eq!(state.sessions.len(), 2);
        // Add again; new id must be unique and not reuse the removed id
        state.apply(Request::Add(NewSession::default()), &client, &pty);
        assert_eq!(state.sessions.len(), 3);
        let ids: Vec<u64> = state.sessions.iter().map(|s| s.id).collect();
        let mut uniq = ids.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(ids.len(), uniq.len(), "ids must be unique");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn stub_requests_accepted_without_error() {
        let (path, mut state) = temp_state("stubs");
        let (client, pty) = dummy_channels();
        let sid = state.sessions[0].id;

        // These must not panic and not mutate sessions
        state.apply(Request::Attach(sid), &client, &pty);
        state.apply(Request::Input(sid, vec![b'a']), &client, &pty);
        state.apply(Request::Resize(sid, 80, 24), &client, &pty);
        state.apply(Request::Kill(sid), &client, &pty);
        assert_eq!(state.sessions.len(), 1);

        // Stub requests for non-existent ids: fine, no-op
        state.apply(Request::Attach(999), &client, &pty);
        state.apply(Request::Input(999, vec![b'b']), &client, &pty);
        state.apply(Request::Resize(999, 120, 40), &client, &pty);
        state.apply(Request::Kill(999), &client, &pty);
        assert_eq!(state.sessions.len(), 1);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn channel_round_trip_snapshot_add_remove() {
        let (path, state) = temp_state("round-trip");
        let (requests, request_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut events) = unbounded_channel::<Event>();
        let engine = tokio::spawn(run(request_rx, internal_tx, internal_rx, client_tx, state));

        assert_eq!(names(events.recv().await.unwrap()), "shell");
        requests.send(Request::Add(NewSession::default())).unwrap();
        // second session gets id-2 name "shell 2" (seeded session has id 1)
        let ev = events.recv().await.unwrap();
        let list = match &ev {
            Event::Sessions(l) => l.clone(),
            _ => panic!("expected Sessions"),
        };
        assert_eq!(list.len(), 2);
        let sid1 = list[0].id;
        let sid2 = list[1].id;
        assert_eq!(names(ev), format!("shell,shell {sid2}"));

        requests.send(Request::Snapshot).unwrap();
        assert_eq!(
            names(events.recv().await.unwrap()),
            format!("shell,shell {sid2}")
        );

        requests.send(Request::Remove(sid1)).unwrap();
        assert_eq!(names(events.recv().await.unwrap()), format!("shell {sid2}"));

        requests.send(Request::Remove(0)).unwrap();
        assert_eq!(names(events.recv().await.unwrap()), format!("shell {sid2}"));

        drop(requests);
        engine.await.unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn channel_snapshot_empty_list() {
        let (path, state) = temp_state("snap-empty");
        let (requests, request_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut events) = unbounded_channel::<Event>();
        let engine = tokio::spawn(run(request_rx, internal_tx, internal_rx, client_tx, state));

        // Initial announcement: one seeded session.
        assert_eq!(names(events.recv().await.unwrap()), "shell");

        // Remove the only session to drain the list to empty.
        requests.send(Request::Remove(1)).unwrap();
        assert_eq!(names(events.recv().await.unwrap()), "");

        // Snapshot on an empty list must produce Sessions([]), not panic.
        requests.send(Request::Snapshot).unwrap();
        assert_eq!(names(events.recv().await.unwrap()), "");

        drop(requests);
        engine.await.unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn save_failure_surfaces_an_error_event() {
        // a regular file where the config dir should be makes create_dir_all fail
        let dir = std::env::temp_dir().join(format!("jetty-err-{}", std::process::id()));
        std::fs::write(&dir, "block").unwrap();
        let state = State::from_sessions(dir.join("sessions.json"), vec![session::default_session()]);
        let (requests, request_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut events) = unbounded_channel::<Event>();
        let engine = tokio::spawn(run(request_rx, internal_tx, internal_rx, client_tx, state));

        let _ = events.recv().await.unwrap();
        requests.send(Request::Add(NewSession::default())).unwrap();

        // Add triggers a save; the save fails, so we get an Error + Sessions pair
        let (mut saw_error, mut saw_sessions) = (false, false);
        for _ in 0..2 {
            match events.recv().await.unwrap() {
                Event::Error(_) => saw_error = true,
                Event::Sessions(list) => {
                    saw_sessions = true;
                    assert_eq!(list.len(), 2, "in-memory mutation kept");
                }
                _ => {}
            }
        }
        assert!(saw_error, "save failure must surface as an error event");
        assert!(saw_sessions, "registry still echoed");

        drop(requests);
        engine.await.unwrap();
        let _ = std::fs::remove_file(&dir);
    }

    #[tokio::test]
    async fn save_failure_state_not_corrupted() {
        // Set up a valid sessions.json to hold the "original" pre-failure state.
        let valid_dir = std::env::temp_dir().join(format!("jetty-sc-valid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&valid_dir);
        std::fs::create_dir_all(&valid_dir).unwrap();
        let valid_path = valid_dir.join("sessions.json");
        let mut initial_session = session::default_session();
        initial_session.id = 1;
        let initial = vec![initial_session.clone()];
        session::save(&valid_path, &initial).unwrap();

        // A regular file where the config dir should be makes create_dir_all fail.
        let block_dir = std::env::temp_dir().join(format!("jetty-sc-block-{}", std::process::id()));
        std::fs::write(&block_dir, "block").unwrap();

        let state = State::from_sessions(block_dir.join("sessions.json"), initial.clone());
        let (requests, request_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut events) = unbounded_channel::<Event>();
        let engine = tokio::spawn(run(request_rx, internal_tx, internal_rx, client_tx, state));

        // Drain initial Sessions event.
        let _ = events.recv().await.unwrap();

        // (a) Add triggers save; save fails -> Error + Sessions. In-memory state grew.
        requests.send(Request::Add(NewSession::default())).unwrap();
        let (mut saw_error, mut saw_sessions) = (false, false);
        for _ in 0..2 {
            match events.recv().await.unwrap() {
                Event::Error(_) => saw_error = true,
                Event::Sessions(list) => {
                    saw_sessions = true;
                    assert_eq!(
                        list.len(),
                        2,
                        "in-memory mutation kept: sessions grew from 1 to 2"
                    );
                }
                _ => {}
            }
        }
        assert!(saw_error, "save failure must surface as an error event");
        assert!(saw_sessions, "registry still echoed after add");

        // (b) Remove still works on in-memory state even though save keeps failing.
        requests.send(Request::Remove(2)).unwrap();
        let mut saw_remove = false;
        for _ in 0..2 {
            match events.recv().await.unwrap() {
                Event::Error(_) => {}
                Event::Sessions(list) => {
                    saw_remove = true;
                    assert_eq!(list.len(), 1, "in-memory remove works despite save failure");
                }
                _ => {}
            }
        }
        assert!(saw_remove, "remove should produce a Sessions event");

        // (c) load() from the valid path still returns the pre-failure state
        //     since every save was rejected.
        let reloaded = session::load(&valid_path);
        assert_eq!(
            reloaded, initial,
            "original path unchanged: save was rejected"
        );

        drop(requests);
        engine.await.unwrap();
        let _ = std::fs::remove_file(&block_dir);
        let _ = std::fs::remove_dir_all(&valid_dir);
    }

    #[tokio::test]
    async fn attach_streams_output_and_exit() {
        let dir = std::env::temp_dir().join(format!("jpt1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = dir.join("s");
        std::fs::write(&s, "#!/bin/sh\nprintf hello").unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
        let session = Session {
            id: 1,
            name: "t1".into(),
            directory: dir.to_string_lossy().into(),
            command: s.to_string_lossy().into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let _engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));
        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = ev_rx.recv().await.unwrap();
            req_tx.send(Request::Attach(1)).unwrap();
            assert!(matches!(ev_rx.recv().await.unwrap(), Event::Sessions(_)));
            let mut out = Vec::new();
            loop {
                match ev_rx.recv().await.unwrap() {
                    Event::Output(1, bytes) => out.extend(bytes),
                    Event::Exited(1) => break,
                    Event::Error(e) => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
            assert!(
                String::from_utf8_lossy(&out).contains("hello"),
                "missing hello"
            );
        })
        .await
        .unwrap();
        drop(req_tx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn input_reaches_the_process() {
        let dir = std::env::temp_dir().join(format!("jpt2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = dir.join("s");
        std::fs::write(&s, "#!/bin/sh\nread line\nprintf got:%s \"$line\"").unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
        let session = Session {
            id: 1,
            name: "t2".into(),
            directory: dir.to_string_lossy().into(),
            command: s.to_string_lossy().into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let _engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));
        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = ev_rx.recv().await.unwrap();
            req_tx.send(Request::Attach(1)).unwrap();
            assert!(matches!(ev_rx.recv().await.unwrap(), Event::Sessions(_)));
            req_tx.send(Request::Input(1, b"abc\n".to_vec())).unwrap();
            let _ = ev_rx.recv().await.unwrap();
            let mut out = Vec::new();
            loop {
                match ev_rx.recv().await.unwrap() {
                    Event::Output(1, bytes) => out.extend(bytes),
                    Event::Exited(1) => break,
                    Event::Error(e) => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
            assert!(
                String::from_utf8_lossy(&out).contains("got:abc"),
                "missing got:abc"
            );
        })
        .await
        .unwrap();
        drop(req_tx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resize_and_kill() {
        let dir = std::env::temp_dir().join(format!("jpt3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = dir.join("s");
        std::fs::write(&s, "#!/bin/sh\nsleep 30").unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
        let session = Session {
            id: 1,
            name: "t3".into(),
            directory: dir.to_string_lossy().into(),
            command: s.to_string_lossy().into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let _engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));
        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = ev_rx.recv().await.unwrap();
            req_tx.send(Request::Attach(1)).unwrap();
            assert!(matches!(ev_rx.recv().await.unwrap(), Event::Sessions(_)));
            req_tx.send(Request::Resize(1, 80, 24)).unwrap();
            let _ = ev_rx.recv().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            while let Ok(ev) = ev_rx.try_recv() {
                assert!(!matches!(ev, Event::Error(_)), "unexpected error");
            }
            req_tx.send(Request::Kill(1)).unwrap();
            loop {
                match ev_rx.recv().await.unwrap() {
                    Event::Exited(1) => break,
                    Event::Error(e) => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        drop(req_tx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Verify that Resize (TIOCSWINSZ) propagates through the PTY to the child
    /// process. Spawns a script that waits for input then prints `stty size`;
    /// after Resize the output must contain the new dimensions.
    #[tokio::test]
    async fn resize_propagation_reaches_child() {
        let dir = std::env::temp_dir().join(format!("jpt-rp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = dir.join("s");
        // Script: wait for any input line, then print terminal size (rows cols).
        std::fs::write(&s, "#!/bin/sh\nread _\nstty size\n").unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
        let session = Session {
            id: 1,
            name: "rp".into(),
            directory: dir.to_string_lossy().into(),
            command: s.to_string_lossy().into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let _engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));
        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = ev_rx.recv().await.unwrap(); // initial Sessions
            req_tx.send(Request::Attach(1)).unwrap();
            assert!(matches!(ev_rx.recv().await.unwrap(), Event::Sessions(_)));

            // Resize to non-default dimensions so the child's output is unambiguous.
            req_tx.send(Request::Resize(1, 45, 15)).unwrap();

            // Small settle window: kernel may buffer TIOCSWINSZ delivery.
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Send a newline to satisfy `read _`, triggering `stty size`.
            req_tx.send(Request::Input(1, b"\n".to_vec())).unwrap();

            // Collect all output until the child exits.
            let mut out = Vec::new();
            loop {
                match ev_rx.recv().await.unwrap() {
                    Event::Output(1, bytes) => out.extend(bytes),
                    Event::Exited(1) => break,
                    Event::Error(e) => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
            let s = String::from_utf8_lossy(&out);
            assert!(
                s.contains("15 45"),
                "expected child to see 15 rows x 45 cols after Resize, got output: {:?}",
                s,
            );
        })
        .await
        .unwrap();
        drop(req_tx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Verify that session.directory is propagated as the child process's
    /// working directory. Creates a temp dir with a unique marker file, sets
    /// session.directory to that dir, attaches a PTY with a script that runs
    /// `ls` (no args), and asserts the marker file name appears in the output.
    #[tokio::test]
    async fn session_directory_is_child_cwd() {
        let dir = std::env::temp_dir().join(format!("jpt-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A uniquely-named marker file that only exists in this temp dir.
        let marker = dir.join("__JETTY_CWD_MARKER__");
        std::fs::write(&marker, "").unwrap();
        let marker_name = marker.file_name().unwrap().to_string_lossy();

        // Script: ls (no args) lists the current directory. The marker must
        // appear only if the child's cwd is session.directory.
        let s = dir.join("s");
        std::fs::write(&s, "#!/bin/sh\nls\n").unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();

        let session = Session {
            id: 1,
            name: "cwd-test".into(),
            directory: dir.to_string_lossy().into(),
            command: s.to_string_lossy().into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let _engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));
        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = ev_rx.recv().await.unwrap(); // initial Sessions
            req_tx.send(Request::Attach(1)).unwrap();
            assert!(matches!(ev_rx.recv().await.unwrap(), Event::Sessions(_)));
            let mut out = Vec::new();
            loop {
                match ev_rx.recv().await.unwrap() {
                    Event::Output(1, bytes) => out.extend(bytes),
                    Event::Exited(1) => break,
                    Event::Error(e) => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
            let text = String::from_utf8_lossy(&out);
            assert!(
                text.contains(marker_name.as_ref()),
                "child cwd is not session.directory; expected {:?} in output: {:?}",
                marker_name,
                text,
            );
        })
        .await
        .unwrap();
        drop(req_tx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Returns the /proc state char for a pid, or None if the process is gone.
    /// The comm field may contain spaces or parens, so parse after the last ')'.
    fn proc_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_paren = stat.rsplit(')').next()?;
        after_paren.trim().chars().next()
    }

    /// True while the process is runnable, sleeping, or in uninterruptible sleep.
    /// Zombies and reaped processes count as dead.
    fn process_alive(pid: u32) -> bool {
        matches!(proc_state(pid), Some('R') | Some('S') | Some('D'))
    }

    /// Attaches session 1 and waits for its child to publish its pid to
    /// `dir/pid` (script: `echo $$ > pid; sleep 30`).
    async fn attach_and_read_child_pid(
        req_tx: &tokio::sync::mpsc::UnboundedSender<Request>,
        pid_path: &std::path::Path,
    ) -> u32 {
        req_tx.send(Request::Attach(1)).unwrap();
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(pid_path)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("child never published its pid to {:?}", pid_path);
    }

    /// Default behavior: when the engine's run loop ends and State drops,
    /// the attached child is killed (SIGKILL via the Drop impl).
    #[tokio::test]
    async fn engine_drop_kills_child_by_default() {
        let dir = std::env::temp_dir().join(format!("jpt-kod-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = dir.join("s");
        std::fs::write(&s, "#!/bin/sh\necho $$ > pid\nsleep 30").unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
        let session = Session {
            id: 1,
            name: "kod".into(),
            directory: dir.to_string_lossy().into(),
            command: s.to_string_lossy().into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, _ev_rx) = unbounded_channel::<Event>();
        let engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));

        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            attach_and_read_child_pid(&req_tx, &dir.join("pid")).await
        })
        .await
        .unwrap();
        assert!(process_alive(pid), "child must be alive before engine drop");

        drop(req_tx);
        engine.await.unwrap();

        let mut dead = false;
        for _ in 0..100 {
            if !process_alive(pid) {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            dead,
            "child must die when State drops with kill_on_drop=true"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The quit dialog's "Leave Running" path: Request::SetKillOnDrop(false)
    /// must reach State before the run loop ends, so the attached child is
    /// spared when State drops. The reader thread keeps a dup of the pty
    /// master, so no SIGHUP is delivered to the child.
    #[tokio::test]
    async fn set_kill_on_drop_false_spares_child_on_engine_drop() {
        let dir = std::env::temp_dir().join(format!("jpt-lr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = dir.join("s");
        std::fs::write(&s, "#!/bin/sh\necho $$ > pid\nsleep 30").unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
        let session = Session {
            id: 1,
            name: "lr".into(),
            directory: dir.to_string_lossy().into(),
            command: s.to_string_lossy().into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, _ev_rx) = unbounded_channel::<Event>();
        let engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));

        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            attach_and_read_child_pid(&req_tx, &dir.join("pid")).await
        })
        .await
        .unwrap();
        assert!(process_alive(pid), "child must be alive before engine drop");

        // Same ordering as the dialog's Leave Running click: send the request,
        // then close the window (which ends the engine loop and drops State).
        req_tx.send(Request::SetKillOnDrop(false)).unwrap();
        drop(req_tx);
        engine.await.unwrap();

        assert!(
            process_alive(pid),
            "child must survive State drop after SetKillOnDrop(false)"
        );
        // Re-check after a settle window: nothing may kill it asynchronously.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            process_alive(pid),
            "child must still be alive 200ms after engine drop"
        );

        // Cleanup: kill the spared child so it does not linger for 30s.
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn remove_session_drops_orphaned_pty_events() {
        // Integration test: attach a PTY, remove the session, verify no
        // orphaned Output/Exited events leak through the client channel.
        let dir = std::env::temp_dir().join(format!("jpt4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = dir.join("s");
        std::fs::write(&s, "#!/bin/sh\nsleep 10").unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755)).unwrap();
        let session = Session {
            id: 1,
            name: "t4".into(),
            directory: dir.to_string_lossy().into(),
            command: s.to_string_lossy().into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));

        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = ev_rx.recv().await.unwrap(); // initial Sessions
            req_tx.send(Request::Attach(1)).unwrap();
            assert!(matches!(ev_rx.recv().await.unwrap(), Event::Sessions(_)));

            // Remove the session while the PTY is still alive.
            req_tx.send(Request::Remove(1)).unwrap();
            // Drain all subsequent events. With the fix, no Output/Exited for
            // session 1 should appear after the Remove-triggered Sessions.
            let mut saw_sessions = false;
            let mut leaked_count = 0u32;
            loop {
                match tokio::time::timeout(Duration::from_secs(2), ev_rx.recv()).await {
                    Ok(Some(ev)) => match ev {
                        Event::Sessions(list) => {
                            saw_sessions = true;
                            if list.is_empty() {
                                break; // Remove was applied, list is empty
                            }
                        }
                        Event::Output(id, _) | Event::Exited(id) => {
                            if id == 1 {
                                leaked_count += 1;
                            }
                        }
                        Event::Error(e) => panic!("unexpected error: {e}"),
                    },
                    Ok(None) => break,
                    Err(_) => break, // timeout — no more events
                }
            }
            assert!(saw_sessions, "should see Sessions after Remove");
            assert_eq!(leaked_count, 0, "no Output/Exited for removed session");
        })
        .await
        .unwrap();

        drop(req_tx);
        engine.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn child_guard_kills_and_reaps_on_drop() {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("100");
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);

        // Wrap in guard: drop must kill and reap the child without panicking.
        let guard = ChildGuard::new(child);
        drop(guard);

        // After drop, kill() + wait() completed. The slave side is closed,
        // so the master reads should drain residual data and eventually see
        // EOF. Verify the master is still usable (not panicked) and that
        // reads complete without hanging.
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut buf = [0u8; 256];
        // Read until EOF or timeout.
        let start = std::time::Instant::now();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF: slave closed
                Ok(_) => {}     // drain residual kernel-buffered data
                Err(e) => panic!("unexpected read error: {e}"),
            }
            if start.elapsed() > Duration::from_secs(2) {
                break; // no more data, master is clean
            }
        }
        // If we get here, the guard's drop completed, and the master is healthy.
    }

    #[test]
    fn disarm_prevents_kill() {
        // Verify that disarming the guard returns the child and prevents kill.
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("100");
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);

        let guard = ChildGuard::new(child);
        let mut child = guard.disarm();

        // The child should still be alive: disarm prevented kill.
        // Kill it manually to clean up.
        child.kill().unwrap();
        child.wait().unwrap();

        // read from master to drain any post-kill output
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut buf = [0u8; 256];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    #[tokio::test]
    async fn add_attach_remove_no_pending_events_leak() {
        // Integration test for Add -> Attach -> Remove flow.
        // Verify the run loop (a) does not panic, (b) does not leak Output/Exited events for the
        // removed session, (c) continue serving requests afterward, and (d) not emit
        // any unexpected Error events.
        let dir =
            std::env::temp_dir().join(format!("jpt-add-attach-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        // Seed with a shell so the engine starts with one known session.
        let seed = Session {
            id: 1,
            name: "shell".into(),
            directory: "/".into(),
            command: "/bin/sh".into(),
        };
        let state = State::from_sessions(path.clone(), vec![seed.clone()]);

        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));

        tokio::time::timeout(Duration::from_secs(30), async {
            // --- Initial announcement ---
            let ev = ev_rx.recv().await.unwrap();
            let initial = match &ev {
                Event::Sessions(l) => l.clone(),
                other => panic!("expected initial Sessions, got {other:?}"),
            };
            assert_eq!(initial.len(), 1, "one seeded session");
            assert_eq!(initial[0].id, 1);

            // --- Step 1: Add (creates a default shell session) ---
            req_tx.send(Request::Add(NewSession::default())).unwrap();
            let ev = ev_rx.recv().await.unwrap();
            let after_add = match &ev {
                Event::Sessions(l) => l.clone(),
                other => panic!("expected Sessions after Add, got {other:?}"),
            };
            assert_eq!(after_add.len(), 2, "seeded + added session");
            let new_id = after_add[1].id;
            assert!(new_id > 1, "new session gets a fresh id");
            assert_eq!(after_add[1].name, format!("shell {new_id}"));

            // --- Step 2: Attach (starts PTY for the added session) ---
            req_tx.send(Request::Attach(new_id)).unwrap();
            // Sessions arrives after Attach.
            let ev = ev_rx.recv().await.unwrap();
            match &ev {
                Event::Sessions(l) => assert_eq!(l.len(), 2),
                Event::Error(e) => panic!("unexpected error after Attach: {e}"),
                other => panic!("expected Sessions after Attach, got {other:?}"),
            }

            // Drain any shell banner/prompt output that arrived with Attach.
            while let Ok(ev) = ev_rx.try_recv() {
                match ev {
                    Event::Error(e) => panic!("unexpected error after Attach: {e}"),
                    Event::Sessions(_) | Event::Output(..) | Event::Exited(..) => {}
                }
            }

            // --- Step 3: send Input and verify Output arrives after Sessions ---
            let marker = format!("JETTY_TEST_MARKER_{new_id}");
            let input_line = format!("echo {marker}\n");
            req_tx
                .send(Request::Input(new_id, input_line.as_bytes().to_vec()))
                .unwrap();

            // Wait for Output containing the marker (requirement 3: Output after Sessions).
            let mut got_output = false;
            loop {
                match tokio::time::timeout(Duration::from_secs(3), ev_rx.recv()).await {
                    Ok(Some(Event::Output(id, bytes))) if id == new_id => {
                        let text = String::from_utf8_lossy(&bytes);
                        if text.contains(&marker) {
                            got_output = true;
                            break;
                        }
                        // Not our marker yet: keep waiting (shell may echo input first).
                    }
                    Ok(Some(Event::Error(e))) => panic!("unexpected error event: {e}"),
                    Ok(Some(Event::Sessions(_))) => {
                        // Duplicate Sessions is harmless; skip and keep waiting.
                    }
                    Ok(Some(Event::Exited(id))) if id == new_id => {
                        panic!("process exited before we could Remove");
                    }
                    Ok(Some(Event::Output(..))) | Ok(Some(Event::Exited(..))) => {
                        // Output/Exited for other sessions; ignore.
                    }
                    Ok(None) => break,
                    Err(_) => break, // timeout
                }
            }
            assert!(
                got_output,
                "Output event containing marker must arrive for attached session"
            );

            // Drain any further Output/Exited events that arrived before Remove.
            while let Ok(ev) = ev_rx.try_recv() {
                match ev {
                    Event::Error(e) => panic!("unexpected error before Remove: {e}"),
                    Event::Sessions(_) | Event::Output(..) | Event::Exited(..) => {}
                }
            }

            // --- Step 4: Remove the session while its PTY is still running ---
            req_tx.send(Request::Remove(new_id)).unwrap();

            // Drain all events after Remove. No Output/Exited for new_id may
            // leak through. Look for the Sessions event confirming removal.
            let mut saw_removal = false;
            let mut leaked = 0u32;
            let mut error_count = 0u32;
            loop {
                match tokio::time::timeout(Duration::from_secs(3), ev_rx.recv()).await {
                    Ok(Some(Event::Sessions(list))) => {
                        // The seed session (id=1) should be the only one left.
                        if list.len() == 1 && list[0].id == 1 {
                            saw_removal = true;
                            break;
                        }
                        // Otherwise the list may still contain the session
                        // (Sessions event before Remove was processed).
                    }
                    Ok(Some(Event::Output(id, _))) | Ok(Some(Event::Exited(id))) => {
                        if id == new_id {
                            leaked += 1;
                        }
                    }
                    Ok(Some(Event::Error(_))) => {
                        error_count += 1;
                    }
                    Ok(None) => break,
                    Err(_) => break, // timeout
                }
            }
            assert!(saw_removal, "should see Sessions reflecting removal");
            assert_eq!(
                leaked, 0,
                "no Output/Exited may leak for removed session {new_id}"
            );
            assert_eq!(error_count, 0, "no Error events emitted");

            // Discard any straggler events from the killed PTY.
            while let Ok(ev) = ev_rx.try_recv() {
                match ev {
                    Event::Output(id, _) | Event::Exited(id) => {
                        assert_ne!(id, new_id, "no straggler events for removed session");
                    }
                    Event::Error(e) => panic!("unexpected error after Remove: {e}"),
                    Event::Sessions(_) => {}
                }
            }

            // --- Step 5: engine is still alive after Remove (requirement 5) ---
            req_tx.send(Request::Add(NewSession::default())).unwrap();
            let ev = tokio::time::timeout(Duration::from_secs(3), ev_rx.recv())
                .await
                .unwrap()
                .unwrap();
            match ev {
                Event::Sessions(list) => {
                    assert_eq!(
                        list.len(),
                        2,
                        "engine still works: new session added after Remove"
                    );
                    let newest = &list[1];
                    // ids are monotonic: the next Add must not reuse the removed id.
                    assert!(
                        newest.id > new_id,
                        "ids must never be reused: got {} after removing {new_id}",
                        newest.id
                    );
                }
                Event::Error(e) => panic!("unexpected error on post-remove Add: {e}"),
                other => panic!("expected Sessions after post-remove Add, got {other:?}"),
            }

            // --- Step 6: final drain, verify no Error events (requirement 6) ---
            while let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_millis(500), ev_rx.recv()).await
            {
                assert!(
                    !matches!(ev, Event::Error(_)),
                    "no Error events during final drain, got {ev:?}"
                );
            }
        })
        .await
        .unwrap();

        drop(req_tx);
        engine.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn command_arguments_split_into_argv() {
        let dir = std::env::temp_dir().join(format!("jetty-args-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = Session {
            id: 1,
            name: "args".into(),
            directory: dir.to_string_lossy().into(),
            command: "/bin/echo hello world".into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let _engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));
        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = ev_rx.recv().await.unwrap(); // initial Sessions
            req_tx.send(Request::Attach(1)).unwrap();
            let mut out = Vec::new();
            loop {
                match ev_rx.recv().await.unwrap() {
                    Event::Output(1, bytes) => out.extend(bytes),
                    Event::Exited(1) => break,
                    Event::Error(e) => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
            let text = String::from_utf8_lossy(&out);
            assert!(
                text.contains("hello world"),
                "command args must reach the child, got {text:?}"
            );
        })
        .await
        .unwrap();
        drop(req_tx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_splitting_honors_quotes() {
        let split = |s: &str| split_command(s).join("|");
        assert_eq!(split("/bin/echo hello world"), "/bin/echo|hello|world");
        assert_eq!(split("/bin/sh -c 'echo hi there'"), "/bin/sh|-c|echo hi there");
        assert_eq!(split("/bin/sh -c \"echo hi\""), "/bin/sh|-c|echo hi");
        assert_eq!(split("  a   b  "), "a|b");
        assert_eq!(split("/bin/echo ''"), "/bin/echo|");
        assert_eq!(split("/bin/sh -c 'unclosed"), "/bin/sh|-c|unclosed");
        assert!(split_command("   ").is_empty());
    }

    #[tokio::test]
    async fn quoted_command_arguments_reach_the_child() {
        let dir = std::env::temp_dir().join(format!("jetty-quoted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = Session {
            id: 1,
            name: "quoted".into(),
            directory: dir.to_string_lossy().into(),
            command: "/bin/sh -c 'printf quoted-ok'".into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let _engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));
        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = ev_rx.recv().await.unwrap(); // initial Sessions
            req_tx.send(Request::Attach(1)).unwrap();
            let mut out = Vec::new();
            loop {
                match ev_rx.recv().await.unwrap() {
                    Event::Output(id, bytes) if id == 1 => out.extend(bytes),
                    Event::Exited(id) if id == 1 => break,
                    Event::Error(e) => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
            let text = String::from_utf8_lossy(&out);
            assert!(text.contains("quoted-ok"), "got {text:?}");
        })
        .await
        .unwrap();
        drop(req_tx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Engine-level integration test that verifies PATH resolution: the session
    /// command is a bare name ("sh", not "/bin/sh") and the PTY spawns, produces
    /// output via stdin commands, and exits cleanly.  No existing PTY test
    /// exercises bare-name resolution — they all use absolute paths to temp
    /// scripts.
    #[tokio::test]
    async fn bare_command_name_resolved_via_path() {
        let dir = std::env::temp_dir().join(format!("jpt-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = Session {
            id: 1,
            name: "path-resolve".into(),
            directory: dir.to_string_lossy().into(),
            command: "sh".into(), // bare name, resolved via PATH by CommandBuilder
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let _engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));
        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = ev_rx.recv().await.unwrap(); // initial Sessions
            req_tx.send(Request::Attach(1)).unwrap();
            assert!(matches!(ev_rx.recv().await.unwrap(), Event::Sessions(_)));
            // Send input that produces distinctive output, then exits cleanly.
            req_tx
                .send(Request::Input(1, b"printf pathok\nexit\n".to_vec()))
                .unwrap();
            let mut out = Vec::new();
            loop {
                match ev_rx.recv().await.unwrap() {
                    Event::Output(1, bytes) => out.extend(bytes),
                    Event::Exited(1) => break,
                    Event::Error(e) => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
            let text = String::from_utf8_lossy(&out);
            assert!(
                text.contains("pathok"),
                "expected 'pathok' in output, got: {:?}",
                text
            );
        })
        .await
        .unwrap();
        drop(req_tx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Multi-session PTY concurrency test.
    ///
    /// Spawns two sessions with PTYs attached concurrently, sends different
    /// input to each, and verifies:
    ///
    /// 1. Output events are correctly routed to the right session IDs
    ///    (no cross-wiring of output between sessions).
    /// 2. Resize to one session does not affect the other.
    /// 3. Kill of one session produces Exited only for that session.
    ///
    /// This is the first concurrency test for the PTY layer; all existing
    /// PTY tests use a single session (id=1).
    #[tokio::test]
    async fn multi_session_pty_concurrency_no_cross_wiring() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = std::env::temp_dir().join(format!("jetty-ms-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();

            let session1 = Session {
                id: 1,
                name: "one".into(),
                directory: dir.to_string_lossy().into(),
                command: "/bin/sh".into(),
            };
            let session2 = Session {
                id: 2,
                name: "two".into(),
                directory: dir.to_string_lossy().into(),
                command: "/bin/sh".into(),
            };

            let state = State::from_sessions(dir.join("sessions.json"), vec![session1, session2]);

            let (req_tx, req_rx) = unbounded_channel::<Request>();
            let (internal_tx, internal_rx) = unbounded_channel::<Event>();
            let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
            let _engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));

            // Drain initial Sessions event.
            let ev = ev_rx.recv().await.unwrap();
            assert!(
                matches!(&ev, Event::Sessions(list) if list.len() == 2),
                "initial Sessions should list both sessions, got {ev:?}"
            );

            // -- Attach both sessions --
            req_tx.send(Request::Attach(1)).unwrap();
            assert!(
                matches!(ev_rx.recv().await.unwrap(), Event::Sessions(_)),
                "Sessions after attach 1"
            );
            req_tx.send(Request::Attach(2)).unwrap();
            assert!(
                matches!(ev_rx.recv().await.unwrap(), Event::Sessions(_)),
                "Sessions after attach 2"
            );

            // Drain shell banners / prompts.
            while let Ok(ev) = ev_rx.try_recv() {
                match ev {
                    Event::Sessions(_) | Event::Output(..) => {}
                    Event::Error(e) => panic!("unexpected error during startup: {e}"),
                    Event::Exited(_) => panic!("unexpected exit during startup"),
                }
            }

            // -- Send different input to each session concurrently --
            let marker1 = "JMS1_INPUT_OK";
            let marker2 = "JMS2_INPUT_OK";
            req_tx
                .send(Request::Input(1, format!("echo {marker1}\n").into_bytes()))
                .unwrap();
            req_tx
                .send(Request::Input(2, format!("echo {marker2}\n").into_bytes()))
                .unwrap();

            // Collect output until both sessions have responded.
            let mut out1 = Vec::new();
            let mut out2 = Vec::new();
            let mut got_m1 = false;
            let mut got_m2 = false;
            loop {
                match tokio::time::timeout(Duration::from_secs(5), ev_rx.recv()).await {
                    Ok(Some(Event::Output(id, bytes))) => {
                        match id {
                            1 => out1.extend(&bytes),
                            2 => out2.extend(&bytes),
                            other => panic!("output for unknown session {other}"),
                        }
                        let text1 = String::from_utf8_lossy(&out1);
                        let text2 = String::from_utf8_lossy(&out2);
                        if text1.contains(marker1) {
                            got_m1 = true;
                        }
                        if text2.contains(marker2) {
                            got_m2 = true;
                        }
                        if got_m1 && got_m2 {
                            break;
                        }
                    }
                    Ok(Some(Event::Sessions(_))) => {} // skip
                    Ok(Some(Event::Error(e))) => panic!("unexpected error: {e}"),
                    Ok(Some(Event::Exited(id))) => {
                        panic!("unexpected Exited({id}) before kill");
                    }
                    Ok(None) => break,
                    Err(_) => break, // timeout
                }
            }

            assert!(
                got_m1,
                "session 1 should have received its input, out1: {:?}",
                String::from_utf8_lossy(&out1)
            );
            assert!(
                got_m2,
                "session 2 should have received its input, out2: {:?}",
                String::from_utf8_lossy(&out2)
            );

            // -- Verify no cross-wiring --
            let text1 = String::from_utf8_lossy(&out1);
            let text2 = String::from_utf8_lossy(&out2);
            assert!(
                !text1.contains(marker2),
                "session 1 output must not contain session 2 marker (cross-wiring): {text1}"
            );
            assert!(
                !text2.contains(marker1),
                "session 2 output must not contain session 1 marker (cross-wiring): {text2}"
            );

            // -- Resize session 1 only --
            req_tx.send(Request::Resize(1, 50, 10)).unwrap();
            // Small settle window for TIOCSWINSZ delivery.
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Drain any events caused by Resize.
            while let Ok(ev) = ev_rx.try_recv() {
                match ev {
                    Event::Sessions(_) | Event::Output(..) => {}
                    Event::Error(e) => panic!("unexpected error after resize: {e}"),
                    Event::Exited(_) => panic!("unexpected exit after resize"),
                }
            }

            // -- Verify session 2 still works after session 1 resize --
            let marker2b = "JMS2_POST_RESIZE_OK";
            req_tx
                .send(Request::Input(2, format!("echo {marker2b}\n").into_bytes()))
                .unwrap();
            let mut out2b = Vec::new();
            let mut got_m2b = false;
            loop {
                match tokio::time::timeout(Duration::from_secs(5), ev_rx.recv()).await {
                    Ok(Some(Event::Output(id, bytes))) => {
                        if id == 2 {
                            out2b.extend(&bytes);
                            if String::from_utf8_lossy(&out2b).contains(marker2b) {
                                got_m2b = true;
                                break;
                            }
                        }
                    }
                    Ok(Some(Event::Sessions(_))) => {}
                    Ok(Some(Event::Error(e))) => panic!("unexpected error after resize: {e}"),
                    Ok(Some(Event::Exited(id))) => {
                        panic!("unexpected Exited({id}) after resize");
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            assert!(
                got_m2b,
                "session 2 should still work after session 1 resize"
            );

            // -- Kill session 1 only --
            req_tx.send(Request::Kill(1)).unwrap();

            // Verify Exited(1) arrives.
            let mut saw_exited_1 = false;
            loop {
                match tokio::time::timeout(Duration::from_secs(5), ev_rx.recv()).await {
                    Ok(Some(Event::Exited(id))) => {
                        assert_eq!(id, 1, "only session 1 should exit, got Exited({id})");
                        saw_exited_1 = true;
                        break;
                    }
                    Ok(Some(Event::Output(..))) | Ok(Some(Event::Sessions(_))) => {
                        // Drain output/Sessions that arrive alongside Exited.
                    }
                    Ok(Some(Event::Error(e))) => panic!("unexpected error after kill: {e}"),
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            assert!(saw_exited_1, "should see Exited(1) after killing session 1");

            // -- Verify session 2 is still alive and functional after kill --
            let marker2c = "JMS2_POST_KILL_OK";
            req_tx
                .send(Request::Input(2, format!("echo {marker2c}\n").into_bytes()))
                .unwrap();
            let mut out2c = Vec::new();
            let mut got_m2c = false;
            loop {
                match tokio::time::timeout(Duration::from_secs(5), ev_rx.recv()).await {
                    Ok(Some(Event::Output(id, bytes))) => {
                        if id == 2 {
                            out2c.extend(&bytes);
                            if String::from_utf8_lossy(&out2c).contains(marker2c) {
                                got_m2c = true;
                                break;
                            }
                        }
                        if id == 1 {
                            panic!("Output for killed session 1 should not arrive");
                        }
                    }
                    Ok(Some(Event::Sessions(_))) => {}
                    Ok(Some(Event::Error(e))) => panic!("unexpected error: {e}"),
                    Ok(Some(Event::Exited(id))) => {
                        panic!("unexpected Exited({id}) after session 1 already killed");
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            assert!(
                got_m2c,
                "session 2 should still work after session 1 killed"
            );

            // -- Clean up --
            drop(req_tx);
            let _ = std::fs::remove_dir_all(&dir);
        })
        .await
        .unwrap();
    }

    /// Discriminates direct-exec from shell dispatch — the only fixture in the
    /// suite that is not shell-invariant.
    ///
    /// The session command is `/bin/echo $HOME` and the test asserts the
    /// LITERAL six-character string `$HOME` appears in the child's output:
    ///
    /// - Direct exec (attach_pty: split_whitespace + CommandBuilder::new +
    ///   cmd.arg, no shell anywhere in the path) passes `$HOME` through as
    ///   argv[1] verbatim; /bin/echo receives the characters `$HOME` and
    ///   prints them back unchanged.
    /// - An `sh -c "<command>"` regression would hand the command to a shell,
    ///   which expands `$HOME` to the user's home directory path (HOME is
    ///   present in the child env: CommandBuilder inherits the parent
    ///   environment). The output would then be the expanded path, not the
    ///   literal, and the assert below fails.
    ///
    /// Why this fixture and not others: every other PTY test in this suite is
    /// shell-invariant — `/bin/echo hello world` prints the same bytes whether
    /// exec'd directly or run under `sh -c`, `#!/bin/sh` scripts produce the
    /// same output either way, and quoting tricks like `printf '<%s>'` do not
    /// discriminate either because the shell strips the quotes before exec. A
    /// literal `$var` argument is the discriminator: only a shell expands it.
    #[tokio::test]
    async fn command_dispatch_is_direct_exec_not_shell() {
        let dir = std::env::temp_dir().join(format!("jpt-dx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = Session {
            id: 1,
            name: "direct-exec".into(),
            directory: dir.to_string_lossy().into(),
            command: "/bin/echo $HOME".into(),
        };
        let state = State::from_sessions(dir.join("sessions.json"), vec![session]);
        let (req_tx, req_rx) = unbounded_channel::<Request>();
        let (internal_tx, internal_rx) = unbounded_channel::<Event>();
        let (client_tx, mut ev_rx) = unbounded_channel::<Event>();
        let _engine = tokio::spawn(run(req_rx, internal_tx, internal_rx, client_tx, state));
        tokio::time::timeout(Duration::from_secs(5), async {
            // initial Sessions: exactly our one session
            let ev = ev_rx.recv().await.unwrap();
            assert!(
                matches!(&ev, Event::Sessions(list) if list.len() == 1 && list[0].id == 1),
                "expected initial Sessions with one session, got {ev:?}"
            );
            req_tx.send(Request::Attach(1)).unwrap();
            assert!(matches!(ev_rx.recv().await.unwrap(), Event::Sessions(_)));
            let mut out = Vec::new();
            loop {
                match ev_rx.recv().await.unwrap() {
                    Event::Output(1, bytes) => out.extend(bytes),
                    Event::Exited(1) => break,
                    Event::Error(e) => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
            let text = String::from_utf8_lossy(&out);
            assert!(
                text.contains("$HOME"),
                "expected literal '$HOME' in output: direct exec passes it as an \
                 unexpanded argv[1]; a shell dispatch (sh -c) would expand it to \
                 the home path. got {text:?}"
            );
            // Belt and braces: if the expanded home path leaked into the output,
            // the command ran under a shell even though the literal also appeared.
            // Only checked for a conventional absolute home path so the literal
            // '$HOME' output itself can never trip it.
            if let Ok(home) = std::env::var("HOME")
                && home.starts_with('/')
            {
                assert!(
                    !text.contains(home.as_str()),
                    "output contains the expanded HOME path {home:?}: \
                     the command was dispatched through a shell"
                );
            }
        })
        .await
        .unwrap();
        drop(req_tx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
