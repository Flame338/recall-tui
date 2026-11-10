//! The daemon's named pipe. Wire types stay private to this module. A client
//! gets the same `watch::Link` an in-process engine gives.
//!
//! One message per line: UTF-8, LF framed, tab separated, keys last, the same
//! style as the cache manifest. Roots travel as hex of their UTF-8 bytes.
//! Every line is parsed into domain types here, so nothing past this module
//! sees a wire string. The pipe carries state, never queries or file bodies.
//!
//! ```text
//! client -> daemon                     daemon -> client
//! HELLO proto build                    WELCOME proto build pid
//! SUBSCRIBE root epoch|- gen|-         SCANNING found
//! RESCAN                               SNAPSHOT epoch gen count, then F size mtime key
//! BYE                                  DELTA epoch from to count, then U size mtime key | R key
//! STATUS                               MANUAL reason
//! STOP                                 GONE
//!                                      ERR text
//!                                      STATUS-REPLY count, then P gen files clients idle|- root
//! ```

use std::{
    collections::{BTreeMap, HashSet},
    fmt, io,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use crate::{
    cache::Stamp,
    truth::{Change, Cursor, Epoch, Gen, Update},
    watch::Link,
};

#[cfg(all(windows, test))]
pub(crate) use pipe::stalled_subscriber;
#[cfg(windows)]
pub(crate) use pipe::{
    Peer, PipeName, Stop, Writer, build_id, connect_to, listen, ping, status, stop,
};

#[derive(Debug)]
pub enum ConnectError {
    Absent,
    OtherBuild {
        daemon: String,
    },
    Unsupported,
    Io(io::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Absent => write!(f, "no daemon is running"),
            ConnectError::OtherBuild { daemon } => {
                write!(f, "the daemon runs another build ({daemon})")
            }
            ConnectError::Unsupported => write!(f, "this platform has no daemon"),
            ConnectError::Io(error) => write!(f, "daemon pipe: {error}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<io::Error> for ConnectError {
    fn from(error: io::Error) -> Self {
        ConnectError::Io(error)
    }
}

pub fn connect(root: &Path, have: Option<Cursor>) -> Result<Link, ConnectError> {
    #[cfg(windows)]
    {
        connect_to(&PipeName::from_env(), build_id(), root, have)
    }
    #[cfg(not(windows))]
    {
        let _ = (root, have);
        Err(ConnectError::Unsupported)
    }
}

const PROTO: u32 = 1;
const MAX_LINE: u64 = 1 << 16;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Subscribe { root: PathBuf, have: Option<Cursor> },
    Rescan,
    Bye,
    Status,
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectStatus {
    pub root: PathBuf,
    pub generation: Gen,
    pub files: usize,
    pub clients: usize,
    pub idle: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Daemon {
    pub build: String,
    pub pid: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum ToDaemon {
    Hello { proto: u32, build: String },
    Request(Request),
}

#[derive(Debug, PartialEq, Eq)]
enum ToClient {
    Welcome { proto: u32, daemon: Daemon },
    Update(Update),
    Err(String),
    Status(Vec<ProjectStatus>),
}

fn bad(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("malformed pipe message: {what}"),
    )
}

fn encode_request(message: &ToDaemon) -> io::Result<String> {
    Ok(match message {
        ToDaemon::Hello { proto, build } => format!("HELLO\t{proto}\t{}\n", clean(build)),
        ToDaemon::Request(Request::Subscribe { root, have }) => {
            let (epoch, generation) = match have {
                Some(cursor) => (
                    format!("{:016x}", cursor.epoch.0),
                    cursor.generation.0.to_string(),
                ),
                None => ("-".to_owned(), "-".to_owned()),
            };
            let root = root.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "the project root is not UTF-8")
            })?;
            format!("SUBSCRIBE\t{}\t{epoch}\t{generation}\n", hex(root))
        }
        ToDaemon::Request(Request::Rescan) => "RESCAN\n".to_owned(),
        ToDaemon::Request(Request::Bye) => "BYE\n".to_owned(),
        ToDaemon::Request(Request::Status) => "STATUS\n".to_owned(),
        ToDaemon::Request(Request::Stop) => "STOP\n".to_owned(),
    })
}

fn decode_request(line: &str) -> io::Result<ToDaemon> {
    let request = match split_tag(line) {
        ("HELLO", Some(rest)) => {
            let [proto, build] = fields(rest)?;
            return Ok(ToDaemon::Hello {
                proto: number(proto)?,
                build: text(build)?,
            });
        }
        ("SUBSCRIBE", Some(rest)) => {
            let [root, epoch, generation] = fields(rest)?;
            let have = match (epoch, generation) {
                ("-", "-") => None,
                (epoch, generation) => Some(Cursor {
                    epoch: parse_epoch(epoch)?,
                    generation: Gen(number(generation)?),
                }),
            };
            Request::Subscribe {
                root: parse_root(root)?,
                have,
            }
        }
        ("RESCAN", None) => Request::Rescan,
        ("BYE", None) => Request::Bye,
        ("STATUS", None) => Request::Status,
        ("STOP", None) => Request::Stop,
        _ => return Err(bad("unknown request")),
    };
    Ok(ToDaemon::Request(request))
}

fn encode_reply(message: &ToClient) -> String {
    match message {
        ToClient::Welcome { proto, daemon } => format!(
            "WELCOME\t{proto}\t{}\t{}\n",
            clean(&daemon.build),
            daemon.pid
        ),
        ToClient::Update(Update::Scanning { found }) => format!("SCANNING\t{found}\n"),
        ToClient::Update(Update::Snapshot { at, files }) => {
            let mut text = format!(
                "SNAPSHOT\t{:016x}\t{}\t{}\n",
                at.epoch.0,
                at.generation.0,
                files.len()
            );
            for (key, stamp) in files {
                text.push_str(&format!("F\t{}\t{}\t{key}\n", stamp.size, stamp.modified));
            }
            text
        }
        ToClient::Update(Update::Delta { from, at, changes }) => {
            let mut text = format!(
                "DELTA\t{:016x}\t{}\t{}\t{}\n",
                at.epoch.0,
                from.0,
                at.generation.0,
                changes.len()
            );
            for change in changes {
                text.push_str(&match change {
                    Change::Upsert(key, stamp) => {
                        format!("U\t{}\t{}\t{key}\n", stamp.size, stamp.modified)
                    }
                    Change::Remove(key) => format!("R\t{key}\n"),
                });
            }
            text
        }
        ToClient::Update(Update::Manual { reason }) => format!("MANUAL\t{}\n", clean(reason)),
        ToClient::Update(Update::Gone) => "GONE\n".to_owned(),
        ToClient::Err(message) => format!("ERR\t{}\n", clean(message)),
        ToClient::Status(projects) => {
            let mut text = format!("STATUS-REPLY\t{}\n", projects.len());
            for project in projects {
                let idle = project
                    .idle
                    .map_or("-".to_owned(), |idle| idle.as_secs().to_string());
                text.push_str(&format!(
                    "P\t{}\t{}\t{}\t{idle}\t{}\n",
                    project.generation.0,
                    project.files,
                    project.clients,
                    hex(&project.root.to_string_lossy())
                ));
            }
            text
        }
    }
}

fn decode_reply(
    header: &str,
    more: &mut dyn FnMut() -> io::Result<Option<String>>,
) -> io::Result<ToClient> {
    let mut body = || more()?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof));
    let update = match split_tag(header) {
        ("WELCOME", Some(rest)) => {
            let [proto, build, pid] = fields(rest)?;
            return Ok(ToClient::Welcome {
                proto: number(proto)?,
                daemon: Daemon {
                    build: text(build)?,
                    pid: number(pid)?,
                },
            });
        }
        ("SCANNING", Some(rest)) => {
            let [found] = fields(rest)?;
            Update::Scanning {
                found: number(found)?,
            }
        }
        ("SNAPSHOT", Some(rest)) => {
            let [epoch, generation, count] = fields(rest)?;
            let at = Cursor {
                epoch: parse_epoch(epoch)?,
                generation: Gen(number(generation)?),
            };
            let mut files = BTreeMap::new();
            for _ in 0..number::<u64>(count)? {
                let line = body()?;
                let ("F", Some(rest)) = split_tag(&line) else {
                    return Err(bad("expected an F line"));
                };
                let [size, modified, key] = fields(rest)?;
                let stamp = parse_stamp(size, modified)?;
                if files.insert(parse_key(key)?, stamp).is_some() {
                    return Err(bad("duplicate key"));
                }
            }
            Update::Snapshot { at, files }
        }
        ("DELTA", Some(rest)) => {
            let [epoch, from, to, count] = fields(rest)?;
            let from = Gen(number(from)?);
            let at = Cursor {
                epoch: parse_epoch(epoch)?,
                generation: Gen(number(to)?),
            };
            if from > at.generation {
                return Err(bad("delta runs backwards"));
            }
            let mut changes = Vec::new();
            let mut seen = HashSet::new();
            for _ in 0..number::<u64>(count)? {
                let line = body()?;
                let change = match split_tag(&line) {
                    ("U", Some(rest)) => {
                        let [size, modified, key] = fields(rest)?;
                        Change::Upsert(parse_key(key)?, parse_stamp(size, modified)?)
                    }
                    ("R", Some(key)) => Change::Remove(parse_key(key)?),
                    _ => return Err(bad("expected a U or R line")),
                };
                let (Change::Upsert(key, _) | Change::Remove(key)) = &change;
                if !seen.insert(key.clone()) {
                    return Err(bad("duplicate key"));
                }
                changes.push(change);
            }
            Update::Delta { from, at, changes }
        }
        ("MANUAL", Some(reason)) => Update::Manual {
            reason: text(reason)?,
        },
        ("GONE", None) => Update::Gone,
        ("ERR", Some(message)) => return Ok(ToClient::Err(text(message)?)),
        ("STATUS-REPLY", Some(rest)) => {
            let [count] = fields(rest)?;
            let mut projects = Vec::new();
            for _ in 0..number::<u64>(count)? {
                let line = body()?;
                let ("P", Some(rest)) = split_tag(&line) else {
                    return Err(bad("expected a P line"));
                };
                let [generation, files, clients, idle, root] = fields(rest)?;
                projects.push(ProjectStatus {
                    root: parse_root(root)?,
                    generation: Gen(number(generation)?),
                    files: number(files)?,
                    clients: number(clients)?,
                    idle: match idle {
                        "-" => None,
                        seconds => Some(Duration::from_secs(number(seconds)?)),
                    },
                });
            }
            return Ok(ToClient::Status(projects));
        }
        _ => return Err(bad("unknown reply")),
    };
    Ok(ToClient::Update(update))
}

fn split_tag(line: &str) -> (&str, Option<&str>) {
    match line.split_once('\t') {
        Some((tag, rest)) => (tag, Some(rest)),
        None => (line, None),
    }
}

fn fields<const N: usize>(rest: &str) -> io::Result<[&str; N]> {
    let parts: Vec<&str> = rest.splitn(N, '\t').collect();
    parts.try_into().map_err(|_| bad("missing field"))
}

fn number<T: FromStr>(field: &str) -> io::Result<T> {
    if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(bad("not a number"));
    }
    field.parse().map_err(|_| bad("number out of range"))
}

fn parse_epoch(field: &str) -> io::Result<Epoch> {
    if field.is_empty() || field.len() > 16 || !field.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(bad("not an epoch"));
    }
    u64::from_str_radix(field, 16)
        .map(Epoch)
        .map_err(|_| bad("not an epoch"))
}

fn parse_stamp(size: &str, modified: &str) -> io::Result<Stamp> {
    Ok(Stamp {
        size: number(size)?,
        modified: number(modified)?,
    })
}

fn text(field: &str) -> io::Result<String> {
    if field.chars().any(char::is_control) {
        return Err(bad("control character"));
    }
    Ok(field.to_owned())
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn parse_key(field: &str) -> io::Result<String> {
    let valid = !field.is_empty()
        && !field.contains(['\\', ':'])
        && !field.chars().any(char::is_control)
        && field
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if !valid {
        return Err(bad("invalid key"));
    }
    Ok(field.to_owned())
}

fn hex(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_root(field: &str) -> io::Result<PathBuf> {
    if field.is_empty() || field.len() % 2 != 0 {
        return Err(bad("invalid root"));
    }
    let bytes = (0..field.len())
        .step_by(2)
        .map(|at| {
            field
                .get(at..at + 2)
                .filter(|pair| pair.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()
        .ok_or_else(|| bad("invalid root"))?;
    let root = String::from_utf8(bytes).map_err(|_| bad("root is not UTF-8"))?;
    let path = PathBuf::from(text(&root)?);
    if !path.is_absolute() {
        return Err(bad("root is not absolute"));
    }
    Ok(path)
}

fn read_line(reader: &mut impl io::BufRead) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut limited = io::Read::take(reader.by_ref(), MAX_LINE + 1);
    let read = io::BufRead::read_until(&mut limited, b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.pop() != Some(b'\n') {
        return Err(if read as u64 > MAX_LINE {
            bad("line too long")
        } else {
            io::ErrorKind::UnexpectedEof.into()
        });
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| bad("not UTF-8"))
}

fn read_reply(reader: &mut impl io::BufRead) -> io::Result<Option<ToClient>> {
    let Some(header) = read_line(reader)? else {
        return Ok(None);
    };
    decode_reply(&header, &mut || read_line(reader)).map(Some)
}

/// FNV-1a, 64 bit. Stable across builds, unlike the std hasher.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(windows)]
mod pipe {
    use std::{
        env,
        ffi::c_void,
        fs,
        io::{self, BufReader, Read},
        mem,
        path::Path,
        ptr,
        sync::{Arc, Mutex, OnceLock, PoisonError, mpsc},
        thread,
        time::{Duration, Instant},
    };

    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
            ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ,
            GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        },
        Security::{
            ACL, ACL_REVISION, AddAccessAllowedAce, CreateWellKnownSid, GetLengthSid,
            GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor, PSID,
            SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE,
            SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ALL_ACCESS, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
            OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION,
            SECURITY_SQOS_PRESENT, WriteFile,
        },
        System::{
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe,
                GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
            },
            Threading::{
                CreateEventW, GetCurrentProcess, INFINITE, OpenProcess, OpenProcessToken,
                PROCESS_QUERY_LIMITED_INFORMATION, SetEvent, WaitForMultipleObjects,
                WaitForSingleObject,
            },
        },
    };

    use super::{
        ConnectError, Daemon, PROTO, ProjectStatus, Request, ToClient, ToDaemon, bad,
        decode_request, encode_reply, encode_request, fnv1a, read_line, read_reply,
    };
    use crate::{
        cache::Stamp,
        truth::{Cursor, Update},
        watch::{Cmd, Link},
    };

    const BUFFER: u32 = 64 * 1024;
    const OPEN_TIMEOUT: Duration = Duration::from_secs(1);
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
    const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

    struct Handle(HANDLE);

    // SAFETY: a kernel handle is a process-wide value, and every call made on
    // one here is safe from any thread.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn millis(timeout: Option<Duration>) -> u32 {
        timeout.map_or(INFINITE, |timeout| {
            timeout.as_millis().min(u128::from(INFINITE - 1)) as u32
        })
    }

    struct Event(Handle);

    impl Event {
        fn new() -> io::Result<Event> {
            let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Event(Handle(handle)))
        }

        fn raw(&self) -> HANDLE {
            self.0.0
        }

        fn wait(&self, timeout: Option<Duration>) -> bool {
            unsafe { WaitForSingleObject(self.raw(), millis(timeout)) == WAIT_OBJECT_0 }
        }
    }

    pub(crate) struct Stop(Event);

    impl Stop {
        pub(crate) fn new() -> io::Result<Stop> {
            Event::new().map(Stop)
        }

        pub(crate) fn signal(&self) {
            unsafe { SetEvent(self.0.raw()) };
        }

        /// Waits up to `timeout` and says whether the stop was signaled.
        pub(crate) fn wait(&self, timeout: Duration) -> bool {
            self.0.wait(Some(timeout))
        }
    }

    /// One end of a pipe connection, opened for overlapped I/O. A read and a
    /// write then run at once from two threads, which a synchronous handle
    /// would serialize into a deadlock.
    struct Pipe {
        handle: Handle,
        writing: Mutex<()>,
    }

    impl Pipe {
        fn new(handle: HANDLE) -> Pipe {
            Pipe {
                handle: Handle(handle),
                writing: Mutex::new(()),
            }
        }

        fn raw(&self) -> HANDLE {
            self.handle.0
        }

        fn overlapped(
            &self,
            timeout: Option<Duration>,
            start: impl FnOnce(*mut OVERLAPPED) -> i32,
        ) -> io::Result<u32> {
            let event = Event::new()?;
            let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
            overlapped.hEvent = event.raw();
            if start(&raw mut overlapped) == 0 {
                let error = unsafe { GetLastError() };
                if error != ERROR_IO_PENDING {
                    return Err(io::Error::from_raw_os_error(error as i32));
                }
                if !event.wait(timeout) {
                    let mut done = 0;
                    unsafe {
                        CancelIoEx(self.raw(), &overlapped);
                        GetOverlappedResult(self.raw(), &overlapped, &mut done, 1);
                    }
                    return Err(io::ErrorKind::TimedOut.into());
                }
            }
            let mut done = 0;
            if unsafe { GetOverlappedResult(self.raw(), &overlapped, &mut done, 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(done)
        }

        fn read(&self, buffer: &mut [u8], timeout: Option<Duration>) -> io::Result<usize> {
            let len = buffer.len().min(u32::MAX as usize) as u32;
            let read = self.overlapped(timeout, |overlapped| unsafe {
                ReadFile(
                    self.raw(),
                    buffer.as_mut_ptr(),
                    len,
                    ptr::null_mut(),
                    overlapped,
                )
            });
            match read {
                Ok(read) => Ok(read as usize),
                Err(error)
                    if [ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED]
                        .iter()
                        .any(|&code| error.raw_os_error() == Some(code as i32)) =>
                {
                    Ok(0)
                }
                Err(error) => Err(error),
            }
        }

        fn send(&self, message: &str) -> io::Result<()> {
            let _turn = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
            let mut rest = message.as_bytes();
            while !rest.is_empty() {
                let len = rest.len().min(1 << 20) as u32;
                let written = self.overlapped(Some(WRITE_TIMEOUT), |overlapped| unsafe {
                    WriteFile(self.raw(), rest.as_ptr(), len, ptr::null_mut(), overlapped)
                })?;
                if written == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                rest = &rest[written as usize..];
            }
            Ok(())
        }

        /// Fails every pending read and write on this handle, from any thread.
        fn cancel(&self) {
            unsafe { CancelIoEx(self.raw(), ptr::null()) };
        }
    }

    struct Reading {
        pipe: Arc<Pipe>,
        timeout: Option<Duration>,
    }

    impl Read for Reading {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.pipe.read(buffer, self.timeout)
        }
    }

    fn reader(pipe: &Arc<Pipe>, timeout: Option<Duration>) -> BufReader<Reading> {
        BufReader::with_capacity(
            BUFFER as usize,
            Reading {
                pipe: Arc::clone(pipe),
                timeout,
            },
        )
    }

    /// A SID in a buffer aligned for the API.
    struct Sid(Vec<u64>);

    impl Sid {
        fn ptr(&self) -> PSID {
            self.0.as_ptr() as PSID
        }

        fn bytes(&self) -> &[u8] {
            let len = unsafe { GetLengthSid(self.ptr()) } as usize;
            unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast::<u8>(), len) }
        }

        fn of_process(process: HANDLE) -> io::Result<Sid> {
            let mut token = ptr::null_mut();
            if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = Handle(token);
            let mut len = 0;
            unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut len) };
            let mut info = vec![0u64; (len as usize).div_ceil(8)];
            if unsafe {
                GetTokenInformation(token.0, TokenUser, info.as_mut_ptr().cast(), len, &mut len)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let sid = unsafe { (*info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
            let sid_len = unsafe { GetLengthSid(sid) } as usize;
            let mut copy = vec![0u64; sid_len.div_ceil(8)];
            unsafe {
                ptr::copy_nonoverlapping(sid.cast::<u8>(), copy.as_mut_ptr().cast::<u8>(), sid_len)
            };
            Ok(Sid(copy))
        }

        fn system() -> io::Result<Sid> {
            let mut sid = Sid(vec![0u64; (SECURITY_MAX_SID_SIZE as usize).div_ceil(8)]);
            let mut len = SECURITY_MAX_SID_SIZE;
            let made = unsafe {
                CreateWellKnownSid(
                    WinLocalSystemSid,
                    ptr::null_mut(),
                    sid.0.as_mut_ptr().cast(),
                    &mut len,
                )
            };
            if made == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(sid)
        }
    }

    fn user() -> Option<&'static Sid> {
        static USER: OnceLock<Option<Sid>> = OnceLock::new();
        USER.get_or_init(|| Sid::of_process(unsafe { GetCurrentProcess() }).ok())
            .as_ref()
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct PipeName(String);

    impl PipeName {
        pub(crate) fn from_env() -> PipeName {
            PipeName::with_suffix(env::var("RECALL_PIPE_SUFFIX").ok().as_deref())
        }

        pub(crate) fn with_suffix(suffix: Option<&str>) -> PipeName {
            let who = match user() {
                Some(sid) => fnv1a(sid.bytes()),
                None => fnv1a(env::var("USERNAME").unwrap_or_default().as_bytes()),
            };
            let mut name = format!(r"\\.\pipe\recall-{who:016x}-p1");
            if let Some(suffix) = suffix.filter(|suffix| !suffix.is_empty()) {
                name.push('-');
                name.push_str(suffix);
            }
            PipeName(name)
        }

        fn wide(&self) -> Vec<u16> {
            self.0.encode_utf16().chain([0]).collect()
        }

        #[cfg(test)]
        pub(crate) fn path(&self) -> &str {
            &self.0
        }
    }

    pub(crate) fn build_id() -> &'static str {
        static ID: OnceLock<String> = OnceLock::new();
        ID.get_or_init(|| {
            let stamp = env::current_exe()
                .and_then(fs::metadata)
                .map(|metadata| Stamp::of(&metadata))
                .unwrap_or(Stamp {
                    size: 0,
                    modified: 0,
                });
            let mut bytes = stamp.size.to_le_bytes().to_vec();
            bytes.extend(stamp.modified.to_le_bytes());
            format!("{}-{:016x}", env!("CARGO_PKG_VERSION"), fnv1a(&bytes))
        })
    }

    fn create_instance(name: &[u16], first: bool) -> io::Result<Pipe> {
        let user = user().ok_or_else(|| io::Error::other("cannot read the current user's SID"))?;
        let system = Sid::system()?;
        let mut acl = vec![0u32; 256];
        let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
        let mut descriptor: SECURITY_DESCRIPTOR = unsafe { mem::zeroed() };
        let descriptor_ptr = (&raw mut descriptor).cast::<c_void>();
        let built = unsafe {
            InitializeAcl(acl_ptr, (acl.len() * 4) as u32, ACL_REVISION) != 0
                && AddAccessAllowedAce(acl_ptr, ACL_REVISION, FILE_ALL_ACCESS, user.ptr()) != 0
                && AddAccessAllowedAce(acl_ptr, ACL_REVISION, FILE_ALL_ACCESS, system.ptr()) != 0
                && InitializeSecurityDescriptor(descriptor_ptr, 1) != 0
                && SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) != 0
        };
        if !built {
            return Err(io::Error::last_os_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor_ptr,
            bInheritHandle: 0,
        };
        let first = if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | first,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                &attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Pipe::new(handle))
    }

    pub(crate) struct Listener {
        name: Vec<u16>,
        waiting: Option<Pipe>,
    }

    /// Takes the pipe name. `None` means another daemon holds it.
    pub(crate) fn listen(name: &PipeName) -> io::Result<Option<Listener>> {
        let wide = name.wide();
        match create_instance(&wide, true) {
            Ok(pipe) => Ok(Some(Listener {
                name: wide,
                waiting: Some(pipe),
            })),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(None),
            Err(error) => Err(error),
        }
    }

    impl Listener {
        /// Waits for the next client. `None` once `stop` is signaled.
        pub(crate) fn accept(&mut self, stop: &Stop) -> io::Result<Option<Peer>> {
            loop {
                let pipe = match self.waiting.take() {
                    Some(pipe) => pipe,
                    None => create_instance(&self.name, false)?,
                };
                let event = Event::new()?;
                let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
                overlapped.hEvent = event.raw();
                let connected = if unsafe { ConnectNamedPipe(pipe.raw(), &mut overlapped) } != 0 {
                    true
                } else {
                    match unsafe { GetLastError() } {
                        ERROR_PIPE_CONNECTED => true,
                        ERROR_IO_PENDING => {
                            let handles = [event.raw(), stop.0.raw()];
                            let woke =
                                unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
                            let mut done = 0;
                            if woke != WAIT_OBJECT_0 {
                                unsafe {
                                    CancelIoEx(pipe.raw(), &overlapped);
                                    GetOverlappedResult(pipe.raw(), &overlapped, &mut done, 1);
                                }
                                self.waiting = Some(pipe);
                                return Ok(None);
                            }
                            unsafe {
                                GetOverlappedResult(pipe.raw(), &overlapped, &mut done, 1) != 0
                            }
                        }
                        // The client connected and left before this call. It costs
                        // only itself, the daemon keeps serving.
                        ERROR_NO_DATA => false,
                        error => return Err(io::Error::from_raw_os_error(error as i32)),
                    }
                };
                if !connected {
                    unsafe { DisconnectNamedPipe(pipe.raw()) };
                    self.waiting = Some(pipe);
                    continue;
                }
                // Keep one instance listening at all times. Without one a client
                // sees no pipe, and another daemon could take the name.
                self.waiting = create_instance(&self.name, false).ok();
                let pipe = Arc::new(pipe);
                return Ok(Some(Peer {
                    lines: reader(&pipe, Some(HANDSHAKE_TIMEOUT)),
                    pipe,
                }));
            }
        }
    }

    pub(crate) struct Peer {
        pipe: Arc<Pipe>,
        lines: BufReader<Reading>,
    }

    impl Peer {
        pub(crate) fn handshake(&mut self, build: &str) -> io::Result<()> {
            let line = read_line(&mut self.lines)?.ok_or(io::ErrorKind::UnexpectedEof)?;
            let ToDaemon::Hello { .. } = decode_request(&line)? else {
                self.writer().error("expected HELLO");
                return Err(bad("expected HELLO"));
            };
            self.pipe.send(&encode_reply(&ToClient::Welcome {
                proto: PROTO,
                daemon: Daemon {
                    build: build.to_owned(),
                    pid: std::process::id(),
                },
            }))?;
            self.lines.get_mut().timeout = None;
            Ok(())
        }

        /// The next request. `None` when the client hung up.
        pub(crate) fn next(&mut self) -> io::Result<Option<Request>> {
            let Some(line) = read_line(&mut self.lines)? else {
                return Ok(None);
            };
            match decode_request(&line)? {
                ToDaemon::Request(request) => Ok(Some(request)),
                ToDaemon::Hello { .. } => Err(bad("HELLO twice")),
            }
        }

        pub(crate) fn writer(&self) -> Writer {
            Writer(Arc::clone(&self.pipe))
        }
    }

    #[derive(Clone)]
    pub(crate) struct Writer(Arc<Pipe>);

    impl Writer {
        pub(crate) fn update(&self, update: &Update) -> io::Result<()> {
            self.0
                .send(&encode_reply(&ToClient::Update(update.clone())))
        }

        pub(crate) fn status(&self, projects: &[ProjectStatus]) -> io::Result<()> {
            self.0
                .send(&encode_reply(&ToClient::Status(projects.to_vec())))
        }

        pub(crate) fn error(&self, message: &str) {
            let _ = self
                .0
                .send(&encode_reply(&ToClient::Err(message.to_owned())));
        }

        /// Ends the connection. Pending reads and writes on it fail at once.
        pub(crate) fn hang_up(&self) {
            self.0.cancel();
            unsafe { DisconnectNamedPipe(self.0.raw()) };
        }
    }

    fn open(name: &PipeName) -> Result<Pipe, ConnectError> {
        let wide = name.wide();
        let deadline = Instant::now() + OPEN_TIMEOUT;
        loop {
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                let pipe = Pipe::new(handle);
                check_owner(&pipe)?;
                return Ok(pipe);
            }
            match unsafe { GetLastError() } {
                ERROR_FILE_NOT_FOUND => return Err(ConnectError::Absent),
                ERROR_PIPE_BUSY => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(ConnectError::Io(io::ErrorKind::TimedOut.into()));
                    }
                    unsafe { WaitNamedPipeW(wide.as_ptr(), millis(Some(left)).max(1)) };
                }
                error => return Err(ConnectError::Io(io::Error::from_raw_os_error(error as i32))),
            }
        }
    }

    /// Pipe names are global, so another user could create ours first. Only a
    /// server running as this user is trusted.
    fn check_owner(pipe: &Pipe) -> io::Result<()> {
        let mut pid = 0;
        if unsafe { GetNamedPipeServerProcessId(pipe.raw(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let owner = Sid::of_process(Handle(process).0)?;
        match user() {
            Some(user) if user.bytes() == owner.bytes() => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the daemon pipe belongs to another user",
            )),
        }
    }

    struct Conversation {
        pipe: Arc<Pipe>,
        lines: BufReader<Reading>,
        daemon: Daemon,
    }

    impl Conversation {
        fn ask(&self, request: Request) -> io::Result<()> {
            self.pipe
                .send(&encode_request(&ToDaemon::Request(request))?)
        }
    }

    fn hello(name: &PipeName, build: &str) -> Result<Conversation, ConnectError> {
        let pipe = Arc::new(open(name)?);
        pipe.send(&encode_request(&ToDaemon::Hello {
            proto: PROTO,
            build: build.to_owned(),
        })?)?;
        let mut lines = reader(&pipe, Some(HANDSHAKE_TIMEOUT));
        match read_reply(&mut lines)? {
            Some(ToClient::Welcome {
                proto: PROTO,
                daemon,
            }) => Ok(Conversation {
                pipe,
                lines,
                daemon,
            }),
            Some(ToClient::Welcome { daemon, .. }) => Err(ConnectError::OtherBuild {
                daemon: daemon.build,
            }),
            Some(_) => Err(ConnectError::Io(bad("expected WELCOME"))),
            None => Err(ConnectError::Absent),
        }
    }

    pub(crate) fn connect_to(
        name: &PipeName,
        build: &str,
        root: &Path,
        have: Option<Cursor>,
    ) -> Result<Link, ConnectError> {
        let subscribe = encode_request(&ToDaemon::Request(Request::Subscribe {
            root: root.to_owned(),
            have,
        }))?;
        let mut conversation = hello(name, build)?;
        if conversation.daemon.build != build {
            let _ = conversation.ask(Request::Bye);
            return Err(ConnectError::OtherBuild {
                daemon: conversation.daemon.build,
            });
        }
        conversation.pipe.send(&subscribe)?;
        conversation.lines.get_mut().timeout = None;
        let Conversation {
            pipe, mut lines, ..
        } = conversation;

        let (update_tx, update_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        thread::Builder::new()
            .name("recall-ipc-read".to_owned())
            .spawn(move || {
                while let Ok(Some(ToClient::Update(update))) = read_reply(&mut lines) {
                    if update_tx.send(update).is_err() {
                        break;
                    }
                }
            })?;
        thread::Builder::new()
            .name("recall-ipc-write".to_owned())
            .spawn(move || {
                for cmd in cmd_rx {
                    let sent = match cmd {
                        Cmd::Rescan => pipe.send("RESCAN\n"),
                        Cmd::Stop => break,
                        Cmd::Subscribe { .. } => Ok(()),
                    };
                    if sent.is_err() {
                        break;
                    }
                }
                let _ = pipe.send("BYE\n");
                pipe.cancel();
            })?;
        Ok((cmd_tx, update_rx))
    }

    pub(crate) fn ping(name: &PipeName) -> Result<Daemon, ConnectError> {
        let conversation = hello(name, build_id())?;
        let _ = conversation.ask(Request::Bye);
        Ok(conversation.daemon)
    }

    pub(crate) fn status(name: &PipeName) -> Result<(Daemon, Vec<ProjectStatus>), ConnectError> {
        let mut conversation = hello(name, build_id())?;
        conversation.ask(Request::Status)?;
        match read_reply(&mut conversation.lines)? {
            Some(ToClient::Status(projects)) => {
                let _ = conversation.ask(Request::Bye);
                Ok((conversation.daemon, projects))
            }
            _ => Err(ConnectError::Io(bad("expected STATUS-REPLY"))),
        }
    }

    pub(crate) fn stop(name: &PipeName) -> Result<Daemon, ConnectError> {
        let mut conversation = hello(name, build_id())?;
        conversation.ask(Request::Stop)?;
        while let Ok(Some(_)) = read_reply(&mut conversation.lines) {}
        Ok(conversation.daemon)
    }

    #[cfg(test)]
    pub(crate) fn stalled_subscriber(
        name: &PipeName,
        build: &str,
        root: &Path,
    ) -> io::Result<fs::File> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name.0)?;
        for message in [
            ToDaemon::Hello {
                proto: PROTO,
                build: build.to_owned(),
            },
            ToDaemon::Request(Request::Subscribe {
                root: root.to_owned(),
                have: None,
            }),
        ] {
            file.write_all(encode_request(&message)?.as_bytes())?;
        }
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::temp_dir().join("recall project \u{fc}")
    }

    fn cursor() -> Cursor {
        Cursor {
            epoch: Epoch(0x00ab_cdef_0123_4567),
            generation: Gen(42),
        }
    }

    fn stamp(size: u64) -> Stamp {
        Stamp {
            size,
            modified: 1_700_000_000_123_456_789,
        }
    }

    fn lines_of(text: &str) -> impl FnMut() -> io::Result<Option<String>> + '_ {
        let mut reader = io::Cursor::new(text.as_bytes());
        move || read_line(&mut reader)
    }

    fn decode_text(text: &str) -> io::Result<ToClient> {
        let mut more = lines_of(text);
        let header = more()?.ok_or(io::ErrorKind::UnexpectedEof)?;
        decode_reply(&header, &mut more)
    }

    fn reply_round_trip(message: ToClient) {
        let encoded = encode_reply(&message);
        let mut more = lines_of(&encoded);
        let header = more().unwrap().unwrap();
        assert_eq!(
            decode_reply(&header, &mut more).unwrap(),
            message,
            "{encoded:?}"
        );
        assert_eq!(
            more().unwrap(),
            None,
            "a reply consumes exactly its own lines"
        );
    }

    #[test]
    fn every_request_round_trips() {
        let requests = [
            ToDaemon::Hello {
                proto: PROTO,
                build: "0.1.0-00ff00ff00ff00ff".to_owned(),
            },
            ToDaemon::Request(Request::Subscribe {
                root: root(),
                have: None,
            }),
            ToDaemon::Request(Request::Subscribe {
                root: root(),
                have: Some(cursor()),
            }),
            ToDaemon::Request(Request::Rescan),
            ToDaemon::Request(Request::Bye),
            ToDaemon::Request(Request::Status),
            ToDaemon::Request(Request::Stop),
        ];
        for request in requests {
            let encoded = encode_request(&request).unwrap();
            let line = encoded.strip_suffix('\n').expect("LF framed");
            assert!(!line.contains('\n'));
            assert_eq!(decode_request(line).unwrap(), request, "{encoded:?}");
        }
    }

    #[test]
    fn every_reply_round_trips() {
        reply_round_trip(ToClient::Welcome {
            proto: PROTO,
            daemon: Daemon {
                build: "0.1.0-1234".to_owned(),
                pid: 4242,
            },
        });
        reply_round_trip(ToClient::Update(Update::Scanning { found: 1234 }));
        reply_round_trip(ToClient::Update(Update::Snapshot {
            at: cursor(),
            files: BTreeMap::from([
                ("README.md".to_owned(), stamp(10)),
                ("src/a b\u{e9}.rs".to_owned(), stamp(0)),
            ]),
        }));
        reply_round_trip(ToClient::Update(Update::Snapshot {
            at: cursor(),
            files: BTreeMap::new(),
        }));
        reply_round_trip(ToClient::Update(Update::Delta {
            from: Gen(41),
            at: cursor(),
            changes: vec![
                Change::Upsert("docs/new.md".to_owned(), stamp(7)),
                Change::Remove("old/gone.txt".to_owned()),
            ],
        }));
        reply_round_trip(ToClient::Update(Update::Manual {
            reason: "cannot watch a network drive".to_owned(),
        }));
        reply_round_trip(ToClient::Update(Update::Gone));
        reply_round_trip(ToClient::Err("already subscribed".to_owned()));
        reply_round_trip(ToClient::Status(vec![
            ProjectStatus {
                root: root(),
                generation: Gen(3),
                files: 120,
                clients: 2,
                idle: None,
            },
            ProjectStatus {
                root: std::env::temp_dir(),
                generation: Gen(0),
                files: 0,
                clients: 0,
                idle: Some(Duration::from_secs(90)),
            },
        ]));
        reply_round_trip(ToClient::Status(Vec::new()));
    }

    #[test]
    fn free_text_stays_on_one_line() {
        let encoded = encode_reply(&ToClient::Update(Update::Manual {
            reason: "line\none\ttab".to_owned(),
        }));
        assert_eq!(
            decode_text(&encoded).unwrap(),
            ToClient::Update(Update::Manual {
                reason: "line one tab".to_owned()
            })
        );
    }

    #[test]
    fn a_non_utf8_root_cannot_be_sent() {
        #[cfg(windows)]
        let root = {
            use std::os::windows::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_wide(&[
                b'C'.into(),
                b':'.into(),
                0xD800,
            ]))
        };
        #[cfg(unix)]
        let root = {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]))
        };
        let error = encode_request(&ToDaemon::Request(Request::Subscribe { root, have: None }))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn malformed_requests_are_rejected() {
        let root = hex(&root().to_string_lossy());
        let relative = hex("relative/dir");
        let malformed = [
            String::new(),
            "HELLO".to_owned(),
            "HELLO\t1".to_owned(),
            "HELLO\tx\tbuild".to_owned(),
            "HELLO\t+1\tbuild".to_owned(),
            "HELLO\t1\tbu\u{7}ild".to_owned(),
            format!("SUBSCRIBE\t{root}\t-"),
            format!("SUBSCRIBE\t{root}\t-\t5"),
            format!("SUBSCRIBE\t{root}\tzz\t5"),
            format!("SUBSCRIBE\t{root}\t11112222333344445\t5"),
            format!("SUBSCRIBE\t{root}\t1\t-5"),
            format!("SUBSCRIBE\t{root}\t1\t5\textra"),
            format!("SUBSCRIBE\t{relative}\t-\t-"),
            "SUBSCRIBE\tabc\t-\t-".to_owned(),
            "SUBSCRIBE\tzz\t-\t-".to_owned(),
            "SUBSCRIBE\tff\t-\t-".to_owned(),
            format!("SUBSCRIBE\t{}\t-\t-", hex("C:\\a\nb")),
            "RESCAN\tnow".to_owned(),
            "rescan".to_owned(),
            "BYE ".to_owned(),
            "PING".to_owned(),
        ];
        for line in malformed {
            let error = decode_request(&line).expect_err(&format!("accepted {line:?}"));
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{line:?}");
        }
    }

    #[test]
    fn malformed_replies_are_rejected() {
        let epoch = "00abcdef01234567";
        let malformed = [
            "WELCOME\t1\tbuild".to_owned(),
            "WELCOME\t1\tbuild\tpid".to_owned(),
            "SCANNING\t-1".to_owned(),
            "SCANNING".to_owned(),
            format!("SNAPSHOT\t{epoch}\t1\tx"),
            format!("SNAPSHOT\t{epoch}\t1\t2\nF\t1\t2\ta"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nU\t1\t2\ta"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\ta"),
            format!("SNAPSHOT\t{epoch}\t1\t2\nF\t1\t2\ta\nF\t1\t2\ta"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\t2\t/abs"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\t2\tC:/abs"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\t2\ta/../b"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\t2\ta/./b"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\t2\ta//b"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\t2\ta/"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\t2\ta\\b"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\t2\ta\tb"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\t2\ta\rb"),
            format!("SNAPSHOT\t{epoch}\t1\t1\nF\t1\t2\t"),
            format!("DELTA\t{epoch}\t5\t4\t0"),
            format!("DELTA\t{epoch}\t1\t2\t1\nF\t1\t2\ta"),
            format!("DELTA\t{epoch}\t1\t2\t1\nR\t.."),
            format!("DELTA\t{epoch}\t1\t2\t2\nR\ta\nU\t1\t2\ta"),
            format!("DELTA\t{epoch}\t1\t2\t3\nR\ta"),
            format!("DELTA\t{epoch}\t1\t2"),
            "MANUAL\tbell\u{7}".to_owned(),
            "GONE\tnow".to_owned(),
            "ERR".to_owned(),
            "STATUS-REPLY\t1\nP\t1\t2\t3\t-\t00".to_owned(),
            "STATUS-REPLY\t1\nP\t1\t2\t3\tsoon\t2f".to_owned(),
            "STATUS-REPLY\t1".to_owned(),
            "HELLO\t1\tbuild".to_owned(),
        ];
        for text in malformed {
            let text = format!("{text}\n");
            assert!(decode_text(&text).is_err(), "accepted {text:?}");
        }
    }

    #[test]
    fn long_or_broken_lines_are_rejected() {
        let long = format!("ERR\t{}\n", "x".repeat(MAX_LINE as usize + 10));
        let kind = |bytes: &[u8]| read_line(&mut io::Cursor::new(bytes)).unwrap_err().kind();
        assert_eq!(kind(long.as_bytes()), io::ErrorKind::InvalidData);
        assert_eq!(kind(b"GONE"), io::ErrorKind::UnexpectedEof);
        assert_eq!(kind(b"ERR\t\xff\n"), io::ErrorKind::InvalidData);
        assert_eq!(read_line(&mut io::Cursor::new(&b""[..])).unwrap(), None);
    }

    #[test]
    fn fnv1a_matches_the_reference_vectors() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
