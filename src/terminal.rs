//! Channel-backed IO adapters that let the terminal view talk to the engine.

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, Sender};

use crate::engine::EngineHandle;
use crate::protocol::Request;

/// Engine `Output` chunks consumed by gpui-terminal's reader thread.
pub struct EventReader {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    offset: usize,
}

impl EventReader {
    pub fn new(rx: Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            pending: Vec::new(),
            offset: 0,
        }
    }
}

impl Read for EventReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.offset == self.pending.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.pending = chunk;
                    self.offset = 0;
                }
                // Engine gone or session ended: EOF for the terminal.
                Err(_) => return Ok(0),
            }
        }
        let n = (self.pending.len() - self.offset).min(buf.len());
        buf[..n].copy_from_slice(&self.pending[self.offset..self.offset + n]);
        self.offset += n;
        if self.offset == self.pending.len() {
            self.pending.clear();
            self.offset = 0;
        }
        Ok(n)
    }
}

/// Keystrokes written by the terminal view, forwarded to the session's pty.
pub struct EngineWriter {
    engine: EngineHandle,
    id: u64,
}

impl EngineWriter {
    pub fn new(engine: EngineHandle, id: u64) -> Self {
        Self { engine, id }
    }
}

impl Write for EngineWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.engine.send(Request::Input(self.id, buf.to_vec()));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Feeds engine `Output` chunks to a session's reader.
pub type Feed = Sender<Vec<u8>>;

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled as _, TestAppContext,
        VisualTestContext, Window, div,
    };
    use gpui_terminal::{TerminalConfig, TerminalView};

    struct Harness {
        terminal: Entity<TerminalView>,
    }

    impl Render for Harness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let focus = self.terminal.read(cx).focus_handle().clone();
            focus.focus(window, cx);
            div().size_full().child(self.terminal.clone())
        }
    }

    #[test]
    fn typed_text_is_forwarded_to_the_engine() {
        let mut app = TestAppContext::single();
        let (requests, mut sent) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let engine = EngineHandle::from_sender(requests);
        let (_feed, reader_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let writer = EngineWriter::new(engine, 7);
        let reader = EventReader::new(reader_rx);

        let window = app.add_window(|_window, cx| {
            let terminal =
                cx.new(|cx| TerminalView::new(writer, reader, TerminalConfig::default(), cx));
            Harness { terminal }
        });

        let mut cx = VisualTestContext::from_window(window.into(), &app);
        cx.simulate_input("hey");

        let mut typed = Vec::new();
        while let Ok(request) = sent.try_recv() {
            if let Request::Input(7, bytes) = request {
                typed.extend(bytes);
            }
        }
        assert_eq!(String::from_utf8_lossy(&typed), "hey");
    }
}