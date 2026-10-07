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