use serde::{Deserialize, Serialize};

use crate::session::Session;

/// ids are stable across removals; all target requests address a session by id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Snapshot,
    Add,
    Remove(u64),
    Attach(u64),
    Input(u64, Vec<u8>),
    Resize(u64, u16, u16),
    Kill(u64),
    SetKillOnDrop(bool),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Sessions(Vec<Session>),
    Output(u64, Vec<u8>),
    Exited(u64),
    Error(String),
}