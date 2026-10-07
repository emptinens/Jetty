use serde::{Deserialize, Serialize};

use crate::session::Session;

/// A session to create; empty fields fall back to the environment defaults.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NewSession {
    pub name: String,
    pub directory: String,
    pub command: String,
}

/// ids are stable across removals; all target requests address a session by id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Snapshot,
    Add(NewSession),
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
