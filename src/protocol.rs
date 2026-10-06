use serde::{Deserialize, Serialize};

use crate::session::Session;

/// The engine is the single source of truth. Remove addresses a session by index
/// in the latest list the engine sent. Every request is answered with a fresh Sessions event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Snapshot,
    Add,
    Remove(usize),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Sessions(Vec<Session>),
    Error(String),
}