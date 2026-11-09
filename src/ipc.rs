//! The daemon's named pipe. Wire types stay private to this module. A client
//! gets the same `watch::Link` an in-process engine gives.

use std::{io, path::Path};

use crate::{truth::Cursor, watch::Link};

#[derive(Debug)]
pub enum ConnectError {
    /// No daemon answered.
    Absent,
    /// A daemon from another build answered. The client runs its own engine.
    OtherBuild {
        daemon: String,
    },
    /// This platform has no daemon.
    Unsupported,
    Io(io::Error),
}

pub fn connect(_root: &Path, _have: Option<Cursor>) -> Result<Link, ConnectError> {
    #[cfg(windows)]
    {
        unimplemented!()
    }
    #[cfg(not(windows))]
    {
        Err(ConnectError::Unsupported)
    }
}
