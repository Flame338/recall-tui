//! Turns filesystem events into `Update`s. One engine thread per project owns
//! that project's `Truth` and `Dirty`. Everything else reaches it through
//! channels, so the same engine runs inside the daemon or inside the TUI.

use std::{
    error::Error,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};

use crate::{
    scan,
    truth::{Cursor, Debounce, Dirty, Epoch, Observed, Rel, Scope, Truth, Update},
};

use reader::Watcher;

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
    /// The engine stops on `Cmd::Stop`, when the root disappears, or once the
    /// `Engine` and every `Link` it handed out are dropped.
    pub fn spawn(config: EngineConfig) -> Engine {
        let (cmd, commands) = mpsc::channel();
        let (input, inbox) = mpsc::channel();
        let forward = input.clone();
        // `Cmd` and `Raw` arrive on different channels and std mpsc cannot
        // select, so commands are funneled into the one inbox the engine reads.
        thread::spawn(move || {
            for command in commands {
                if forward.send(Input::Cmd(command)).is_err() {
                    return;
                }
            }
            let _ = forward.send(Input::Cmd(Cmd::Stop));
        });
        thread::spawn(move || run(config, input, inbox));
        Engine { cmd }
    }

    /// Subscribes a new client and returns its link.
    pub fn subscribe(&self, have: Option<Cursor>) -> Link {
        let (tx, rx) = mpsc::channel();
        let _ = self.cmd.send(Cmd::Subscribe { have, tx });
        (self.cmd.clone(), rx)
    }
}

enum Input {
    Raw(Raw),
    Cmd(Cmd),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Go,
    Stop,
}

const PROGRESS_EVERY: Duration = Duration::from_millis(100);

fn run(config: EngineConfig, input: Sender<Input>, inbox: Receiver<Input>) {
    // Armed before the first walk, so a change made while it runs is either
    // seen by the walk or queued as an event. Nothing falls between them.
    let watcher = Watcher::start(&config.root, config.buffer_bytes, input);
    let mut actor = Actor {
        debounce: config.debounce,
        truth: Truth::new(Epoch::fresh()),
        dirty: Dirty::default(),
        subs: Vec::new(),
        manual: watcher.as_ref().err().cloned(),
        scanning: Some(0),
    };
    actor.serve(&config.root, &inbox);
    // The directory handle closes before subscribers see their channel end.
    drop(watcher);
}

struct Sub {
    tx: Sender<Update>,
    cursor: Option<Cursor>,
}

impl Sub {
    /// Sends what changed since this client's cursor. False once the client is gone.
    fn catch_up(&mut self, truth: &Truth) -> bool {
        if self.cursor == Some(truth.cursor()) {
            return true;
        }
        let update = truth.since(self.cursor);
        self.cursor = Some(truth.cursor());
        self.tx.send(update).is_ok()
    }
}

struct Actor {
    debounce: Debounce,
    truth: Truth,
    dirty: Dirty,
    subs: Vec<Sub>,
    /// Why this project is not watched. Rescans still work.
    manual: Option<String>,
    /// Files found so far, until the first full walk succeeds.
    scanning: Option<usize>,
}

impl Actor {
    fn serve(&mut self, root: &Path, inbox: &Receiver<Input>) {
        if self.round(root, vec![Scope::ROOT], inbox) == Flow::Stop {
            return;
        }
        loop {
            let input = match self.dirty.deadline(&self.debounce) {
                None => match inbox.recv() {
                    Ok(input) => Some(input),
                    Err(_) => return,
                },
                Some(at) => match inbox.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(input) => Some(input),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            if let Some(input) = input
                && self.take(input) == Flow::Stop
            {
                return;
            }
            let due = self
                .dirty
                .deadline(&self.debounce)
                .is_some_and(|at| at <= Instant::now());
            if due {
                let scopes = self
                    .dirty
                    .resolve(&self.truth, &scan::Disk(root), &self.debounce);
                if self.round(root, scopes, inbox) == Flow::Stop {
                    return;
                }
            }
        }
    }

    fn take(&mut self, input: Input) -> Flow {
        let now = Instant::now();
        match input {
            Input::Raw(Raw::Changed(rel)) => self.dirty.note(rel, now, &self.debounce),
            Input::Raw(Raw::Lost) | Input::Cmd(Cmd::Rescan) => self.dirty.overflow(now),
            Input::Raw(Raw::Gone) => {
                self.broadcast(Update::Gone);
                return Flow::Stop;
            }
            Input::Cmd(Cmd::Stop) => return Flow::Stop,
            Input::Cmd(Cmd::Subscribe { have, tx }) => self.subscribe(have, tx),
        }
        Flow::Go
    }

    fn subscribe(&mut self, have: Option<Cursor>, tx: Sender<Update>) {
        let mut sub = Sub { tx, cursor: have };
        if let Some(reason) = &self.manual {
            let manual = Update::Manual {
                reason: reason.clone(),
            };
            if sub.tx.send(manual).is_err() {
                return;
            }
        }
        let alive = match self.scanning {
            Some(found) => sub.tx.send(Update::Scanning { found }).is_ok(),
            None => sub.catch_up(&self.truth),
        };
        if alive {
            self.subs.push(sub);
        }
    }

    fn broadcast(&mut self, update: Update) {
        self.subs.retain(|sub| sub.tx.send(update.clone()).is_ok());
    }

    fn publish(&mut self) {
        let truth = &self.truth;
        self.subs.retain_mut(|sub| sub.catch_up(truth));
    }

    /// Walks each scope and applies what it saw. A failed walk applies nothing
    /// and marks the whole project dirty, so the next round retries it all.
    fn round(&mut self, root: &Path, scopes: Vec<Scope>, inbox: &Receiver<Input>) -> Flow {
        let mut failed = false;
        for scope in scopes {
            match self.walk(root, &scope, inbox) {
                None => return Flow::Stop,
                Some(Ok(observed)) => {
                    self.truth.apply(observed);
                }
                Some(Err(_)) => {
                    failed = true;
                    self.dirty.overflow(Instant::now());
                }
            }
        }
        if !failed {
            self.scanning = None;
        }
        if self.scanning.is_none() {
            self.publish();
        }
        Flow::Go
    }

    /// Keeps serving the inbox while the walk runs. `None` means stop.
    fn walk(
        &mut self,
        root: &Path,
        scope: &Scope,
        inbox: &Receiver<Input>,
    ) -> Option<Result<Observed, Box<dyn Error>>> {
        let mut stop = false;
        let mut shown = Instant::now();
        let walked = scan::walk(root, scope, |found, _| {
            while let Ok(input) = inbox.try_recv() {
                if self.take(input) == Flow::Stop {
                    stop = true;
                    return Ok(false);
                }
            }
            if self.scanning.is_some() && shown.elapsed() >= PROGRESS_EVERY {
                shown = Instant::now();
                self.scanning = Some(found);
                self.broadcast(Update::Scanning { found });
            }
            Ok(true)
        });
        match walked {
            Ok(Some(observed)) => Some(Ok(observed)),
            Ok(None) => None,
            Err(error) => (!stop).then_some(Err(error)),
        }
    }
}

#[cfg(not(windows))]
mod reader {
    use std::{path::Path, sync::mpsc::Sender};

    use super::Input;

    pub struct Watcher;

    impl Watcher {
        pub fn start(_: &Path, _: usize, _: Sender<Input>) -> Result<Watcher, String> {
            Err("live watching is only built for Windows".into())
        }
    }
}

#[cfg(windows)]
mod reader {
    use std::{
        ffi::OsString,
        io,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::Path,
        ptr,
        sync::mpsc::Sender,
        thread::{self, JoinHandle},
    };

    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_NOTIFY_ENUM_DIR, FALSE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
            TRUE, WAIT_OBJECT_0,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
            FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
            FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetLongPathNameW, OPEN_EXISTING,
            ReadDirectoryChangesW,
        },
        System::{
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
            Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects},
        },
    };

    use super::{Input, Raw};
    use crate::truth::Rel;

    const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_CREATION;

    struct Owned(HANDLE);

    // A kernel handle is a process-wide value, usable from any thread.
    unsafe impl Send for Owned {}

    impl Owned {
        fn check(handle: HANDLE) -> io::Result<Owned> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Owned(handle))
            }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    struct Borrowed(HANDLE);

    // The `Watcher` that owns this handle joins the thread before closing it.
    unsafe impl Send for Borrowed {}

    /// One subtree watch on a project root, read by its own thread.
    pub struct Watcher {
        stop: Owned,
        thread: Option<JoinHandle<()>>,
    }

    impl Watcher {
        /// Returns once the first read is queued, so every later change is reported.
        pub fn start(
            root: &Path,
            buffer_bytes: usize,
            sink: Sender<Input>,
        ) -> Result<Watcher, String> {
            let cannot = |error: io::Error| format!("cannot watch {}: {error}", root.display());
            let path = wide(root);
            let dir = Owned::check(unsafe {
                CreateFileW(
                    path.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            })
            .map_err(cannot)?;
            let event = || Owned::check(unsafe { CreateEventW(ptr::null(), TRUE, FALSE, ptr::null()) });
            let stop = event().map_err(cannot)?;
            let mut read = Read {
                overlapped: Box::new(unsafe { std::mem::zeroed() }),
                buffer: vec![0; buffer_bytes.div_ceil(4).max(1)],
                pending: false,
                io: event().map_err(cannot)?,
                dir,
            };
            read.issue()
                .map_err(|code| cannot(io::Error::from_raw_os_error(code as i32)))?;
            let signal = Borrowed(stop.0);
            let root = root.to_path_buf();
            let thread = thread::spawn(move || read.pump(signal, &root, &sink));
            Ok(Watcher {
                stop,
                thread: Some(thread),
            })
        }
    }

    impl Drop for Watcher {
        fn drop(&mut self) {
            unsafe { SetEvent(self.stop.0) };
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct Read {
        // Boxed and owned here so both stay put while the kernel writes to them.
        overlapped: Box<OVERLAPPED>,
        buffer: Vec<u32>,
        pending: bool,
        io: Owned,
        dir: Owned,
    }

    // Moved to the reader thread once and only touched there afterward.
    unsafe impl Send for Read {}

    enum Done {
        Bytes(Vec<u8>),
        Lost,
        Failed,
        Stopped,
    }

    impl Read {
        fn issue(&mut self) -> Result<(), u32> {
            *self.overlapped = unsafe { std::mem::zeroed() };
            self.overlapped.hEvent = self.io.0;
            let queued = unsafe {
                ReadDirectoryChangesW(
                    self.dir.0,
                    self.buffer.as_mut_ptr().cast(),
                    (self.buffer.len() * 4) as u32,
                    TRUE,
                    FILTER,
                    ptr::null_mut(),
                    &mut *self.overlapped,
                    None,
                )
            };
            if queued == 0 {
                return Err(unsafe { GetLastError() });
            }
            self.pending = true;
            Ok(())
        }

        fn wait(&mut self, stop: &Borrowed) -> Done {
            let handles = [self.io.0, stop.0];
            let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), FALSE, INFINITE) };
            if woke != WAIT_OBJECT_0 {
                return Done::Stopped;
            }
            let mut bytes = 0;
            let ok = unsafe { GetOverlappedResult(self.dir.0, &*self.overlapped, &mut bytes, FALSE) };
            self.pending = false;
            if ok == 0 {
                return match unsafe { GetLastError() } {
                    ERROR_NOTIFY_ENUM_DIR => Done::Lost,
                    _ => Done::Failed,
                };
            }
            if bytes == 0 {
                return Done::Lost;
            }
            let filled = unsafe {
                std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), bytes as usize)
            };
            Done::Bytes(filled.to_vec())
        }

        fn pump(mut self, stop: Borrowed, root: &Path, sink: &Sender<Input>) {
            let gone = loop {
                let raws = match self.wait(&stop) {
                    Done::Stopped => break false,
                    Done::Failed => break true,
                    Done::Lost => vec![Raw::Lost],
                    Done::Bytes(bytes) => {
                        let reissued = self.issue();
                        let raws = parse(&bytes, root);
                        if reissued.is_err() {
                            let _ = raws.into_iter().try_for_each(|raw| sink.send(Input::Raw(raw)));
                            break true;
                        }
                        raws
                    }
                };
                if !self.pending && self.issue().is_err() {
                    break true;
                }
                if raws.into_iter().try_for_each(|raw| sink.send(Input::Raw(raw))).is_err() {
                    break false;
                }
            };
            drop(self);
            if gone {
                let _ = sink.send(Input::Raw(Raw::Gone));
            }
        }
    }

    impl Drop for Read {
        fn drop(&mut self) {
            if self.pending {
                let mut bytes = 0;
                unsafe {
                    CancelIoEx(self.dir.0, &*self.overlapped);
                    GetOverlappedResult(self.dir.0, &*self.overlapped, &mut bytes, TRUE);
                }
            }
        }
    }

    /// Walks the FILE_NOTIFY_INFORMATION records. Renames report the old and
    /// the new name as two records, and both become a change.
    pub(super) fn parse(bytes: &[u8], root: &Path) -> Vec<Raw> {
        let word = |at: usize| {
            bytes
                .get(at..at + 4)
                .map(|word| u32::from_le_bytes(word.try_into().unwrap()) as usize)
        };
        let mut raws = Vec::new();
        let mut at = 0;
        while let (Some(next), Some(length)) = (word(at), word(at + 8)) {
            let Some(name) = bytes.get(at + 12..at + 12 + length) else {
                break;
            };
            let units: Vec<u16> = name
                .chunks_exact(2)
                .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
                .collect();
            raws.extend(raw_of(&units, root));
            if next == 0 {
                break;
            }
            at += next;
        }
        raws
    }

    /// `Rel::from_event` expects long names, so an 8.3 short name is expanded
    /// first. One that no longer exists cannot be, and becomes `Lost`.
    pub(super) fn raw_of(name: &[u16], root: &Path) -> Option<Raw> {
        let name = String::from_utf16(name).ok()?;
        if !name.split('\\').any(looks_short) {
            return Rel::from_event(&name).map(Raw::Changed);
        }
        match long_name(root, &name) {
            Some(long) => Rel::from_event(&long).map(Raw::Changed),
            None => Some(Raw::Lost),
        }
    }

    /// Generated short names look like `PROGRA~1` or `LONGFI~2.MD`.
    fn looks_short(part: &str) -> bool {
        let (stem, extension) = part.rsplit_once('.').unwrap_or((part, ""));
        let tilde = stem
            .rsplit_once('~')
            .is_some_and(|(_, number)| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()));
        tilde && stem.len() <= 8 && extension.len() <= 3 && !part.chars().any(char::is_lowercase)
    }

    /// Expansion keeps the number of components, so the tail is the relative name.
    fn long_name(root: &Path, name: &str) -> Option<String> {
        let short = wide(&root.join(name));
        let mut long = vec![0u16; 512];
        loop {
            let length = unsafe { GetLongPathNameW(short.as_ptr(), long.as_mut_ptr(), long.len() as u32) } as usize;
            if length == 0 {
                return None;
            }
            if length < long.len() {
                long.truncate(length);
                break;
            }
            long.resize(length, 0);
        }
        let long = OsString::from_wide(&long).into_string().ok()?;
        let parts: Vec<&str> = long.split('\\').collect();
        let keep = name.split('\\').count();
        Some(parts.get(parts.len().checked_sub(keep)?..)?.join("\\"))
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect()
    }
}
