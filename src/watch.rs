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
                Some(at) => {
                    match inbox.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(input) => Some(input),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
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
            FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
            FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetLongPathNameW, OPEN_EXISTING,
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
            let event =
                || Owned::check(unsafe { CreateEventW(ptr::null(), TRUE, FALSE, ptr::null()) });
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
            let ok =
                unsafe { GetOverlappedResult(self.dir.0, &*self.overlapped, &mut bytes, FALSE) };
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
                let copy = match self.wait(&stop) {
                    Done::Stopped => break false,
                    Done::Failed => break true,
                    Done::Lost => None,
                    Done::Bytes(bytes) => Some(bytes),
                };
                let reissued = self.issue();
                let raws = copy.map_or_else(|| vec![Raw::Lost], |bytes| parse(&bytes, root));
                if raws
                    .into_iter()
                    .try_for_each(|raw| sink.send(Input::Raw(raw)))
                    .is_err()
                {
                    break false;
                }
                if reissued.is_err() {
                    break true;
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
                .as_chunks::<2>()
                .0
                .iter()
                .map(|unit| u16::from_le_bytes(*unit))
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
        let tilde = stem.rsplit_once('~').is_some_and(|(_, number)| {
            !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
        });
        tilde && stem.len() <= 8 && extension.len() <= 3 && !part.chars().any(char::is_lowercase)
    }

    /// Expansion keeps the number of components, so the tail is the relative name.
    fn long_name(root: &Path, name: &str) -> Option<String> {
        let short = wide(&root.join(name));
        let mut long = vec![0u16; 512];
        loop {
            let length =
                unsafe { GetLongPathNameW(short.as_ptr(), long.as_mut_ptr(), long.len() as u32) }
                    as usize;
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
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        os::windows::ffi::OsStrExt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::{cache::Stamp, truth::Change};

    const SOON: Duration = Duration::from_secs(2);
    const BIG: Duration = Duration::from_secs(30);

    struct Project {
        outer: PathBuf,
        root: PathBuf,
    }

    impl Project {
        fn new(name: &str) -> Project {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let outer = std::env::temp_dir().join(format!(
                "recall-watch-{}-{unique}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&outer);
            let root = outer.join("proj");
            fs::create_dir_all(&root).unwrap();
            Project { outer, root }
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn walk(&self) -> BTreeMap<String, Stamp> {
            scan::walk(&self.root, &Scope::ROOT, |_, _| Ok(true))
                .unwrap()
                .unwrap()
                .found
        }

        fn engine(&self) -> Engine {
            self.engine_with(256 * 1024)
        }

        fn engine_with(&self, buffer_bytes: usize) -> Engine {
            Engine::spawn(EngineConfig {
                root: self.root.clone(),
                debounce: Debounce::default(),
                buffer_bytes,
            })
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.outer);
        }
    }

    /// A subscriber that mirrors the engine's view the way a client would.
    struct Client {
        cmd: Sender<Cmd>,
        rx: Receiver<Update>,
        files: BTreeMap<String, Stamp>,
        cursor: Option<Cursor>,
        snapshots: usize,
        deltas: usize,
        changes: Vec<Change>,
    }

    impl Client {
        fn of(engine: &Engine) -> Client {
            Client::from(engine.subscribe(None))
        }

        fn from((cmd, rx): Link) -> Client {
            Client {
                cmd,
                rx,
                files: BTreeMap::new(),
                cursor: None,
                snapshots: 0,
                deltas: 0,
                changes: Vec::new(),
            }
        }

        fn apply(&mut self, update: Update) {
            match update {
                Update::Snapshot { at, files } => {
                    self.snapshots += 1;
                    self.files = files;
                    self.cursor = Some(at);
                }
                Update::Delta { from, at, changes } => {
                    let cursor = self.cursor.expect("a delta before any snapshot");
                    assert_eq!((cursor.epoch, cursor.generation), (at.epoch, from));
                    self.deltas += 1;
                    for change in &changes {
                        match change {
                            Change::Upsert(key, stamp) => self.files.insert(key.clone(), *stamp),
                            Change::Remove(key) => self.files.remove(key),
                        };
                    }
                    self.changes.extend(changes);
                    self.cursor = Some(at);
                }
                Update::Scanning { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }

        /// Takes updates until the mirror equals a fresh full walk.
        fn converge(&mut self, project: &Project, within: Duration) {
            let deadline = Instant::now() + within;
            loop {
                if self.cursor.is_some() && self.files == project.walk() {
                    return;
                }
                match self
                    .rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                {
                    Ok(update) => self.apply(update),
                    Err(error) => panic!(
                        "not converged within {within:?} ({error:?}): mirror has {}, disk has {}",
                        self.files.len(),
                        project.walk().len()
                    ),
                }
            }
        }

        /// Counts what still arrives once the engine has had time to settle.
        fn settle(&mut self) {
            let quiet = Debounce::default().ceiling + Debounce::default().quiet;
            while let Ok(update) = self.rx.recv_timeout(quiet) {
                self.apply(update);
            }
        }
    }

    fn keys(changes: &[Change]) -> (Vec<&str>, Vec<&str>) {
        let mut upserts = Vec::new();
        let mut removes = Vec::new();
        for change in changes {
            match change {
                Change::Upsert(key, _) => upserts.push(key.as_str()),
                Change::Remove(key) => removes.push(key.as_str()),
            }
        }
        (upserts, removes)
    }

    /// Counts what a bare reader on `root` reports, to measure event volume.
    fn counter(root: &Path, buffer_bytes: usize) -> (reader::Watcher, Receiver<Input>) {
        let (tx, rx) = mpsc::channel();
        (reader::Watcher::start(root, buffer_bytes, tx).unwrap(), rx)
    }

    fn tally(rx: &Receiver<Input>) -> (usize, usize) {
        rx.try_iter()
            .fold((0, 0), |(changed, lost), input| match input {
                Input::Raw(Raw::Changed(_)) => (changed + 1, lost),
                Input::Raw(Raw::Lost) => (changed, lost + 1),
                _ => (changed, lost),
            })
    }

    #[test]
    fn file_and_folder_edits_reach_a_subscriber_and_match_a_fresh_walk() {
        let project = Project::new("edits");
        project.write("keep.md", "k");
        let engine = project.engine();
        let mut client = Client::of(&engine);
        client.converge(&project, SOON);
        type Step = (&'static str, fn(&Path));
        let steps: [Step; 7] = [
            ("write a file", |root| {
                fs::write(root.join("a.md"), "one").unwrap()
            }),
            ("modify a file", |root| {
                fs::write(root.join("a.md"), "one two").unwrap()
            }),
            ("rename a file", |root| {
                fs::rename(root.join("a.md"), root.join("b.md")).unwrap()
            }),
            ("delete a file", |root| {
                fs::remove_file(root.join("b.md")).unwrap()
            }),
            ("create a folder", |root| {
                fs::create_dir_all(root.join("docs/deep")).unwrap();
                fs::write(root.join("docs/deep/c.md"), "c").unwrap();
            }),
            ("rename a folder", |root| {
                fs::rename(root.join("docs"), root.join("notes")).unwrap()
            }),
            ("delete a folder", |root| {
                fs::remove_dir_all(root.join("notes")).unwrap()
            }),
        ];
        for (step, act) in steps {
            let before = project.walk();
            act(&project.root);
            assert_ne!(
                before,
                project.walk(),
                "{step} should change what a walk sees"
            );
            let started = Instant::now();
            client.converge(&project, SOON);
            eprintln!("{step}: converged in {:?}", started.elapsed());
        }
        assert_eq!(client.snapshots, 1);
    }

    #[test]
    fn a_burst_of_5000_files_in_50_folders_lands_in_at_most_10_deltas() {
        let project = Project::new("burst");
        let engine = project.engine();
        let mut client = Client::of(&engine);
        client.converge(&project, SOON);
        let (watcher, events) = counter(&project.root, 256 * 1024);
        let started = Instant::now();
        for n in 0..5_000 {
            project.write(&format!("d{:02}/f{n}.md", n % 50), "burst");
        }
        let written = started.elapsed();
        client.converge(&project, BIG);
        let converged = started.elapsed();
        client.settle();
        drop(watcher);
        let (changed, lost) = tally(&events);
        eprintln!(
            "burst: wrote in {written:?}, converged in {converged:?}, {} deltas, {changed} events, {lost} overflows",
            client.deltas
        );
        assert_eq!(client.files.len(), 5_000);
        assert_eq!(client.files, project.walk());
        assert!(client.deltas <= 10, "{} deltas", client.deltas);
    }

    /// Writes 2,000 files from 8 threads into an engine with a small buffer.
    /// Returns how many overflows a bare reader with the same buffer saw.
    fn flood(name: &str, buffer_bytes: usize) -> usize {
        let project = Project::new(name);
        let engine = project.engine_with(buffer_bytes);
        let mut client = Client::of(&engine);
        client.converge(&project, SOON);
        let (watcher, events) = counter(&project.root, buffer_bytes);
        let started = Instant::now();
        let padding = "x".repeat(100);
        thread::scope(|scope| {
            for writer in 0..8 {
                let (project, padding) = (&project, &padding);
                scope.spawn(move || {
                    for n in (writer..2_000).step_by(8) {
                        project.write(&format!("d{}/{padding}{n}.md", n % 20), "flood");
                    }
                });
            }
        });
        client.converge(&project, BIG);
        let converged = started.elapsed();
        drop(watcher);
        let (changed, lost) = tally(&events);
        eprintln!(
            "{buffer_bytes} byte buffer: converged in {converged:?}, {changed} events, {lost} overflows"
        );
        assert_eq!(client.files.len(), 2_000);
        lost
    }

    #[test]
    fn a_4_kib_buffer_under_2000_files_converges() {
        flood("flood", 4096);
    }

    #[test]
    fn a_buffer_too_small_for_one_record_overflows_and_recovers() {
        assert!(flood("overflow", 16) > 0);
    }
    #[test]
    fn ignoring_a_folder_removes_its_files_and_unignoring_restores_them() {
        let project = Project::new("gitignore");
        project.write("generated/a.md", "a");
        project.write("generated/b.md", "b");
        project.write("src/x.md", "x");
        let engine = project.engine();
        let mut client = Client::of(&engine);
        client.converge(&project, SOON);
        assert!(client.files.contains_key("generated/a.md"));

        project.write(".gitignore", "generated/\n");
        client.converge(&project, SOON);
        let (_, removes) = keys(&client.changes);
        assert_eq!(removes, ["generated/a.md", "generated/b.md"]);

        client.changes.clear();
        project.write(".gitignore", "");
        client.converge(&project, SOON);
        let (upserts, _) = keys(&client.changes);
        assert!(upserts.contains(&"generated/a.md") && upserts.contains(&"generated/b.md"));
    }

    #[test]
    fn writes_under_target_and_node_modules_produce_no_update() {
        let project = Project::new("noise");
        let engine = project.engine();
        let mut client = Client::of(&engine);
        client.converge(&project, SOON);
        project.write("target/debug/out.md", "noise");
        project.write("node_modules/p/readme.md", "noise");
        project.write("sentinel.md", "signal");
        let update = client.rx.recv_timeout(SOON).unwrap();
        let Update::Delta { changes, .. } = &update else {
            panic!("expected a delta, got {update:?}");
        };
        assert_eq!(keys(changes), (vec!["sentinel.md"], vec![]));
    }

    #[test]
    fn a_deleted_root_says_gone_ends_the_engine_and_leaves_no_handle() {
        let project = Project::new("gone");
        project.write("a.md", "a");
        let engine = project.engine();
        let mut client = Client::of(&engine);
        client.converge(&project, SOON);
        let (_cmd, rx) = (client.cmd, client.rx);
        fs::remove_dir_all(&project.root).unwrap();
        let deadline = Instant::now() + SOON;
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Update::Gone) => break,
                Ok(_) => continue,
                Err(error) => panic!("no Gone: {error:?}"),
            }
        }
        assert_eq!(
            rx.recv_timeout(SOON).err(),
            Some(RecvTimeoutError::Disconnected)
        );
        fs::create_dir(&project.root).unwrap();
        fs::remove_dir_all(&project.outer).unwrap();
        assert!(!project.outer.exists());
    }

    #[test]
    fn stop_ends_the_engine_and_releases_the_folder() {
        let project = Project::new("stop");
        project.write("a.md", "a");
        let engine = project.engine();
        let mut client = Client::of(&engine);
        client.converge(&project, SOON);
        let (cmd, rx) = (client.cmd, client.rx);
        cmd.send(Cmd::Stop).unwrap();
        assert_eq!(
            rx.recv_timeout(SOON).err(),
            Some(RecvTimeoutError::Disconnected)
        );
        fs::remove_dir_all(&project.root).unwrap();
        fs::create_dir(&project.root).unwrap();
    }

    #[test]
    fn rescan_picks_up_an_ignore_rule_above_the_root_that_no_event_reports() {
        let project = Project::new("rescan");
        project.write("secret.md", "s");
        project.write("open.md", "o");
        let engine = project.engine();
        let mut client = Client::of(&engine);
        client.converge(&project, SOON);
        fs::write(project.outer.join(".gitignore"), "secret.md\n").unwrap();
        client.cmd.send(Cmd::Rescan).unwrap();
        client.converge(&project, SOON);
        assert_eq!(keys(&client.changes), (vec![], vec!["secret.md"]));
    }

    #[test]
    fn a_root_that_cannot_be_watched_is_manual_but_still_scanned() {
        let project = Project::new("manual");
        let missing = project.root.join("not-yet");
        let engine = Engine::spawn(EngineConfig {
            root: missing,
            debounce: Debounce::default(),
            buffer_bytes: 4096,
        });
        let (_cmd, rx) = engine.subscribe(None);
        let first = rx.recv_timeout(SOON).unwrap();
        assert!(matches!(first, Update::Manual { .. }), "{first:?}");
        let deadline = Instant::now() + SOON;
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Update::Snapshot { .. }) => break,
                Ok(Update::Scanning { .. }) => continue,
                other => panic!("expected a snapshot, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_disconnected_subscriber_does_not_stall_the_engine() {
        let project = Project::new("disconnect");
        project.write("a.md", "a");
        let engine = project.engine();
        let mut leaver = Client::of(&engine);
        leaver.converge(&project, SOON);
        drop(leaver);
        let _never_reads = engine.subscribe(None);
        let mut stayer = Client::of(&engine);
        stayer.converge(&project, SOON);
        for n in 0..3 {
            project.write(&format!("n{n}.md"), "n");
            stayer.converge(&project, SOON);
        }
    }

    #[test]
    fn two_subscribers_both_converge() {
        let project = Project::new("two");
        let engine = project.engine();
        let mut first = Client::of(&engine);
        let mut second = Client::of(&engine);
        project.write("a.md", "a");
        project.write("sub/b.md", "b");
        first.converge(&project, SOON);
        second.converge(&project, SOON);
        fs::remove_file(project.root.join("a.md")).unwrap();
        first.converge(&project, SOON);
        second.converge(&project, SOON);
        assert_eq!(first.files, second.files);
        assert_eq!(first.cursor, second.cursor);
    }

    #[test]
    fn a_current_cursor_resumes_with_deltas_and_a_foreign_one_gets_a_snapshot() {
        let project = Project::new("cursor");
        project.write("a.md", "a");
        let engine = project.engine();
        let mut first = Client::of(&engine);
        first.converge(&project, SOON);
        let have = first.cursor.unwrap();

        let (_cmd, current) = engine.subscribe(Some(have));
        project.write("new.md", "new");
        match current.recv_timeout(SOON).unwrap() {
            Update::Delta { from, at, changes } => {
                assert_eq!((from, at.epoch), (have.generation, have.epoch));
                assert_eq!(keys(&changes), (vec!["new.md"], vec![]));
            }
            other => panic!("expected a delta, got {other:?}"),
        }

        let foreign = Cursor {
            epoch: Epoch(have.epoch.0 ^ 1),
            generation: have.generation,
        };
        let mut resumed = Client::from(engine.subscribe(Some(foreign)));
        let update = resumed.rx.recv_timeout(SOON).unwrap();
        assert!(matches!(update, Update::Snapshot { .. }), "{update:?}");
        resumed.apply(update);
        resumed.converge(&project, SOON);
    }

    #[test]
    fn an_empty_project_still_gets_a_snapshot_at_generation_zero() {
        let project = Project::new("empty");
        let engine = project.engine();
        let (_cmd, rx) = engine.subscribe(None);
        let deadline = Instant::now() + SOON;
        let update = loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Update::Scanning { .. } => continue,
                other => break other,
            }
        };
        let Update::Snapshot { at, files } = update else {
            panic!("expected a snapshot, got {update:?}");
        };
        assert_eq!((at.generation.0, files.len()), (0, 0));
    }

    #[test]
    fn a_subscriber_during_the_first_walk_sees_scanning_then_a_snapshot() {
        let project = Project::new("scanning");
        for n in 0..3_000 {
            project.write(&format!("d{}/f{n}.md", n % 30), "x");
        }
        let engine = project.engine();
        let mut client = Client::of(&engine);
        let first = client.rx.recv_timeout(SOON).unwrap();
        assert!(matches!(first, Update::Scanning { .. }), "{first:?}");
        client.converge(&project, BIG);
        assert_eq!(client.files.len(), 3_000);
    }

    fn short_path(path: &Path) -> Option<String> {
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let long: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut short = vec![0u16; 1024];
        let length =
            unsafe { GetShortPathNameW(long.as_ptr(), short.as_mut_ptr(), short.len() as u32) };
        short.truncate(length as usize);
        String::from_utf16(&short).ok()
    }

    #[test]
    fn short_names_expand_to_long_ones_and_a_vanished_short_name_is_lost() {
        let project = Project::new("short");
        project.write("a long folder name/a long file name.md", "x");
        let file = project.root.join("a long folder name\\a long file name.md");
        let short = short_path(&file).unwrap();
        let tail: Vec<&str> = short.rsplitn(3, '\\').take(2).collect();
        let event = format!("{}\\{}", tail[1], tail[0]);
        if !event.contains('~') {
            eprintln!("8.3 names are off on this volume, skipping: {event}");
            return;
        }
        let units: Vec<u16> = event.encode_utf16().collect();
        match reader::raw_of(&units, &project.root) {
            Some(Raw::Changed(rel)) => {
                assert_eq!(rel.as_str(), "a long folder name/a long file name.md")
            }
            _ => panic!("{event} should expand"),
        }
        fs::remove_file(&file).unwrap();
        assert!(matches!(
            reader::raw_of(&units, &project.root),
            Some(Raw::Lost)
        ));
    }

    #[test]
    fn rename_records_report_the_old_and_the_new_name() {
        let record = |next: u32, action: u32, name: &str| {
            let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut bytes = Vec::new();
            bytes.extend(next.to_le_bytes());
            bytes.extend(action.to_le_bytes());
            bytes.extend((units.len() as u32).to_le_bytes());
            bytes.extend(units);
            bytes.resize(next.max(bytes.len() as u32) as usize, 0);
            bytes
        };
        let mut bytes = record(40, 4, "src\\old.rs");
        bytes.extend(record(0, 5, "src\\new.rs"));
        let names: Vec<String> = reader::parse(&bytes, Path::new("C:\\nowhere"))
            .into_iter()
            .map(|raw| match raw {
                Raw::Changed(rel) => rel.as_str().to_owned(),
                _ => panic!("expected a change"),
            })
            .collect();
        assert_eq!(names, ["src/old.rs", "src/new.rs"]);
    }
}
