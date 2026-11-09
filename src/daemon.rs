//! The background process that hosts one engine per watched project. It never
//! loads LuxCore. Clients reach it through `ipc`.

use std::error::Error;

#[derive(Debug)]
pub struct SpawnError(pub String);

/// Starts the daemon unless one already answers.
pub fn ensure_running() -> Result<(), SpawnError> {
    unimplemented!()
}

/// The body of the daemon process.
pub fn run() -> Result<(), Box<dyn Error>> {
    unimplemented!()
}
