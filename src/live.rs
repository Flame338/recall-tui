//! Keeps the TUI's LuxIndex equal to the project's files while it runs.

/// What the status bar shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Connecting,
    Scanning {
        found: usize,
    },
    /// Events are flowing.
    Live {
        via: Via,
    },
    /// Cannot watch here. A rescan still works.
    Manual {
        reason: String,
    },
    /// The project root is gone.
    Gone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Daemon,
    InProcess,
}

/// What one tick tells the TUI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// Keys whose indexed content changed.
    pub changed: Vec<String>,
    /// Files still waiting to be read or removed.
    pub pending: usize,
}
