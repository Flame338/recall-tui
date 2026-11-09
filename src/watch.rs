//! Turns filesystem events into `Update`s. One engine thread per project owns
//! that project's `Truth` and `Dirty`. Everything else reaches it through
//! channels, so the same engine runs inside the daemon or inside the TUI.

use std::{
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
};

use crate::truth::{Cursor, Debounce, Rel, Update};

/// A client's two ends: commands in, updates out. An in-process engine and
/// the daemon pipe both hand out this pair, so a client cannot tell them apart.
pub type Link = (Sender<Cmd>, Receiver<Update>);

pub enum Cmd {
    /// Stream updates to `tx`, starting from `have`. `None` starts with a snapshot.
    Subscribe {
        have: Option<Cursor>,
        tx: Sender<Update>,
    },
    /// Mark the whole project dirty.
    Rescan,
    /// End the engine thread.
    Stop,
}

/// What the directory reader tells the engine.
pub enum Raw {
    Changed(Rel),
    /// The event buffer overflowed, so some events were lost.
    Lost,
    /// The root is gone or the directory handle failed.
    Gone,
}

pub struct EngineConfig {
    pub root: PathBuf,
    pub debounce: Debounce,
    /// Size of the ReadDirectoryChangesW buffer.
    pub buffer_bytes: usize,
}

pub struct Engine {
    pub cmd: Sender<Cmd>,
}

impl Engine {
    pub fn spawn(_config: EngineConfig) -> Engine {
        unimplemented!()
    }

    /// Subscribes a new client and returns its link.
    pub fn subscribe(&self, _have: Option<Cursor>) -> Link {
        unimplemented!()
    }
}
