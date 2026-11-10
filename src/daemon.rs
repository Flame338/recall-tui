//! The background process that hosts one engine per watched project. It never
//! loads LuxCore. Clients reach it through `ipc`.
//!
//! The daemon keeps an engine armed for 12 hours after its last client left,
//! at most 16 idle ones, and exits once it hosts nothing and nobody is
//! connected. It runs from a copy of the exe under `%LOCALAPPDATA%\Recall\bin`,
//! so `cargo build` can still replace the original.

use std::{error::Error, fmt};

#[derive(Debug)]
pub struct SpawnError(pub String);

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot start the daemon: {}", self.0)
    }
}

impl Error for SpawnError {}

/// Starts the daemon unless one already answers.
pub fn ensure_running() -> Result<(), SpawnError> {
    #[cfg(windows)]
    {
        host::ensure(&crate::ipc::PipeName::from_env(), &["daemon", "run"])
    }
    #[cfg(not(windows))]
    {
        Err(SpawnError("this platform has no daemon".to_owned()))
    }
}

/// The body of the daemon process.
pub fn run() -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    {
        host::serve(host::Config::production(crate::ipc::PipeName::from_env()))
    }
    #[cfg(not(windows))]
    {
        Err("this platform has no daemon".into())
    }
}

/// `recall daemon run|start|status|stop`. `args` are the words after `daemon`.
pub fn cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["run"] => run(),
        #[cfg(windows)]
        ["start"] => {
            ensure_running()?;
            println!("the daemon is running");
            Ok(())
        }
        #[cfg(windows)]
        ["status"] => host::print_status(),
        #[cfg(windows)]
        ["stop"] => host::stop(),
        #[cfg(not(windows))]
        ["start" | "status" | "stop"] => Err("this platform has no daemon".into()),
        _ => Err("usage: recall daemon run|start|status|stop".into()),
    }
}

#[cfg(windows)]
pub(crate) mod host {
    use std::{
        collections::{HashMap, HashSet},
        env,
        error::Error,
        ffi::OsStr,
        fs, io, iter, mem,
        os::windows::ffi::OsStrExt,
        path::{Path, PathBuf},
        ptr,
        sync::{
            Arc, Mutex, MutexGuard, PoisonError,
            atomic::{AtomicBool, Ordering},
            mpsc::{self, RecvTimeoutError, Sender},
        },
        thread,
        time::{Duration, Instant},
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, STILL_ACTIVE},
        System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CreateProcessW,
            DETACHED_PROCESS, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
        },
    };

    use super::SpawnError;
    use crate::{
        ipc::{self, ConnectError, Peer, PipeName, ProjectStatus, Request, Stop, Writer},
        truth::{Change, Cursor, Debounce, Gen, Update},
        watch::{Cmd, Engine, EngineConfig},
    };

    /// What the daemon needs from an engine. The daemon stops it with `Cmd::Stop`.
    pub(crate) trait Hosted: Send {
        fn commands(&self) -> &Sender<Cmd>;
    }

    impl Hosted for Engine {
        fn commands(&self) -> &Sender<Cmd> {
            &self.cmd
        }
    }

    /// Starts an engine for a project root.
    pub(crate) type Spawn = Box<dyn Fn(&Path) -> Box<dyn Hosted> + Send + Sync>;

    pub(crate) struct Policy {
        /// An engine stays armed this long after its last client leaves.
        pub idle_ttl: Duration,
        /// Beyond this many projects the least recently used idle one stops.
        pub max_projects: usize,
        /// With no projects and no connections for this long, the daemon exits.
        pub linger: Duration,
        pub tick: Duration,
    }

    impl Default for Policy {
        fn default() -> Policy {
            Policy {
                idle_ttl: Duration::from_secs(12 * 60 * 60),
                max_projects: 16,
                linger: Duration::from_secs(60),
                tick: Duration::from_secs(1),
            }
        }
    }

    pub(crate) struct Config {
        pub pipe: PipeName,
        pub build: String,
        pub policy: Policy,
        pub spawn: Spawn,
    }

    impl Config {
        pub(crate) fn production(pipe: PipeName) -> Config {
            Config {
                pipe,
                build: ipc::build_id().to_owned(),
                policy: Policy::default(),
                spawn: Box::new(|root| {
                    Box::new(Engine::spawn(EngineConfig {
                        root: root.to_owned(),
                        debounce: Debounce::default(),
                        buffer_bytes: 256 * 1024,
                    }))
                }),
            }
        }
    }

    /// What the daemon learned about a project from its own subscription.
    #[derive(Default)]
    struct Seen {
        generation: Gen,
        /// Key hashes, enough to count files without a second copy of every key.
        files: HashSet<u64>,
        ended: bool,
    }

    impl Seen {
        fn observe(&mut self, update: &Update) {
            match update {
                Update::Snapshot { at, files } => {
                    self.generation = at.generation;
                    self.files = files.keys().map(|key| ipc::fnv1a(key.as_bytes())).collect();
                }
                Update::Delta { at, changes, .. } => {
                    self.generation = at.generation;
                    for change in changes {
                        match change {
                            Change::Upsert(key, _) => {
                                self.files.insert(ipc::fnv1a(key.as_bytes()));
                            }
                            Change::Remove(key) => {
                                self.files.remove(&ipc::fnv1a(key.as_bytes()));
                            }
                        }
                    }
                }
                Update::Gone => self.ended = true,
                Update::Scanning { .. } | Update::Manual { .. } => {}
            }
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    struct Project {
        /// Tells this engine apart from a later one for the same root.
        id: u64,
        root: PathBuf,
        engine: Box<dyn Hosted>,
        clients: usize,
        last_used: Instant,
        seen: Arc<Mutex<Seen>>,
    }

    impl Project {
        fn start(id: u64, root: &Path, spawn: &Spawn, now: Instant) -> Project {
            let engine = spawn(root);
            let seen = Arc::new(Mutex::new(Seen::default()));
            let (tx, rx) = mpsc::channel();
            let _ = engine.commands().send(Cmd::Subscribe { have: None, tx });
            let watcher = Arc::clone(&seen);
            let _ = thread::Builder::new()
                .name("recall-daemon-seen".to_owned())
                .spawn(move || {
                    for update in rx {
                        lock(&watcher).observe(&update);
                    }
                    lock(&watcher).ended = true;
                });
            Project {
                id,
                root: root.to_owned(),
                engine,
                clients: 0,
                last_used: now,
                seen,
            }
        }

        fn retire(self) {
            let _ = self.engine.commands().send(Cmd::Stop);
        }

        fn ended(&self) -> bool {
            lock(&self.seen).ended
        }
    }

    /// Windows paths ignore case, so two spellings of a root share one engine.
    fn project_key(root: &Path) -> String {
        root.to_string_lossy().to_ascii_lowercase()
    }

    struct Registry {
        projects: HashMap<String, Project>,
        peers: HashMap<u64, Writer>,
        /// When `projects` or `peers` last changed. Drives the exit after `linger`.
        changed: Instant,
        next_id: u64,
    }

    impl Registry {
        fn subscribe(
            &mut self,
            root: &Path,
            have: Option<Cursor>,
            tx: Sender<Update>,
            config: &Config,
            now: Instant,
        ) -> (String, u64) {
            let key = project_key(root);
            if self.projects.get(&key).is_none_or(Project::ended) {
                if let Some(old) = self.projects.remove(&key) {
                    old.retire();
                }
                if self.projects.len() >= config.policy.max_projects {
                    self.evict_least_recent();
                }
                self.next_id += 1;
                let project = Project::start(self.next_id, root, &config.spawn, now);
                self.projects.insert(key.clone(), project);
            }
            let project = self.projects.get_mut(&key).expect("inserted above");
            project.clients += 1;
            project.last_used = now;
            let _ = project.engine.commands().send(Cmd::Subscribe { have, tx });
            self.changed = now;
            (key, project.id)
        }

        /// Stops the idle project used longest ago. Projects with clients stay,
        /// even past the cap.
        fn evict_least_recent(&mut self) {
            let oldest = self
                .projects
                .iter()
                .filter(|(_, project)| project.clients == 0)
                .min_by_key(|(_, project)| project.last_used)
                .map(|(key, _)| key.clone());
            if let Some(project) = oldest.and_then(|key| self.projects.remove(&key)) {
                project.retire();
            }
        }

        fn project(&mut self, key: &str, id: u64) -> Option<&mut Project> {
            self.projects
                .get_mut(key)
                .filter(|project| project.id == id)
        }

        fn leave(&mut self, key: &str, id: u64, now: Instant) {
            if let Some(project) = self.project(key, id) {
                project.clients = project.clients.saturating_sub(1);
                project.last_used = now;
            }
        }

        fn rescan(&mut self, key: &str, id: u64) {
            if let Some(project) = self.project(key, id) {
                let _ = project.engine.commands().send(Cmd::Rescan);
            }
        }

        fn expire(&mut self, now: Instant, ttl: Duration) {
            let expired: Vec<String> = self
                .projects
                .iter()
                .filter(|(_, project)| {
                    project.clients == 0
                        && (now.duration_since(project.last_used) >= ttl || project.ended())
                })
                .map(|(key, _)| key.clone())
                .collect();
            for key in expired {
                if let Some(project) = self.projects.remove(&key) {
                    project.retire();
                    self.changed = now;
                }
            }
        }

        fn quiet_for(&self, now: Instant) -> Option<Duration> {
            (self.projects.is_empty() && self.peers.is_empty())
                .then(|| now.duration_since(self.changed))
        }

        fn status(&self, now: Instant) -> Vec<ProjectStatus> {
            let mut projects: Vec<ProjectStatus> = self
                .projects
                .values()
                .map(|project| {
                    let seen = lock(&project.seen);
                    ProjectStatus {
                        root: project.root.clone(),
                        generation: seen.generation,
                        files: seen.files.len(),
                        clients: project.clients,
                        idle: (project.clients == 0).then(|| now.duration_since(project.last_used)),
                    }
                })
                .collect();
            projects.sort_by(|a, b| a.root.cmp(&b.root));
            projects
        }
    }

    struct Shared {
        config: Config,
        stop: Stop,
        registry: Mutex<Registry>,
    }

    impl Shared {
        fn registry(&self) -> MutexGuard<'_, Registry> {
            lock(&self.registry)
        }

        fn tick(&self, now: Instant) {
            let mut registry = self.registry();
            registry.expire(now, self.config.policy.idle_ttl);
            if registry
                .quiet_for(now)
                .is_some_and(|quiet| quiet >= self.config.policy.linger)
            {
                self.stop.signal();
            }
        }
    }

    /// Serves until `STOP` or until idle. Returns at once when another daemon
    /// holds the pipe.
    pub(crate) fn serve(config: Config) -> Result<(), Box<dyn Error>> {
        let Some(mut listener) = ipc::listen(&config.pipe)? else {
            return Ok(());
        };
        let shared = Arc::new(Shared {
            config,
            stop: Stop::new()?,
            registry: Mutex::new(Registry {
                projects: HashMap::new(),
                peers: HashMap::new(),
                changed: Instant::now(),
                next_id: 0,
            }),
        });
        let timer = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("recall-daemon-timer".to_owned())
                .spawn(move || {
                    while !shared.stop.wait(shared.config.policy.tick) {
                        shared.tick(Instant::now());
                    }
                })?
        };
        let mut next_peer = 0;
        let served = loop {
            let peer = match listener.accept(&shared.stop) {
                Ok(Some(peer)) => peer,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            };
            next_peer += 1;
            let id = next_peer;
            {
                let mut registry = shared.registry();
                registry.peers.insert(id, peer.writer());
                registry.changed = Instant::now();
            }
            let connection = Arc::clone(&shared);
            let spawned = thread::Builder::new()
                .name("recall-daemon-peer".to_owned())
                .spawn(move || serve_peer(&connection, peer, id));
            if spawned.is_err() {
                shared.registry().peers.remove(&id);
            }
        };
        shared.stop.signal();
        let _ = timer.join();
        let mut registry = shared.registry();
        for (_, project) in registry.projects.drain() {
            project.retire();
        }
        for (_, peer) in registry.peers.drain() {
            peer.hang_up();
        }
        Ok(served?)
    }

    struct Joined {
        key: String,
        project: u64,
        done: Arc<AtomicBool>,
    }

    fn serve_peer(shared: &Shared, mut peer: Peer, id: u64) {
        let writer = peer.writer();
        let mut joined: Option<Joined> = None;
        if peer.handshake(&shared.config.build).is_ok() {
            loop {
                let request = match peer.next() {
                    Ok(Some(request)) => request,
                    Ok(None) => break,
                    Err(error) => {
                        writer.error(&error.to_string());
                        break;
                    }
                };
                match request {
                    Request::Subscribe { root, have } => {
                        if joined.is_some() {
                            writer.error("this connection already has a subscription");
                            break;
                        }
                        let (tx, rx) = mpsc::channel();
                        let (key, project) = shared.registry().subscribe(
                            &root,
                            have,
                            tx,
                            &shared.config,
                            Instant::now(),
                        );
                        let done = Arc::new(AtomicBool::new(false));
                        forward(rx, writer.clone(), Arc::clone(&done));
                        joined = Some(Joined { key, project, done });
                    }
                    Request::Rescan => match &joined {
                        Some(joined) => shared.registry().rescan(&joined.key, joined.project),
                        None => writer.error("RESCAN needs a subscription"),
                    },
                    Request::Status => {
                        let projects = shared.registry().status(Instant::now());
                        if writer.status(&projects).is_err() {
                            break;
                        }
                    }
                    Request::Stop => {
                        shared.stop.signal();
                        break;
                    }
                    Request::Bye => break,
                }
            }
        }
        writer.hang_up();
        let now = Instant::now();
        let mut registry = shared.registry();
        if let Some(joined) = joined {
            joined.done.store(true, Ordering::Relaxed);
            registry.leave(&joined.key, joined.project, now);
        }
        registry.peers.remove(&id);
        registry.changed = now;
    }

    /// Copies one subscription's updates to its client on a thread of its own,
    /// so a client that stops reading stalls nobody else.
    fn forward(rx: mpsc::Receiver<Update>, writer: Writer, done: Arc<AtomicBool>) {
        let _ = thread::Builder::new()
            .name("recall-daemon-send".to_owned())
            .spawn(move || {
                loop {
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(update) => {
                            if writer.update(&update).is_err() {
                                break;
                            }
                        }
                        // The engine keeps its sender until its next send fails, so the
                        // end of the connection has to be noticed here.
                        Err(RecvTimeoutError::Timeout) if !done.load(Ordering::Relaxed) => {}
                        Err(_) => break,
                    }
                }
                writer.hang_up();
            });
    }

    pub(crate) fn ensure(pipe: &PipeName, args: &[&str]) -> Result<(), SpawnError> {
        if answers(pipe) {
            return Ok(());
        }
        let exe = env::current_exe()
            .map_err(|error| SpawnError(format!("cannot find this exe: {error}")))?;
        let bin = env::var_os("LOCALAPPDATA")
            .map(|local| PathBuf::from(local).join("Recall").join("bin"))
            .ok_or_else(|| SpawnError("LOCALAPPDATA is not set".to_owned()))?;
        let copy = install(&exe, &bin, ipc::build_id()).map_err(|error| {
            SpawnError(format!("cannot copy the exe to {}: {error}", bin.display()))
        })?;
        sweep(&bin, &copy);
        let child = spawn_detached(&copy, args, &bin)
            .map_err(|error| SpawnError(format!("cannot run {}: {error}", copy.display())))?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if answers(pipe) {
                return Ok(());
            }
            // A child that already exited will never open the pipe, so every
            // launch would otherwise wait out the full timeout for nothing.
            if let Some(code) = child.exit_code() {
                return Err(SpawnError(format!(
                    "{} exited with code {code} before it answered",
                    copy.display()
                )));
            }
            thread::sleep(Duration::from_millis(25));
        }
        Err(SpawnError(format!(
            "{} did not answer within 3 s",
            copy.display()
        )))
    }

    fn answers(pipe: &PipeName) -> bool {
        matches!(
            ipc::ping(pipe),
            Ok(_) | Err(ConnectError::OtherBuild { .. })
        )
    }

    /// Copies `exe` to `bin\recall-<build>.exe` unless that copy exists. The
    /// copy keeps the mtime, so it computes the same build id.
    fn install(exe: &Path, bin: &Path, build: &str) -> io::Result<PathBuf> {
        let copy = bin.join(format!("recall-{build}.exe"));
        let source = fs::metadata(exe)?;
        let current =
            |copy: &Path| fs::metadata(copy).is_ok_and(|metadata| metadata.len() == source.len());
        if current(&copy) {
            return Ok(copy);
        }
        fs::create_dir_all(bin)?;
        let partial = bin.join(format!("recall-{build}.{}.partial", std::process::id()));
        let made = (|| {
            fs::copy(exe, &partial)?;
            fs::File::options()
                .write(true)
                .open(&partial)?
                .set_modified(source.modified()?)?;
            fs::rename(&partial, &copy)
        })();
        let _ = fs::remove_file(&partial);
        // A racing client may have installed the same copy first.
        match made {
            Err(error) if !current(&copy) => Err(error),
            _ => Ok(copy),
        }
    }

    /// Deletes older copies. A copy that still runs cannot be deleted and stays.
    fn sweep(bin: &Path, keep: &Path) {
        let Ok(entries) = fs::read_dir(bin) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with("recall-") && name.ends_with(".exe") && path != keep {
                let _ = fs::remove_file(path);
            }
        }
    }

    /// The process handle of a daemon just started. It lets `ensure` see a child
    /// that died instead of waiting for it.
    struct Child(HANDLE);

    impl Child {
        fn exit_code(&self) -> Option<u32> {
            let mut code = 0;
            let alive = unsafe { GetExitCodeProcess(self.0, &mut code) } != 0;
            (alive && code != STILL_ACTIVE as u32).then_some(code)
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn spawn_detached(exe: &Path, args: &[&str], dir: &Path) -> io::Result<Child> {
        let wide = |text: &OsStr| text.encode_wide().chain([0]).collect::<Vec<u16>>();
        let mut line = Vec::new();
        for part in iter::once(exe.as_os_str()).chain(args.iter().map(OsStr::new)) {
            if !line.is_empty() {
                line.push(u16::from(b' '));
            }
            line.push(u16::from(b'"'));
            line.extend(part.encode_wide());
            line.push(u16::from(b'"'));
        }
        line.push(0);
        let (exe, dir) = (wide(exe.as_os_str()), wide(dir.as_os_str()));
        let mut startup: STARTUPINFOW = unsafe { mem::zeroed() };
        startup.cb = mem::size_of::<STARTUPINFOW>() as u32;
        let flags = DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        let mut error = io::Error::other("no attempt made");
        // Breaking away from the caller's job keeps a terminal that kills its
        // job on close from taking the daemon along. Some jobs forbid it.
        for flags in [flags | CREATE_BREAKAWAY_FROM_JOB, flags] {
            let mut command_line = line.clone();
            let mut info: PROCESS_INFORMATION = unsafe { mem::zeroed() };
            // No inherited handles: the daemon must not hold the caller's pipes
            // open, such as a stdout another process reads to its end. The
            // working directory is the bin folder, so no project folder stays open.
            let made = unsafe {
                CreateProcessW(
                    exe.as_ptr(),
                    command_line.as_mut_ptr(),
                    ptr::null(),
                    ptr::null(),
                    0,
                    flags,
                    ptr::null(),
                    dir.as_ptr(),
                    &startup,
                    &mut info,
                )
            };
            if made != 0 {
                unsafe { CloseHandle(info.hThread) };
                return Ok(Child(info.hProcess));
            }
            error = io::Error::last_os_error();
        }
        Err(error)
    }

    pub(crate) fn print_status() -> Result<(), Box<dyn Error>> {
        let (daemon, projects) = match ipc::status(&PipeName::from_env()) {
            Ok(status) => status,
            Err(ConnectError::Absent) => {
                println!("no daemon is running");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let skew = if daemon.build == ipc::build_id() {
            ""
        } else {
            ", another build: this recall watches in-process"
        };
        println!("daemon pid {} build {}{skew}", daemon.pid, daemon.build);
        if projects.is_empty() {
            println!("no projects armed");
        }
        for project in projects {
            let idle = project
                .idle
                .map_or("active".to_owned(), |idle| format!("idle {}", span(idle)));
            println!(
                "{}\tgeneration {}\t{} files\t{} clients\t{idle}",
                project.root.display(),
                project.generation.0,
                project.files,
                project.clients
            );
        }
        Ok(())
    }

    fn span(duration: Duration) -> String {
        let seconds = duration.as_secs();
        match seconds {
            0..60 => format!("{seconds}s"),
            60..3600 => format!("{}m", seconds / 60),
            _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
        }
    }

    pub(crate) fn stop() -> Result<(), Box<dyn Error>> {
        match ipc::stop(&PipeName::from_env()) {
            Ok(daemon) => println!("stopped daemon pid {}", daemon.pid),
            Err(ConnectError::Absent) => println!("no daemon is running"),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use std::{
            collections::BTreeMap, os::windows::process::CommandExt, process::Command,
            sync::atomic::AtomicUsize, thread::JoinHandle,
        };

        use super::*;
        use crate::{cache::Stamp, truth::Epoch};

        const WAIT: Duration = Duration::from_secs(5);
        const BUILD: &str = "test-build";

        #[derive(Default)]
        struct Log {
            spawned: AtomicUsize,
            stopped: AtomicUsize,
        }

        struct Fake(Sender<Cmd>);

        impl Hosted for Fake {
            fn commands(&self) -> &Sender<Cmd> {
                &self.0
            }
        }

        fn stamp() -> Stamp {
            Stamp {
                size: 1,
                modified: 2,
            }
        }

        /// An engine that hands out a snapshot of `files(root)` files and adds
        /// one file per rescan.
        fn fake(log: &Arc<Log>, files: fn(&Path) -> usize) -> Spawn {
            let log = Arc::clone(log);
            Box::new(move |root| {
                log.spawned.fetch_add(1, Ordering::SeqCst);
                let (tx, rx) = mpsc::channel::<Cmd>();
                let log = Arc::clone(&log);
                let mut known: BTreeMap<String, Stamp> = (0..files(root))
                    .map(|n| (format!("dir/file{n}.txt"), stamp()))
                    .collect();
                thread::spawn(move || {
                    let epoch = Epoch(7);
                    let mut generation = Gen(1);
                    let mut subscribers: Vec<Sender<Update>> = Vec::new();
                    for cmd in rx {
                        match cmd {
                            Cmd::Subscribe { have, tx } => {
                                let at = Cursor { epoch, generation };
                                let first = if have == Some(at) {
                                    Update::Delta {
                                        from: generation,
                                        at,
                                        changes: Vec::new(),
                                    }
                                } else {
                                    Update::Snapshot {
                                        at,
                                        files: known.clone(),
                                    }
                                };
                                if tx.send(first).is_ok() {
                                    subscribers.push(tx);
                                }
                            }
                            Cmd::Rescan => {
                                let from = generation;
                                generation.0 += 1;
                                let key = format!("rescan{}.txt", generation.0);
                                known.insert(key.clone(), stamp());
                                let delta = Update::Delta {
                                    from,
                                    at: Cursor { epoch, generation },
                                    changes: vec![Change::Upsert(key, stamp())],
                                };
                                subscribers.retain(|tx| tx.send(delta.clone()).is_ok());
                            }
                            Cmd::Stop => break,
                        }
                    }
                    log.stopped.fetch_add(1, Ordering::SeqCst);
                });
                Box::new(Fake(tx))
            })
        }

        fn three(_: &Path) -> usize {
            3
        }

        fn quick() -> Policy {
            Policy {
                idle_ttl: Duration::from_secs(3600),
                max_projects: 16,
                linger: Duration::from_secs(3600),
                tick: Duration::from_millis(20),
            }
        }

        fn pipe(test: &str) -> PipeName {
            PipeName::with_suffix(Some(&format!("test-{}-{test}", std::process::id())))
        }

        fn root(name: &str) -> PathBuf {
            env::temp_dir().join(format!("recall-daemon-test-{name}"))
        }

        fn start(config: Config) -> JoinHandle<Result<(), String>> {
            let pipe = config.pipe.clone();
            let daemon = thread::spawn(move || serve(config).map_err(|error| error.to_string()));
            wait_until("the daemon answers", || ipc::ping(&pipe).is_ok());
            daemon
        }

        fn config(pipe: &PipeName, log: &Arc<Log>, policy: Policy) -> Config {
            Config {
                pipe: pipe.clone(),
                build: BUILD.to_owned(),
                policy,
                spawn: fake(log, three),
            }
        }

        fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
            let deadline = Instant::now() + WAIT;
            while !done() {
                assert!(Instant::now() < deadline, "timed out waiting until {what}");
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn projects(pipe: &PipeName) -> Vec<ProjectStatus> {
            ipc::status(pipe).expect("status").1
        }

        fn clients(pipe: &PipeName) -> Vec<usize> {
            projects(pipe)
                .iter()
                .map(|project| project.clients)
                .collect()
        }

        fn stop_and_join(pipe: &PipeName, daemon: JoinHandle<Result<(), String>>) {
            ipc::stop(pipe).expect("stop");
            wait_until("the daemon thread ends", || daemon.is_finished());
            daemon.join().unwrap().expect("serve returns Ok");
        }

        fn next(link: &crate::watch::Link) -> Update {
            link.1.recv_timeout(WAIT).expect("an update")
        }

        fn snapshot_len(update: Update) -> usize {
            match update {
                Update::Snapshot { files, .. } => files.len(),
                other => panic!("expected a snapshot, got {other:?}"),
            }
        }

        #[test]
        fn two_clients_on_one_root_share_one_engine() {
            let (pipe, log) = (pipe("share"), Arc::new(Log::default()));
            let daemon = start(config(&pipe, &log, quick()));
            let a = ipc::connect_to(&pipe, BUILD, &root("share"), None).unwrap();
            let b = ipc::connect_to(&pipe, BUILD, &root("SHARE"), None).unwrap();
            assert_eq!(snapshot_len(next(&a)), 3);
            assert_eq!(snapshot_len(next(&b)), 3);
            assert_eq!(
                log.spawned.load(Ordering::SeqCst),
                1,
                "one engine per root, whatever its case"
            );
            let status = projects(&pipe);
            assert_eq!(status.len(), 1);
            assert_eq!(
                (status[0].clients, status[0].files, status[0].generation),
                (2, 3, Gen(1))
            );
            assert_eq!(status[0].idle, None);

            a.0.send(Cmd::Rescan).unwrap();
            for link in [&a, &b] {
                let Update::Delta { from, at, changes } = next(link) else {
                    panic!("expected a delta");
                };
                assert_eq!((from, at.generation, changes.len()), (Gen(1), Gen(2), 1));
            }
            wait_until("status sees the rescan", || projects(&pipe)[0].files == 4);

            drop(a);
            wait_until("the dropped link unsubscribes", || clients(&pipe) == [1]);
            drop(b);
            wait_until("both links unsubscribed", || clients(&pipe) == [0]);
            assert!(projects(&pipe)[0].idle.is_some());
            assert_eq!(
                log.stopped.load(Ordering::SeqCst),
                0,
                "an idle engine stays armed"
            );
            stop_and_join(&pipe, daemon);
            wait_until("STOP stops the engines", || {
                log.stopped.load(Ordering::SeqCst) == 1
            });
        }

        #[test]
        fn a_client_with_a_cursor_resumes_with_a_delta() {
            let (pipe, log) = (pipe("cursor"), Arc::new(Log::default()));
            let daemon = start(config(&pipe, &log, quick()));
            let first = ipc::connect_to(&pipe, BUILD, &root("cursor"), None).unwrap();
            let Update::Snapshot { at, .. } = next(&first) else {
                panic!("expected a snapshot");
            };
            drop(first);
            let again = ipc::connect_to(&pipe, BUILD, &root("cursor"), Some(at)).unwrap();
            assert_eq!(
                next(&again),
                Update::Delta {
                    from: at.generation,
                    at,
                    changes: Vec::new()
                }
            );
            stop_and_join(&pipe, daemon);
        }

        #[test]
        fn a_second_daemon_on_the_same_pipe_exits_without_serving() {
            let (pipe, log) = (pipe("second"), Arc::new(Log::default()));
            let daemon = start(config(&pipe, &log, quick()));
            let second_log = Arc::new(Log::default());
            let begun = Instant::now();
            serve(config(&pipe, &second_log, quick())).expect("the loser exits cleanly");
            assert!(begun.elapsed() < Duration::from_secs(1));
            assert_eq!(second_log.spawned.load(Ordering::SeqCst), 0);
            let link = ipc::connect_to(&pipe, BUILD, &root("second"), None).unwrap();
            assert_eq!(
                snapshot_len(next(&link)),
                3,
                "the first daemon still serves"
            );
            assert_eq!(log.spawned.load(Ordering::SeqCst), 1);
            stop_and_join(&pipe, daemon);
        }

        #[test]
        fn a_client_that_connects_and_drops_does_not_kill_the_daemon() {
            let (pipe, log) = (pipe("drop"), Arc::new(Log::default()));
            let daemon = start(config(&pipe, &log, quick()));
            let end = Instant::now() + Duration::from_millis(1500);
            let mut opened = 0;
            while Instant::now() < end {
                if fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(pipe.path())
                    .is_ok()
                {
                    opened += 1;
                }
            }
            assert!(opened > 0, "no connection was made");
            assert!(!daemon.is_finished(), "the daemon exited: {:?}", daemon);
            let link = ipc::connect_to(&pipe, BUILD, &root("drop"), None).unwrap();
            assert_eq!(snapshot_len(next(&link)), 3, "it still serves");
            ipc::stop(&pipe).expect("stop");
            daemon.join().unwrap().expect("serve returns Ok");
        }

        #[test]
        fn stop_ends_the_daemon() {
            let (pipe, log) = (pipe("stop"), Arc::new(Log::default()));
            let daemon = start(config(&pipe, &log, quick()));
            let link = ipc::connect_to(&pipe, BUILD, &root("stop"), None).unwrap();
            next(&link);
            let answered = ipc::stop(&pipe).expect("stop");
            assert_eq!(answered.pid, std::process::id());
            wait_until("the daemon thread ends", || daemon.is_finished());
            daemon.join().unwrap().expect("serve returns Ok");
            assert!(matches!(ipc::ping(&pipe), Err(ConnectError::Absent)));
            assert!(
                link.1.recv_timeout(WAIT).is_err(),
                "a connected client sees the end of its stream"
            );
            assert_eq!(log.stopped.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn another_build_is_refused_and_no_daemon_is_absent() {
            let (pipe, log) = (pipe("build"), Arc::new(Log::default()));
            let mut other = config(&pipe, &log, quick());
            other.build = "someone-else".to_owned();
            let daemon = start(other);
            match ipc::connect_to(&pipe, BUILD, &root("build"), None) {
                Err(ConnectError::OtherBuild { daemon }) => assert_eq!(daemon, "someone-else"),
                other => panic!("expected OtherBuild, got {:?}", other.map(|_| ())),
            }
            assert_eq!(log.spawned.load(Ordering::SeqCst), 0);
            stop_and_join(&pipe, daemon);
            assert!(matches!(
                ipc::connect_to(&pipe, BUILD, &root("build"), None),
                Err(ConnectError::Absent)
            ));
        }

        #[test]
        fn a_client_that_stops_reading_stalls_nobody_else() {
            fn big_or_small(root: &Path) -> usize {
                if root.ends_with("recall-daemon-test-big") {
                    200_000
                } else {
                    3
                }
            }
            let (pipe, log) = (pipe("stall"), Arc::new(Log::default()));
            let mut config = config(&pipe, &log, quick());
            config.spawn = fake(&log, big_or_small);
            let daemon = start(config);

            let stalled = ipc::stalled_subscriber(&pipe, BUILD, &root("big")).unwrap();
            wait_until("the stalled client subscribed", || clients(&pipe) == [1]);

            let small = ipc::connect_to(&pipe, BUILD, &root("small"), None).unwrap();
            assert_eq!(
                snapshot_len(next(&small)),
                3,
                "another project still streams"
            );
            let big = ipc::connect_to(&pipe, BUILD, &root("big"), None).unwrap();
            assert_eq!(
                snapshot_len(next(&big)),
                200_000,
                "the same project still streams"
            );
            assert_eq!(log.spawned.load(Ordering::SeqCst), 2);

            drop(stalled);
            wait_until("the stalled client is gone", || {
                projects(&pipe)
                    .iter()
                    .find(|project| project.root.ends_with("recall-daemon-test-big"))
                    .is_some_and(|project| project.clients == 1)
            });
            big.0.send(Cmd::Rescan).unwrap();
            assert!(matches!(next(&big), Update::Delta { .. }));
            stop_and_join(&pipe, daemon);
        }

        #[test]
        fn idle_engines_expire_and_then_the_daemon_exits() {
            let (pipe, log) = (pipe("idle"), Arc::new(Log::default()));
            let policy = Policy {
                idle_ttl: Duration::from_millis(200),
                linger: Duration::from_millis(300),
                ..quick()
            };
            let daemon = start(config(&pipe, &log, policy));
            let link = ipc::connect_to(&pipe, BUILD, &root("idle"), None).unwrap();
            next(&link);
            thread::sleep(Duration::from_millis(400));
            assert_eq!(
                log.stopped.load(Ordering::SeqCst),
                0,
                "a project with a client stays"
            );
            drop(link);
            wait_until("the idle engine stops", || {
                log.stopped.load(Ordering::SeqCst) == 1
            });
            wait_until("the daemon exits on its own", || daemon.is_finished());
            daemon.join().unwrap().expect("serve returns Ok");
            assert!(matches!(ipc::ping(&pipe), Err(ConnectError::Absent)));
        }

        #[test]
        fn past_the_cap_the_least_recently_used_idle_project_stops() {
            let (pipe, log) = (pipe("cap"), Arc::new(Log::default()));
            let policy = Policy {
                max_projects: 2,
                ..quick()
            };
            let daemon = start(config(&pipe, &log, policy));
            let a = ipc::connect_to(&pipe, BUILD, &root("a"), None).unwrap();
            next(&a);
            let b = ipc::connect_to(&pipe, BUILD, &root("b"), None).unwrap();
            next(&b);
            drop(b);
            wait_until("b is idle", || clients(&pipe).contains(&0));
            drop(a);
            wait_until("a is idle", || clients(&pipe) == [0, 0]);
            let busy = ipc::connect_to(&pipe, BUILD, &root("b"), None).unwrap();
            next(&busy);
            let c = ipc::connect_to(&pipe, BUILD, &root("c"), None).unwrap();
            next(&c);
            let roots: Vec<PathBuf> = projects(&pipe)
                .into_iter()
                .map(|project| project.root)
                .collect();
            assert_eq!(roots, [root("b"), root("c")], "a was used least recently");
            assert_eq!(log.stopped.load(Ordering::SeqCst), 1);
            let d = ipc::connect_to(&pipe, BUILD, &root("d"), None).unwrap();
            next(&d);
            assert_eq!(
                projects(&pipe).len(),
                3,
                "projects with clients stay past the cap"
            );
            stop_and_join(&pipe, daemon);
        }

        const CHILD: &str = "RECALL_DAEMON_TEST_CHILD";

        /// Runs in a child process: plays the TUI that starts the daemon.
        #[test]
        #[ignore = "a helper process for ensure_running_starts_a_detached_copy"]
        fn ensure_from_a_child_process() {
            if env::var(CHILD).as_deref() != Ok("1") {
                return;
            }
            ensure(
                &PipeName::from_env(),
                &[
                    "daemon::host::tests::daemon_in_a_child_process",
                    "--exact",
                    "--ignored",
                    "--quiet",
                ],
            )
            .unwrap();
        }

        /// Runs in a child process: starts a "daemon" that exits at once.
        #[test]
        #[ignore = "a helper process for ensure_running_gives_up_when_the_daemon_exits"]
        fn ensure_with_a_daemon_that_exits() {
            if env::var(CHILD).as_deref() != Ok("2") {
                return;
            }
            let begun = Instant::now();
            let error = ensure(&PipeName::from_env(), &["no_test_has_this_name"]).unwrap_err();
            assert!(error.0.contains("exited with code"), "{error:?}");
            assert!(
                begun.elapsed() < Duration::from_millis(2500),
                "waited {:?} for a child that was already gone",
                begun.elapsed()
            );
        }

        #[test]
        fn ensure_running_gives_up_when_the_daemon_exits() {
            let suffix = format!("test-{}-exits", std::process::id());
            let local = env::temp_dir().join(format!("recall-exits-{}", std::process::id()));
            let _ = fs::remove_dir_all(&local);
            fs::create_dir_all(&local).unwrap();
            let helper = Command::new(env::current_exe().unwrap())
                .args([
                    "daemon::host::tests::ensure_with_a_daemon_that_exits",
                    "--exact",
                    "--ignored",
                    "--quiet",
                ])
                .env("RECALL_PIPE_SUFFIX", &suffix)
                .env("LOCALAPPDATA", &local)
                .env(CHILD, "2")
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .unwrap();
            let _ = fs::remove_dir_all(&local);
            assert!(
                helper.status.success(),
                "{}",
                String::from_utf8_lossy(&helper.stdout)
            );
        }

        /// Runs in the detached copy: the daemon process itself.
        #[test]
        #[ignore = "a helper process for ensure_running_starts_a_detached_copy"]
        fn daemon_in_a_child_process() {
            if env::var(CHILD).as_deref() != Ok("1") {
                return;
            }
            crate::daemon::run().unwrap();
        }

        #[test]
        fn ensure_running_starts_a_detached_copy() {
            use windows_sys::Win32::{
                Foundation::WAIT_OBJECT_0,
                System::Threading::{
                    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW,
                    TerminateProcess, WaitForSingleObject,
                },
            };

            let suffix = format!("test-{}-ensure", std::process::id());
            let pipe = PipeName::with_suffix(Some(&suffix));
            let local = env::temp_dir().join(format!("recall-ensure-{}", std::process::id()));
            let bin = local.join("Recall").join("bin");
            let _ = fs::remove_dir_all(&local);
            fs::create_dir_all(&bin).unwrap();
            let stale = bin.join("recall-0.0.0-0000000000000000.exe");
            fs::write(&stale, b"an older build").unwrap();

            let begun = Instant::now();
            let helper = Command::new(env::current_exe().unwrap())
                .args([
                    "daemon::host::tests::ensure_from_a_child_process",
                    "--exact",
                    "--ignored",
                    "--quiet",
                ])
                .env("RECALL_PIPE_SUFFIX", &suffix)
                .env("LOCALAPPDATA", &local)
                .env(CHILD, "1")
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .unwrap();
            assert!(
                helper.status.success(),
                "ensure_running fails in the helper: {}",
                String::from_utf8_lossy(&helper.stdout)
            );
            assert!(
                begun.elapsed() < Duration::from_secs(10),
                "the helper's captured stdout closed, so the daemon inherited none of its handles"
            );

            let daemon = ipc::ping(&pipe)
                .expect("the daemon outlives the process and console that started it");
            let process = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                    0,
                    daemon.pid,
                )
            };
            assert!(!process.is_null());
            let mut image = vec![0u16; 1024];
            let mut len = image.len() as u32;
            let named = unsafe {
                QueryFullProcessImageNameW(
                    process,
                    PROCESS_NAME_WIN32,
                    image.as_mut_ptr(),
                    &mut len,
                )
            };
            let image = PathBuf::from(String::from_utf16_lossy(&image[..len as usize]));

            let stopped = ipc::stop(&pipe);
            let exited = unsafe { WaitForSingleObject(process, 5000) } == WAIT_OBJECT_0;
            if !exited {
                unsafe { TerminateProcess(process, 1) };
            }
            unsafe { CloseHandle(process) };

            assert!(named != 0);
            let copy = bin.join(format!("recall-{}.exe", ipc::build_id()));
            assert_eq!(
                image.file_name().map(|name| name.to_ascii_lowercase()),
                copy.file_name().map(|name| name.to_ascii_lowercase()),
                "the daemon runs the copy, not the original exe"
            );
            assert_eq!(
                daemon.build,
                ipc::build_id(),
                "the copy computes the same build id"
            );
            assert_ne!(daemon.pid, std::process::id());
            assert!(copy.exists());
            assert!(!stale.exists(), "older copies are swept");
            stopped.expect("stop");
            assert!(exited, "STOP ends the daemon process");
            // The exited copy can stay locked for a moment, by a virus scan for one.
            for _ in 0..50 {
                if fs::remove_dir_all(&local).is_ok() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
