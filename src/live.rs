//! Keeps the TUI's LuxIndex equal to the project's files while it runs.
//!
//! `Live` owns the index and is the only thing that changes it. Updates come
//! from a `watch::Link`, which is either the daemon's pipe or an engine on a
//! thread of this process, and `Live` cannot tell which. Every update becomes
//! entries in one queue, so a snapshot and a delta share one reconcile path.

use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc::{RecvTimeoutError, TryRecvError},
    time::{Duration, Instant},
};

use crate::{
    cache::{self, CacheFiles, Entry, Manifest, Plan, Seen, Stamp},
    daemon, ipc,
    lux::LuxIndex,
    scan,
    truth::{Change, Cursor, Debounce, Scope, Update},
    watch::{Cmd, Engine, EngineConfig, Link},
};

/// A project with at most this many jobs left counts as open. The rest
/// finishes while the user types.
const WARM_LIMIT: usize = 64;
const WORK_BUDGET: Duration = Duration::from_millis(25);
const SETTLE_SLICE: Duration = Duration::from_millis(15);
const SETTLE_FLUSH: Duration = Duration::from_millis(100);
const SETTLE_WAIT: Duration = Duration::from_millis(20);
const PROGRESS_EVERY: Duration = Duration::from_millis(60);
const IDLE_POLL: Duration = Duration::from_millis(100);
const MAX_TRIES: u8 = 5;
const BACKOFF: Duration = Duration::from_millis(250);
/// A file that stays unreadable is retried at this pace for as long as it is wanted.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const MANUAL_GRACE: Duration = Duration::from_secs(1);
const WATCH_BUFFER_BYTES: usize = 256 * 1024;

/// The stamp of a file that could not be read. No file can have it, so the
/// next plan that names the file reads it again.
const UNREAD: Stamp = Stamp {
    size: u64::MAX,
    modified: u64::MAX,
};

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

/// What `settle` reports to the Open screen.
pub struct Progress<'a> {
    pub mode: &'a Mode,
    pub done: usize,
    pub total: usize,
    pub current: &'a str,
}

/// What opening the project did, for the startup message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub indexed: usize,
    /// A saved index was restored.
    pub reused: bool,
    pub updated: usize,
    pub removed: usize,
    /// The open itself wrote the cache.
    pub saved: bool,
}

/// Saves are whole-index and big, so they are rare: on close, and once the
/// index has been quiet for a while after enough changes.
struct SavePolicy {
    quiet: Duration,
    min_gap: Duration,
    min_changes: usize,
    unsaved: usize,
    last_change: Instant,
    last_save: Instant,
}

impl SavePolicy {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            quiet: Duration::from_secs(10),
            min_gap: Duration::from_secs(120),
            min_changes: 20,
            unsaved: 0,
            last_change: now,
            last_save: now,
        }
    }

    fn changed(&mut self) {
        self.unsaved += 1;
        self.last_change = Instant::now();
    }

    fn due(&self) -> bool {
        self.unsaved >= self.min_changes
            && self.last_change.elapsed() >= self.quiet
            && self.last_save.elapsed() >= self.min_gap
    }
}

/// Where updates come from.
struct Feed {
    link: Link,
    via: Via,
    /// Set for an in-process engine, which this `Live` must stop.
    engine: Option<Engine>,
}

enum Step {
    Done { changed: bool },
    Retry,
}

pub struct Live {
    root: PathBuf,
    index: LuxIndex,
    files: Option<CacheFiles>,
    fingerprint: String,
    /// What the index holds.
    manifest: Manifest,
    indexed: usize,
    /// The newest engine position whose updates are all applied. Saved with the manifest.
    committed: Option<Cursor>,
    /// The newest engine position accepted into the queue.
    received: Option<Cursor>,
    /// The work queue. `Some(stamp)` makes the index match that stamp, `None`
    /// removes the file. A newer update for a key replaces the older job.
    want: BTreeMap<String, Option<Stamp>>,
    /// Keys that hit a read error: tries so far and when to try again.
    attempts: BTreeMap<String, (u8, Instant)>,
    link: Option<Link>,
    engine: Option<Engine>,
    via: Via,
    mode: Mode,
    /// A snapshot or delta arrived, so the queue describes the whole project.
    synced: bool,
    last_feed: Instant,
    save: SavePolicy,
    backoff: Duration,
    manual_grace: Duration,
    jobs: usize,
    current: String,
    summary: Summary,
}

impl Live {
    /// Restores the saved index if its pair verifies, then picks a source of
    /// updates. Returns before any file is read. `settle` or `tick` make progress.
    pub fn open(root: &Path, library: &Path) -> Result<Live, Box<dyn Error>> {
        Live::build(root, library, cache::default_dir(), |have| {
            Some(connect(root, have))
        })
    }

    fn build(
        root: &Path,
        library: &Path,
        cache_dir: Option<PathBuf>,
        feed: impl FnOnce(Option<Cursor>) -> Option<Feed>,
    ) -> Result<Live, Box<dyn Error>> {
        let mut index = LuxIndex::load(library)?;
        let files = cache_dir.map(|dir| cache::cache_files(&dir, root));
        let fingerprint = cache::fingerprint(library);
        let loaded = files
            .as_ref()
            .and_then(|files| cache::load(&mut index, files, &fingerprint));
        if loaded.is_none() && files.as_ref().is_some_and(|files| files.manifest.exists()) {
            // A failed load can leave a half restored index behind.
            index = LuxIndex::load(library)?;
        }
        let reused = loaded.is_some();
        let (manifest, mut committed) =
            loaded.map_or_else(Default::default, |loaded| (loaded.manifest, loaded.cursor));
        if manifest.values().any(|entry| entry.stamp == UNREAD) {
            // A delta never names an unread file again. Only a snapshot re-reads it.
            committed = None;
        }
        let indexed = manifest.values().filter(|entry| entry.indexed).count();
        let (link, engine, via) = match feed(committed) {
            Some(feed) => (Some(feed.link), feed.engine, feed.via),
            None => (None, None, Via::InProcess),
        };
        Ok(Live {
            root: root.to_path_buf(),
            index,
            files,
            fingerprint,
            manifest,
            indexed,
            committed,
            received: None,
            want: BTreeMap::new(),
            attempts: BTreeMap::new(),
            link,
            engine,
            via,
            mode: Mode::Connecting,
            synced: false,
            last_feed: Instant::now(),
            save: SavePolicy::new(),
            backoff: BACKOFF,
            manual_grace: MANUAL_GRACE,
            jobs: 0,
            current: String::new(),
            summary: Summary {
                reused,
                ..Summary::default()
            },
        })
    }

    pub fn index(&self) -> &LuxIndex {
        &self.index
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    pub fn indexed_files(&self) -> usize {
        self.indexed
    }

    /// Files still waiting to be read, removed, or retried.
    pub fn pending(&self) -> usize {
        self.want.len()
    }

    pub fn summary(&self) -> Summary {
        Summary {
            indexed: self.indexed,
            ..self.summary.clone()
        }
    }

    /// How long the event loop may sleep: no time when a job can run now,
    /// otherwise until the next retry is due.
    pub fn poll_wait(&self) -> Duration {
        let now = Instant::now();
        self.want
            .keys()
            .map(|key| self.attempts.get(key).map_or(now, |(_, due)| *due))
            .min()
            .map_or(IDLE_POLL, |due| {
                due.saturating_duration_since(now).min(IDLE_POLL)
            })
    }

    /// The status bar suffix.
    pub fn status(&self) -> String {
        let pending = self.pending();
        match &self.mode {
            Mode::Connecting => "… connecting".to_owned(),
            Mode::Scanning { found } => format!("scanning {found}"),
            Mode::Live { .. } if pending > 0 => format!("● live, updating {pending}"),
            Mode::Live { .. } => "● live".to_owned(),
            Mode::Manual { .. } if pending > 0 => {
                format!("○ manual (Ctrl-R), updating {pending}")
            }
            Mode::Manual { .. } => "○ manual (Ctrl-R)".to_owned(),
            Mode::Gone => "○ project gone".to_owned(),
        }
    }

    /// Blocks for the Open screen until the project is searchable: a snapshot
    /// or delta arrived and at most `WARM_LIMIT` jobs remain. `progress` runs
    /// at most every 60 ms, and `Ok(false)` from it cancels. A cancelled open
    /// leaves the index valid and unsaved.
    pub fn settle(
        &mut self,
        mut progress: impl FnMut(Progress<'_>) -> Result<bool, Box<dyn Error>>,
    ) -> Result<bool, Box<dyn Error>> {
        let first = self.jobs;
        let mut last_report: Option<Instant> = None;
        loop {
            let busy = self.poll_wait().is_zero();
            self.drain(if busy { Duration::ZERO } else { SETTLE_WAIT });
            self.walk_if_stranded()?;
            if self.mode == Mode::Gone || self.synced && self.pending() <= WARM_LIMIT {
                break;
            }
            self.work(SETTLE_SLICE);
            if last_report.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY) {
                last_report = Some(Instant::now());
                let done = self.jobs - first;
                let report = Progress {
                    mode: &self.mode,
                    done,
                    total: done + self.pending(),
                    current: &self.current,
                };
                if !progress(report)? {
                    return Ok(false);
                }
            }
        }
        self.work(SETTLE_FLUSH);
        self.commit_cursor();
        if !self.summary.reused && self.save.unsaved > 0 {
            self.summary.saved = self.save_now();
        }
        Ok(true)
    }

    /// Never blocks. Drains updates, applies about 25 ms of jobs, and saves
    /// when the policy says so.
    pub fn tick(&mut self) -> Tick {
        self.drain(Duration::ZERO);
        let changed = self.work(WORK_BUDGET);
        self.commit_cursor();
        if self.save.due() {
            self.save_now();
        }
        Tick {
            changed,
            pending: self.pending(),
        }
    }

    /// Ctrl-R. Works in every mode. With no engine left to ask, it walks the
    /// project itself.
    pub fn rescan(&mut self) {
        if self.mode == Mode::Gone {
            return;
        }
        let asked = self
            .link
            .as_ref()
            .is_some_and(|(cmd, _)| cmd.send(Cmd::Rescan).is_ok());
        if asked {
            return;
        }
        self.lose_feed();
        if let Err(error) = self.walk_project() {
            self.mode = Mode::Manual {
                reason: format!("scan failed: {error}"),
            };
        }
    }

    /// Saves what is applied. Jobs still queued are not run: they come back
    /// from the saved cursor on the next open.
    pub fn close(mut self) {
        self.commit_cursor();
        if self.save.unsaved > 0 {
            self.save_now();
        }
    }

    fn recv(&self, wait: Duration) -> Result<Option<Update>, ()> {
        let Some((_, updates)) = &self.link else {
            return Ok(None);
        };
        if wait.is_zero() {
            match updates.try_recv() {
                Ok(update) => Ok(Some(update)),
                Err(TryRecvError::Empty) => Ok(None),
                Err(TryRecvError::Disconnected) => Err(()),
            }
        } else {
            match updates.recv_timeout(wait) {
                Ok(update) => Ok(Some(update)),
                Err(RecvTimeoutError::Timeout) => Ok(None),
                Err(RecvTimeoutError::Disconnected) => Err(()),
            }
        }
    }

    /// Accepts every waiting update. With `wait`, sleeps for the first one.
    fn drain(&mut self, mut wait: Duration) {
        loop {
            match self.recv(wait) {
                Ok(Some(update)) => {
                    wait = Duration::ZERO;
                    self.accept(update);
                }
                Ok(None) => return,
                Err(()) => return self.lose_feed(),
            }
        }
    }

    fn lose_feed(&mut self) {
        self.link = None;
        if !matches!(self.mode, Mode::Gone | Mode::Manual { .. }) {
            self.mode = Mode::Manual {
                reason: "watcher stopped".into(),
            };
        }
    }

    /// Turns an update into queue entries by comparing it with the manifest.
    /// Accepting the same update twice queues nothing new.
    fn accept(&mut self, update: Update) {
        self.last_feed = Instant::now();
        match update {
            Update::Scanning { found } => {
                if !self.synced && matches!(self.mode, Mode::Connecting | Mode::Scanning { .. }) {
                    self.mode = Mode::Scanning { found };
                }
            }
            Update::Snapshot { at, files } => self.take_snapshot(&files, Some(at)),
            Update::Delta { at, changes, .. } => {
                let plan = cache::plan(&self.manifest, Seen::Changes(&changes));
                let replaced = changes
                    .iter()
                    .map(|change| match change {
                        Change::Upsert(key, _) | Change::Remove(key) => key,
                    })
                    .filter_map(|key| self.want.remove(key).map(|job| (key.clone(), job)))
                    .collect();
                self.queue(plan, replaced);
                self.received = Some(at);
                self.synced();
            }
            Update::Manual { reason } => self.mode = Mode::Manual { reason },
            Update::Gone => self.mode = Mode::Gone,
        }
    }

    fn take_snapshot(&mut self, files: &BTreeMap<String, Stamp>, at: Option<Cursor>) {
        let plan = cache::plan(&self.manifest, Seen::All(files));
        let replaced = std::mem::take(&mut self.want);
        self.queue(plan, replaced);
        self.received = at;
        if at.is_none() {
            self.committed = None;
        }
        self.synced();
    }

    fn synced(&mut self) {
        self.synced = true;
        if !matches!(self.mode, Mode::Manual { .. } | Mode::Gone) {
            self.mode = Mode::Live { via: self.via };
        }
    }

    /// `replaced` holds the jobs this update superseded, so a job that comes
    /// back unchanged keeps its retry state.
    fn queue(&mut self, plan: Plan, replaced: BTreeMap<String, Option<Stamp>>) {
        for key in plan.to_forget {
            self.record(&key, None);
        }
        let jobs = plan
            .to_read
            .into_iter()
            .map(|(key, stamp)| (key, Some(stamp)))
            .chain(plan.to_remove.into_iter().map(|key| (key, None)));
        self.want.extend(jobs);
        self.attempts.retain(|key, _| {
            self.want.contains_key(key)
                && replaced
                    .get(key)
                    .is_none_or(|before| Some(before) == self.want.get(key))
        });
    }

    fn next_job(&self) -> Option<(String, Option<Stamp>)> {
        let now = Instant::now();
        self.want
            .iter()
            .find(|(key, _)| self.attempts.get(*key).is_none_or(|(_, due)| *due <= now))
            .map(|(key, job)| (key.clone(), *job))
    }

    /// Runs jobs for about `budget`, and at least one. Returns the keys whose
    /// indexed content changed.
    fn work(&mut self, budget: Duration) -> Vec<String> {
        let start = Instant::now();
        let mut changed = Vec::new();
        while let Some((key, job)) = self.next_job() {
            let tries = self
                .attempts
                .get(&key)
                .map_or(0, |(tries, _)| *tries)
                .saturating_add(1);
            match self.run_job(&key, job, tries >= MAX_TRIES) {
                Step::Done { changed: moved } => {
                    self.want.remove(&key);
                    self.attempts.remove(&key);
                    self.jobs += 1;
                    self.save.changed();
                    match job {
                        Some(_) => self.summary.updated += 1,
                        None => self.summary.removed += 1,
                    }
                    if moved {
                        changed.push(key.clone());
                    }
                    self.current = key;
                }
                Step::Retry => {
                    let wait = (self.backoff * 2u32.pow(u32::from(tries.min(16)) - 1)).min(MAX_BACKOFF);
                    self.attempts.insert(key, (tries, Instant::now() + wait));
                }
            }
            if start.elapsed() >= budget {
                break;
            }
        }
        changed
    }

    /// One job. The stamp comes from the update and was taken before the
    /// read, so a file that changes mid-read is named again by the next delta.
    fn run_job(&mut self, key: &str, job: Option<Stamp>, last_try: bool) -> Step {
        let was_indexed = self.manifest.get(key).is_some_and(|entry| entry.indexed);
        let Some(stamp) = job else {
            self.drop_document(key, was_indexed);
            self.record(key, None);
            return Step::Done {
                changed: was_indexed,
            };
        };
        let unindexed = |stamp| {
            Some(Entry {
                stamp,
                indexed: false,
            })
        };
        match fs::read_to_string(self.root.join(key)) {
            Ok(body) => {
                let title = key.rsplit('/').next().unwrap_or(key);
                let indexed = self.index.add(key, title, &body).is_ok();
                if !indexed {
                    self.drop_document(key, was_indexed);
                }
                self.record(key, Some(Entry { stamp, indexed }));
                Step::Done {
                    changed: indexed || was_indexed,
                }
            }
            // A missing file is retried like a locked one. If it is really gone, the
            // engine's next delta removes the key and replaces this job. If it was
            // renamed away and back, the engine sees the same stamp and says nothing,
            // so dropping it here would leave it out of the index for good.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                self.drop_document(key, was_indexed);
                self.record(key, unindexed(stamp));
                Step::Done {
                    changed: was_indexed,
                }
            }
            Err(_) => {
                if last_try {
                    // Marks the file for a re-read at the next open. The old
                    // document stays searchable while the retries go on.
                    self.record(
                        key,
                        Some(Entry {
                            stamp: UNREAD,
                            indexed: was_indexed,
                        }),
                    );
                }
                Step::Retry
            }
        }
    }

    fn drop_document(&mut self, key: &str, was_indexed: bool) {
        if was_indexed {
            let _ = self.index.remove(key);
        }
    }

    fn record(&mut self, key: &str, entry: Option<Entry>) {
        let was = self.manifest.get(key).is_some_and(|entry| entry.indexed);
        let now = entry.is_some_and(|entry| entry.indexed);
        match entry {
            Some(entry) => self.manifest.insert(key.to_owned(), entry),
            None => self.manifest.remove(key),
        };
        self.indexed = self.indexed - usize::from(was) + usize::from(now);
    }

    /// The cursor only moves once the queue is empty, so a saved cursor never
    /// runs ahead of the saved manifest. A crash repeats work and never skips it.
    fn commit_cursor(&mut self) {
        if self.want.is_empty() && self.received.is_some() {
            self.committed = self.received;
        }
    }

    fn save_now(&mut self) -> bool {
        let Some(files) = &self.files else {
            return false;
        };
        let saved = cache::save(
            &self.index,
            files,
            &self.fingerprint,
            self.committed,
            &self.manifest,
        )
        .is_ok();
        self.save.last_save = Instant::now();
        if saved {
            self.save.unsaved = 0;
        }
        saved
    }

    /// With no engine to wait for, builds the snapshot itself.
    fn walk_if_stranded(&mut self) -> Result<(), Box<dyn Error>> {
        let silent = matches!(self.mode, Mode::Manual { .. })
            && self.last_feed.elapsed() >= self.manual_grace;
        if self.synced || self.mode == Mode::Gone || !(silent || self.link.is_none()) {
            return Ok(());
        }
        self.walk_project()
    }

    fn walk_project(&mut self) -> Result<(), Box<dyn Error>> {
        let observed = scan::walk(&self.root, &Scope::ROOT, |_, _| Ok(true))?;
        let files = observed.map(|observed| observed.found).unwrap_or_default();
        self.take_snapshot(&files, None);
        Ok(())
    }

    #[cfg(test)]
    pub fn detached(root: &Path, library: &Path) -> Result<Live, Box<dyn Error>> {
        Live::build(root, library, None, |_| None)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(engine) = &self.engine {
            let _ = engine.cmd.send(Cmd::Stop);
        }
    }
}

/// The daemon when one answers or starts, otherwise an engine in this process.
fn connect(root: &Path, have: Option<Cursor>) -> Feed {
    if env::var_os("RECALL_DAEMON").is_some_and(|value| value == "0") {
        return in_process(root, have);
    }
    let attached = match ipc::connect(root, have) {
        Err(ipc::ConnectError::Absent) => daemon::ensure_running()
            .ok()
            .and_then(|()| ipc::connect(root, have).ok()),
        other => other.ok(),
    };
    match attached {
        Some(link) => Feed {
            link,
            via: Via::Daemon,
            engine: None,
        },
        None => in_process(root, have),
    }
}

fn in_process(root: &Path, have: Option<Cursor>) -> Feed {
    let engine = Engine::spawn(EngineConfig {
        root: root.to_path_buf(),
        debounce: Debounce::default(),
        buffer_bytes: WATCH_BUFFER_BYTES,
    });
    Feed {
        link: engine.subscribe(have),
        via: Via::InProcess,
        engine: Some(engine),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        lux,
        truth::{Epoch, Gen},
    };
    use std::{
        process,
        sync::mpsc::{Receiver, Sender, channel},
        thread,
    };

    struct Project {
        base: PathBuf,
        root: PathBuf,
        cache: PathBuf,
    }

    impl Project {
        fn new(name: &str) -> Self {
            let base = env::temp_dir().join(format!("recall-live-{}-{name}", process::id()));
            let _ = fs::remove_dir_all(&base);
            let (root, cache) = (base.join("src"), base.join("cache"));
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(&cache).unwrap();
            Self { base, root, cache }
        }

        fn write(&self, rel: &str, body: impl AsRef<[u8]>) {
            let path = self.root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }

        fn remove(&self, rel: &str) {
            fs::remove_file(self.root.join(rel)).unwrap();
        }

        fn stamp(&self, rel: &str) -> Stamp {
            Stamp::of(&fs::metadata(self.root.join(rel)).unwrap())
        }

        fn walk(&self) -> BTreeMap<String, Stamp> {
            scan::walk(&self.root, &Scope::ROOT, |_, _| Ok(true))
                .unwrap()
                .unwrap()
                .found
        }

        fn rig(&self, cache: Option<&Path>) -> Rig {
            let (commands, command_rx) = channel();
            let (updates, update_rx) = channel();
            let live = Live::build(
                &self.root,
                &lux::library_path(),
                cache.map(Path::to_path_buf),
                move |_| {
                    Some(Feed {
                        link: (commands, update_rx),
                        via: Via::InProcess,
                        engine: None,
                    })
                },
            )
            .expect("build libluxcore");
            Rig {
                live,
                updates,
                commands: command_rx,
            }
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    struct Rig {
        live: Live,
        updates: Sender<Update>,
        commands: Receiver<Cmd>,
    }

    impl Rig {
        fn send(&self, update: Update) {
            self.updates.send(update).unwrap();
        }

        fn drive(&mut self) {
            let end = Instant::now() + Duration::from_secs(10);
            loop {
                self.live.tick();
                if self.live.pending() == 0 {
                    return;
                }
                assert!(Instant::now() < end, "queue never drained");
                thread::sleep(Duration::from_millis(1));
            }
        }

        fn settled(&mut self) {
            assert!(self.live.settle(|_| Ok(true)).unwrap());
        }
    }

    fn cursor(generation: u64) -> Cursor {
        Cursor {
            epoch: Epoch(7),
            generation: Gen(generation),
        }
    }

    fn snapshot(files: &BTreeMap<String, Stamp>, generation: u64) -> Update {
        Update::Snapshot {
            at: cursor(generation),
            files: files.clone(),
        }
    }

    fn delta(from: u64, to: u64, changes: Vec<Change>) -> Update {
        Update::Delta {
            from: Gen(from),
            at: cursor(to),
            changes,
        }
    }

    fn upsert(key: &str, stamp: Stamp) -> Change {
        Change::Upsert(key.to_owned(), stamp)
    }

    fn changes_between(
        before: &BTreeMap<String, Stamp>,
        after: &BTreeMap<String, Stamp>,
    ) -> Vec<Change> {
        let removed = before
            .keys()
            .filter(|key| !after.contains_key(*key))
            .map(|key| Change::Remove(key.clone()));
        let upserted = after
            .iter()
            .filter(|(key, stamp)| before.get(*key) != Some(stamp))
            .map(|(key, stamp)| upsert(key, *stamp));
        removed.chain(upserted).collect()
    }

    fn found(live: &Live, query: &str) -> Vec<String> {
        live.index()
            .search(query, 20)
            .unwrap()
            .into_iter()
            .map(|hit| hit.key)
            .collect()
    }

    fn tick_until(rig: &mut Rig, what: &str, done: impl Fn(&Live) -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !done(&rig.live) {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            rig.live.tick();
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn delta_and_snapshot_converge_to_the_same_manifest_and_results() {
        let project = Project::new("converge");
        project.write("a.md", "alpha first");
        project.write("b.txt", "beta second");
        project.write("sub/c.rs", "fn gamma() {}");
        let before = project.walk();

        let mut warm = project.rig(None);
        warm.send(snapshot(&before, 1));
        warm.drive();

        project.write("a.md", "alpha rewritten words");
        project.remove("b.txt");
        project.write("d.md", "delta fourth file");
        let after = project.walk();
        warm.send(delta(1, 2, changes_between(&before, &after)));
        warm.drive();

        let mut cold = project.rig(None);
        cold.send(snapshot(&after, 1));
        cold.drive();

        assert_eq!(warm.live.manifest, cold.live.manifest);
        assert_eq!(warm.live.indexed_files(), cold.live.indexed_files());
        for query in ["alpha", "beta", "gamma", "delta", "rewritten", "first"] {
            assert_eq!(
                found(&warm.live, query),
                found(&cold.live, query),
                "query {query:?}"
            );
        }
        assert!(found(&warm.live, "first").is_empty());
        assert_eq!(found(&warm.live, "rewritten"), ["a.md"]);
    }

    #[test]
    fn accepting_the_same_update_twice_queues_nothing_new() {
        let project = Project::new("twice");
        project.write("a.md", "alpha");
        project.write("b.md", "beta");
        let files = project.walk();
        let mut rig = project.rig(None);

        rig.send(snapshot(&files, 1));
        rig.live.drain(Duration::ZERO);
        let queued = rig.live.want.clone();
        assert_eq!(queued.len(), 2);

        rig.send(snapshot(&files, 1));
        rig.send(delta(0, 1, changes_between(&BTreeMap::new(), &files)));
        rig.live.drain(Duration::ZERO);
        assert_eq!(rig.live.want, queued);

        rig.drive();
        rig.send(snapshot(&files, 1));
        rig.send(delta(0, 1, changes_between(&BTreeMap::new(), &files)));
        rig.live.drain(Duration::ZERO);
        assert_eq!(rig.live.pending(), 0);
    }

    #[test]
    fn a_newer_update_for_a_key_replaces_the_queued_job() {
        let project = Project::new("newer");
        project.write("a.md", "alpha");
        let (old, new) = (
            Stamp {
                size: 1,
                modified: 1,
            },
            Stamp {
                size: 2,
                modified: 2,
            },
        );
        let mut rig = project.rig(None);
        rig.send(delta(0, 1, vec![upsert("a.md", old)]));
        rig.send(delta(1, 2, vec![upsert("a.md", new)]));
        rig.live.drain(Duration::ZERO);
        assert_eq!(
            rig.live.want,
            BTreeMap::from([("a.md".to_owned(), Some(new))])
        );

        rig.drive();
        assert_eq!(rig.live.manifest["a.md"].stamp, new);

        rig.send(delta(2, 3, vec![upsert("a.md", old)]));
        rig.send(delta(3, 4, vec![Change::Remove("a.md".into())]));
        rig.live.drain(Duration::ZERO);
        assert_eq!(
            rig.live.want,
            BTreeMap::from([("a.md".to_owned(), None)]),
            "the remove replaced the upsert"
        );
        rig.drive();
        assert!(found(&rig.live, "alpha").is_empty());
        assert!(rig.live.manifest.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_file_is_retried_then_marked_unread_not_lost() {
        use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};

        let project = Project::new("locked");
        project.write("a.md", "locked alpha");
        let files = project.walk();
        let mut rig = project.rig(None);
        rig.live.backoff = Duration::from_millis(1);
        rig.send(snapshot(&files, 1));

        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(project.root.join("a.md"))
            .unwrap();
        rig.live.tick();
        assert_eq!(
            rig.live.pending(),
            1,
            "a locked file is retried, not dropped"
        );
        assert_eq!(rig.live.attempts["a.md"].0, 1);

        let end = Instant::now() + Duration::from_secs(10);
        while rig.live.manifest.get("a.md").map(|entry| entry.stamp) != Some(UNREAD) {
            rig.live.tick();
            assert!(Instant::now() < end, "never marked unread");
            thread::sleep(Duration::from_millis(1));
        }
        let entry = rig.live.manifest["a.md"];
        assert_eq!((entry.stamp, entry.indexed), (UNREAD, false));
        assert!(found(&rig.live, "alpha").is_empty());
        assert_eq!(rig.live.pending(), 1, "it keeps being retried");

        // No new snapshot: the retry itself picks the file up once it is free.
        drop(lock);
        rig.live.backoff = Duration::from_millis(1);
        rig.live.attempts.get_mut("a.md").unwrap().1 = Instant::now();
        rig.drive();
        assert_eq!(found(&rig.live, "alpha"), ["a.md"]);
        assert!(rig.live.manifest["a.md"].indexed);
    }

    #[test]
    fn an_unread_file_makes_the_saved_cursor_unusable() {
        let project = Project::new("unread-cursor");
        project.write("a.md", "alpha");
        let files = project.walk();
        let mut rig = project.rig(Some(&project.cache));
        rig.send(snapshot(&files, 1));
        rig.drive();
        rig.live.record(
            "a.md",
            Some(Entry {
                stamp: UNREAD,
                indexed: false,
            }),
        );
        rig.live.save.unsaved = 1;
        rig.live.close();

        let reopened = project.rig(Some(&project.cache));
        assert!(reopened.live.summary.reused);
        assert_eq!(reopened.live.committed, None);
    }

    #[test]
    fn a_saved_pair_is_reused_and_a_mismatched_lux_stat_is_discarded() {
        let project = Project::new("pair");
        project.write("a.md", "alpha pair");
        let mut rig = project.rig(Some(&project.cache));
        rig.send(snapshot(&project.walk(), 1));
        rig.settled();
        assert!(rig.live.summary().saved);
        rig.live.close();

        let reopened = project.rig(Some(&project.cache));
        assert!(reopened.live.summary.reused);
        assert_eq!(reopened.live.indexed_files(), 1);
        assert_eq!(found(&reopened.live, "alpha"), ["a.md"]);
        drop(reopened);

        let files = cache::cache_files(&project.cache, &project.root);
        let mut lux = fs::OpenOptions::new()
            .append(true)
            .open(&files.index)
            .unwrap();
        io::Write::write_all(&mut lux, b"torn").unwrap();
        drop(lux);

        let discarded = project.rig(Some(&project.cache));
        assert!(!discarded.live.summary.reused);
        assert_eq!(discarded.live.indexed_files(), 0);
        assert_eq!(discarded.live.committed, None);
        assert!(found(&discarded.live, "alpha").is_empty());
    }

    #[test]
    fn a_write_behind_save_round_trips_the_cursor() {
        let project = Project::new("write-behind");
        project.write("a.md", "alpha");
        let mut rig = project.rig(Some(&project.cache));
        rig.send(snapshot(&project.walk(), 4));
        rig.settled();
        assert_eq!(rig.live.committed, Some(cursor(4)));

        project.write("b.md", "bravo behind");
        rig.send(delta(4, 5, vec![upsert("b.md", project.stamp("b.md"))]));
        rig.live.tick();
        assert_eq!(rig.live.save.unsaved, 1, "the default policy waits");
        rig.live.save.quiet = Duration::ZERO;
        rig.live.save.min_gap = Duration::ZERO;
        rig.live.save.min_changes = 1;
        rig.live.tick();
        assert_eq!(rig.live.save.unsaved, 0, "the policy saved");
        drop(rig);

        let reopened = project.rig(Some(&project.cache));
        assert_eq!(reopened.live.committed, Some(cursor(5)));
        assert!(reopened.live.manifest.contains_key("b.md"));
        assert_eq!(found(&reopened.live, "bravo"), ["b.md"]);
    }

    #[test]
    fn closing_saves_changes_the_policy_has_not_yet_saved() {
        let project = Project::new("close");
        project.write("a.md", "alpha");
        let mut rig = project.rig(Some(&project.cache));
        rig.send(snapshot(&project.walk(), 1));
        rig.settled();

        project.write("b.md", "bravo closing");
        rig.send(delta(1, 2, vec![upsert("b.md", project.stamp("b.md"))]));
        rig.drive();
        rig.live.close();

        let reopened = project.rig(Some(&project.cache));
        assert_eq!(reopened.live.committed, Some(cursor(2)));
        assert_eq!(found(&reopened.live, "bravo"), ["b.md"]);
    }

    #[test]
    fn the_saved_cursor_stays_behind_while_jobs_are_queued() {
        let project = Project::new("cursor-lag");
        project.write("a.md", "alpha");
        let mut rig = project.rig(None);
        rig.send(snapshot(&project.walk(), 1));
        rig.settled();
        assert_eq!(rig.live.committed, Some(cursor(1)));

        project.write("b.md", "b");
        rig.send(delta(
            1,
            2,
            vec![upsert(
                "b.md",
                Stamp {
                    size: 1,
                    modified: 1,
                },
            )],
        ));
        rig.live.drain(Duration::ZERO);
        rig.live.commit_cursor();
        assert_eq!(rig.live.committed, Some(cursor(1)));
        rig.drive();
        assert_eq!(rig.live.committed, Some(cursor(2)));
    }

    #[test]
    fn the_mode_follows_the_updates() {
        let project = Project::new("mode");
        project.write("a.md", "alpha");
        let mut rig = project.rig(None);
        assert_eq!(rig.live.mode(), &Mode::Connecting);

        rig.send(Update::Scanning { found: 7 });
        rig.live.tick();
        assert_eq!(rig.live.mode(), &Mode::Scanning { found: 7 });
        assert_eq!(rig.live.status(), "scanning 7");

        rig.send(snapshot(&project.walk(), 1));
        rig.live.tick();
        assert_eq!(
            rig.live.mode(),
            &Mode::Live {
                via: Via::InProcess
            }
        );
        assert_eq!(rig.live.status(), "● live");

        rig.send(Update::Gone);
        rig.live.tick();
        assert_eq!(rig.live.mode(), &Mode::Gone);
        assert_eq!(rig.live.status(), "○ project gone");
    }

    #[test]
    fn the_status_counts_pending_files_and_manual_stays_manual() {
        let project = Project::new("status");
        project.write("a.md", "alpha");
        let mut rig = project.rig(None);
        rig.send(Update::Manual {
            reason: "no watcher".into(),
        });
        rig.send(Update::Scanning { found: 1 });
        rig.send(snapshot(&project.walk(), 1));
        rig.live.drain(Duration::ZERO);
        assert_eq!(rig.live.status(), "○ manual (Ctrl-R), updating 1");
        rig.drive();
        assert_eq!(rig.live.status(), "○ manual (Ctrl-R)");

        let mut live = project.rig(None);
        live.send(snapshot(&project.walk(), 1));
        live.live.drain(Duration::ZERO);
        assert_eq!(live.live.status(), "● live, updating 1");
    }

    #[test]
    fn a_unique_token_is_found_after_its_upsert_and_gone_after_its_remove() {
        let project = Project::new("token");
        project.write("a.md", "alpha base");
        let mut rig = project.rig(None);
        rig.send(snapshot(&project.walk(), 1));
        rig.settled();

        let token = "zq7xkplumbago";
        project.write("token.md", format!("note about {token} here"));
        rig.send(delta(
            1,
            2,
            vec![upsert("token.md", project.stamp("token.md"))],
        ));
        let mut seen = Vec::new();
        tick_until(&mut rig, "the token to be searchable", |live| {
            !found(live, token).is_empty()
        });
        assert_eq!(found(&rig.live, token), ["token.md"]);
        assert_eq!(rig.live.indexed_files(), 2);
        seen.extend(rig.live.tick().changed);
        assert!(seen.is_empty(), "nothing left to change");

        let other = "kv3wrnquartzite";
        project.write("token.md", format!("now it says {other} instead"));
        rig.send(delta(
            2,
            3,
            vec![upsert("token.md", project.stamp("token.md"))],
        ));
        tick_until(&mut rig, "the rewrite", |live| {
            !found(live, other).is_empty()
        });
        assert!(found(&rig.live, token).is_empty(), "the old text is gone");

        project.remove("token.md");
        rig.send(delta(3, 4, vec![Change::Remove("token.md".into())]));
        tick_until(&mut rig, "the remove", |live| found(live, other).is_empty());
        assert_eq!(rig.live.indexed_files(), 1);
    }

    #[test]
    fn a_tick_reports_the_keys_whose_content_changed() {
        let project = Project::new("changed");
        project.write("a.md", "alpha");
        project.write("bad.txt", [0xff, 0xfe, 0x00, 0x9f]);
        let mut rig = project.rig(None);
        rig.send(snapshot(&project.walk(), 1));
        let tick = rig.live.tick();
        assert_eq!(tick.changed, ["a.md"], "an unreadable file changes nothing");
        assert_eq!(tick.pending, 0);
        assert!(!rig.live.manifest["bad.txt"].indexed);
        assert_eq!(rig.live.indexed_files(), 1);
    }

    #[test]
    fn a_vanished_unindexed_file_is_forgotten_without_a_job() {
        let project = Project::new("forget");
        project.write("c.txt", [0xff, 0xfe, 0x00, 0x9f]);
        let mut rig = project.rig(None);
        rig.send(snapshot(&project.walk(), 1));
        rig.drive();
        assert!(rig.live.manifest.contains_key("c.txt"));

        rig.send(delta(1, 2, vec![Change::Remove("c.txt".into())]));
        rig.live.drain(Duration::ZERO);
        assert_eq!(rig.live.pending(), 0);
        assert!(rig.live.manifest.is_empty());
    }

    #[test]
    fn settle_waits_for_the_first_snapshot_and_indexes_a_small_project() {
        let project = Project::new("settle");
        project.write("a.md", "alpha");
        project.write("b.md", "beta");
        let mut rig = project.rig(None);
        let files = project.walk();
        let updates = rig.updates.clone();
        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            updates.send(Update::Scanning { found: 2 }).unwrap();
            // Longer than the 60 ms progress pace, so a report lands in between.
            thread::sleep(Duration::from_millis(400));
            updates.send(snapshot(&files, 1)).unwrap();
        });
        let mut modes = Vec::new();
        rig.live
            .settle(|progress| {
                modes.push(progress.mode.clone());
                Ok(true)
            })
            .unwrap();
        sender.join().unwrap();
        assert_eq!(rig.live.indexed_files(), 2);
        assert_eq!(found(&rig.live, "alpha"), ["a.md"]);
        assert!(modes.contains(&Mode::Connecting) && modes.contains(&Mode::Scanning { found: 2 }));
    }

    #[test]
    fn a_cancelled_settle_saves_nothing() {
        let project = Project::new("cancel");
        project.write("a.md", "alpha");
        let mut rig = project.rig(Some(&project.cache));
        let finished = rig.live.settle(|_| Ok(false)).unwrap();
        assert!(!finished);
        let files = cache::cache_files(&project.cache, &project.root);
        assert!(!files.manifest.exists());
    }

    #[test]
    fn a_silent_manual_engine_leaves_one_walk_to_live() {
        let project = Project::new("manual-walk");
        project.write("a.md", "alpha");
        let mut rig = project.rig(None);
        rig.live.manual_grace = Duration::ZERO;
        rig.send(Update::Manual {
            reason: "no watcher".into(),
        });
        rig.settled();
        assert_eq!(found(&rig.live, "alpha"), ["a.md"]);
        assert!(matches!(rig.live.mode(), Mode::Manual { .. }));
        assert_eq!(rig.live.committed, None);
    }

    #[test]
    fn ctrl_r_asks_the_engine_and_walks_alone_when_it_is_gone() {
        let project = Project::new("rescan");
        project.write("a.md", "alpha");
        let mut rig = project.rig(None);
        rig.live.rescan();
        assert!(matches!(rig.commands.try_recv(), Ok(Cmd::Rescan)));

        let Rig {
            mut live,
            commands,
            updates,
        } = rig;
        drop(commands);
        drop(updates);
        live.rescan();
        assert!(matches!(live.mode(), Mode::Manual { .. }));
        live.tick();
        assert_eq!(found(&live, "alpha"), ["a.md"]);
    }

    #[test]
    fn a_dropped_feed_before_any_snapshot_still_opens_by_walking() {
        let project = Project::new("dropped");
        project.write("a.md", "alpha");
        let Rig {
            mut live,
            commands,
            updates,
        } = project.rig(None);
        drop(commands);
        drop(updates);
        assert!(live.settle(|_| Ok(true)).unwrap());
        assert_eq!(found(&live, "alpha"), ["a.md"]);
    }

    #[test]
    fn a_failed_load_never_leaves_a_stale_index_behind() {
        let project = Project::new("stale");
        project.write("a.md", "alpha");
        let mut rig = project.rig(Some(&project.cache));
        rig.send(snapshot(&project.walk(), 1));
        rig.settled();
        rig.live.close();

        let files = cache::cache_files(&project.cache, &project.root);
        fs::write(&files.manifest, "not a manifest").unwrap();
        let reopened = project.rig(Some(&project.cache));
        assert!(found(&reopened.live, "alpha").is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn live_follows_edits_through_a_real_daemon_and_the_real_watcher() {
        use crate::{daemon::host, ipc::{self, PipeName}};
        let project = Project::new("daemon-e2e");
        project.write("docs/one.md", "alpha bravo");
        project.write("docs/two.md", "charlie delta");
        let suffix = format!("live-e2e-{}", process::id());
        let name = PipeName::with_suffix(Some(&suffix));
        let daemon = {
            let config = host::Config::production(PipeName::with_suffix(Some(&suffix)));
            thread::spawn(move || host::serve(config).map_err(|error| error.to_string()))
        };
        let until = Instant::now() + Duration::from_secs(5);
        while ipc::ping(&name).is_err() {
            assert!(Instant::now() < until, "the daemon never answered");
            thread::sleep(Duration::from_millis(20));
        }
        let (root, build) = (project.root.clone(), ipc::build_id().to_owned());
        let mut live = Live::build(&root, &lux::library_path(), None, |have| {
            let link = ipc::connect_to(&name, &build, &root, have).expect("connect");
            Some(Feed { link, via: Via::Daemon, engine: None })
        })
        .expect("build libluxcore");
        assert!(live.settle(|_| Ok(true)).unwrap());
        assert_eq!(found(&live, "alpha"), ["docs/one.md"]);

        let converge = |live: &mut Live, token: &str, expect: &[&str]| {
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                live.tick();
                if found(live, token) == expect {
                    return;
                }
                assert!(Instant::now() < end, "{token} never became {expect:?}");
                thread::sleep(Duration::from_millis(20));
            }
        };
        project.write("docs/three.md", "zulu unique-token-qwerty");
        converge(&mut live, "qwerty", &["docs/three.md"]);
        project.write("docs/three.md", "zulu replaced-token-asdfgh");
        converge(&mut live, "asdfgh", &["docs/three.md"]);
        converge(&mut live, "qwerty", &[]);
        project.remove("docs/two.md");
        converge(&mut live, "charlie", &[]);
        project.write("docs/.gitignore", "three.md
");
        converge(&mut live, "asdfgh", &[]);
        assert_eq!(live.mode(), &Mode::Live { via: Via::Daemon });

        drop(live);
        ipc::stop(&name).expect("stop");
        daemon.join().unwrap().unwrap();
    }

    #[cfg(windows)]
    mod real_engine {
        use super::*;
        use crate::{
            daemon::host,
            ipc::{self, PipeName},
        };
        use std::os::windows::fs::OpenOptionsExt;

        fn open_in_process(project: &Project, cache: Option<&Path>) -> Live {
            let root = project.root.clone();
            let mut live = Live::build(
                &project.root,
                &lux::library_path(),
                cache.map(Path::to_path_buf),
                move |have| Some(in_process(&root, have)),
            )
            .expect("build libluxcore");
            live.backoff = Duration::from_millis(40);
            assert!(live.settle(|_| Ok(true)).unwrap());
            live
        }

        fn tick_until(live: &mut Live, what: &str, mut done: impl FnMut(&Live) -> bool) {
            let end = Instant::now() + Duration::from_secs(6);
            while !done(live) {
                live.tick();
                assert!(Instant::now() < end, "timed out waiting until {what}");
                thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        fn a_file_that_is_missing_at_read_time_and_comes_back_is_indexed() {
            let project = Project::new("missing-then-back");
            project.write("keep.md", "keep");
            let mut live = open_in_process(&project, None);
            project.write("x.md", "roundtriptoken");
            // The engine walks and queues the delta, but this Live does not tick yet.
            thread::sleep(Duration::from_millis(900));
            fs::rename(project.root.join("x.md"), project.root.join("x.tmp")).unwrap();
            live.tick();
            fs::rename(project.root.join("x.tmp"), project.root.join("x.md")).unwrap();
            tick_until(&mut live, "x.md is searchable", |live| {
                found(live, "roundtriptoken") == ["x.md"]
            });
        }

        #[test]
        fn a_file_locked_for_a_while_is_indexed_once_it_is_unlocked() {
            let project = Project::new("locked-a-while");
            project.write("keep.md", "keep");
            project.write("a.md", "lockedtoken");
            let lock = fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(project.root.join("a.md"))
                .unwrap();
            let mut live = open_in_process(&project, None);
            let end = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < end {
                live.tick();
                thread::sleep(Duration::from_millis(10));
            }
            assert!(found(&live, "lockedtoken").is_empty());
            assert_eq!(live.manifest["a.md"].stamp, UNREAD, "a close or crash re-reads it");
            drop(lock);
            tick_until(&mut live, "a.md is searchable after the unlock", |live| {
                found(live, "lockedtoken") == ["a.md"]
            });
        }

        #[test]
        fn reopening_against_a_warm_daemon_with_nothing_changed_still_goes_live() {
            let project = Project::new("warm-reopen");
            project.write("a.md", "alphatoken");
            let suffix = format!("warm-reopen-{}", process::id());
            let name = PipeName::with_suffix(Some(&suffix));
            let config = host::Config::production(PipeName::with_suffix(Some(&suffix)));
            let daemon = thread::spawn(move || host::serve(config).map_err(|e| e.to_string()));
            let until = Instant::now() + Duration::from_secs(5);
            while ipc::ping(&name).is_err() {
                assert!(Instant::now() < until, "the daemon never answered");
                thread::sleep(Duration::from_millis(20));
            }
            let open = |name: &PipeName| {
                let (root, build, name) =
                    (project.root.clone(), ipc::build_id().to_owned(), name.clone());
                Live::build(
                    &project.root,
                    &lux::library_path(),
                    Some(project.cache.clone()),
                    move |have| {
                        let link = ipc::connect_to(&name, &build, &root, have).expect("connect");
                        Some(Feed {
                            link,
                            via: Via::Daemon,
                            engine: None,
                        })
                    },
                )
                .expect("build libluxcore")
            };
            let mut first = open(&name);
            assert!(first.settle(|_| Ok(true)).unwrap());
            first.close();

            let mut second = open(&name);
            assert!(second.committed.is_some(), "the saved cursor was handed over");
            let begun = Instant::now();
            let settled = second
                .settle(|_| Ok(begun.elapsed() < Duration::from_secs(5)))
                .unwrap();
            assert!(settled, "settle never returned for a cursor with nothing new");
            assert_eq!(second.mode(), &Mode::Live { via: Via::Daemon });
            assert_eq!(found(&second, "alphatoken"), ["a.md"]);
            drop(second);
            ipc::stop(&name).expect("stop");
            daemon.join().unwrap().unwrap();
        }
    }
}
