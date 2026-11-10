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
    cache::{self, CacheFiles, Content, Plan, Record, Records, Seen, Stamp},
    daemon, ipc,
    lux::LuxIndex,
    scan,
    truth::{Change, Cursor, Debounce, Scope, Update},
    watch::{Cmd, Engine, EngineConfig, Link},
};

const WARM_LIMIT: usize = 64;
const WORK_BUDGET: Duration = Duration::from_millis(25);
const SETTLE_SLICE: Duration = Duration::from_millis(15);
const SETTLE_FLUSH: Duration = Duration::from_millis(100);
const SETTLE_WAIT: Duration = Duration::from_millis(20);
const PROGRESS_EVERY: Duration = Duration::from_millis(60);
const IDLE_POLL: Duration = Duration::from_millis(100);
const MAX_TRIES: u8 = 5;
const BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const MANUAL_GRACE: Duration = Duration::from_secs(1);
const WATCH_BUFFER_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Connecting,
    Scanning {
        found: usize,
    },
    Live {
        via: Via,
    },
    Manual {
        reason: String,
    },
    Gone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Daemon,
    InProcess,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    pub changed: Vec<String>,
    pub pending: usize,
}

pub struct Progress<'a> {
    pub mode: &'a Mode,
    pub done: usize,
    pub total: usize,
    pub current: &'a str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub indexed: usize,
    pub reused: bool,
    pub updated: usize,
    pub removed: usize,
    pub saved: bool,
}

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

struct Feed {
    link: Link,
    via: Via,
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
    records: Records,
    indexed: usize,
    committed: Option<Cursor>,
    received: Option<Cursor>,
    want: BTreeMap<String, Option<Stamp>>,
    attempts: BTreeMap<String, (u8, Instant)>,
    link: Option<Link>,
    engine: Option<Engine>,
    via: Via,
    mode: Mode,
    synced: bool,
    last_feed: Instant,
    save: SavePolicy,
    backoff: Duration,
    manual_grace: Duration,
    jobs: usize,
    current: String,
    summary: Summary,
    /// A saved pair existed, from the load or from our own save.
    pair_on_disk: bool,
    /// How `rescan` gets a new feed after the old one was lost.
    connector: fn(&Path, Option<Cursor>) -> Feed,
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
            index = LuxIndex::load(library)?;
        }
        let reused = loaded.is_some();
        let (records, mut committed) =
            loaded.map_or_else(Default::default, |loaded| (loaded.records, loaded.cursor));
        if records.values().any(|record| record.content == Content::Unread) {
            committed = None;
        }
        let indexed = records.values().filter(|record| record.indexed).count();
        let (link, engine, via) = match feed(committed) {
            Some(feed) => (Some(feed.link), feed.engine, feed.via),
            None => (None, None, Via::InProcess),
        };
        Ok(Live {
            root: root.to_path_buf(),
            index,
            files,
            fingerprint,
            records,
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
            pair_on_disk: reused,
            connector: connect,
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
            let walked = self.walk_if_stranded(|found, latest| {
                progress(Progress {
                    mode: &Mode::Scanning { found },
                    done: 0,
                    total: 0,
                    current: latest,
                })
            })?;
            if !walked {
                return Ok(false);
            }
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
        // The daemon stopped or crashed. This starts it again, on the user's ask.
        self.lose_feed();
        let feed = (self.connector)(&self.root, self.received);
        self.link = Some(feed.link);
        self.engine = feed.engine;
        self.via = feed.via;
        self.mode = Mode::Connecting;
        self.synced = false;
        self.last_feed = Instant::now();
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
                let plan = cache::plan(&self.records, Seen::Changes(&changes));
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
        let plan = cache::plan(&self.records, Seen::All(files));
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

    fn run_job(&mut self, key: &str, job: Option<Stamp>, last_try: bool) -> Step {
        let was_indexed = self.records.get(key).is_some_and(|record| record.indexed);
        let Some(stamp) = job else {
            self.drop_document(key, was_indexed);
            self.record(key, None);
            return Step::Done {
                changed: was_indexed,
            };
        };
        let read = |indexed| {
            Some(Record {
                content: Content::Read(stamp),
                indexed,
            })
        };
        match fs::read_to_string(self.root.join(key)) {
            Ok(body) => {
                let title = key.rsplit('/').next().unwrap_or(key);
                let indexed = self.index.add(key, title, &body).is_ok();
                if !indexed {
                    self.drop_document(key, was_indexed);
                }
                self.record(key, read(indexed));
                Step::Done {
                    changed: indexed || was_indexed,
                }
            }
            // Access denied does not clear on its own, so it is not retried.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::InvalidData | io::ErrorKind::PermissionDenied
                ) =>
            {
                self.drop_document(key, was_indexed);
                self.record(key, read(false));
                Step::Done {
                    changed: was_indexed,
                }
            }
            Err(_) => {
                if last_try {
                    self.record(
                        key,
                        Some(Record {
                            content: Content::Unread,
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

    fn record(&mut self, key: &str, record: Option<Record>) {
        let was = self.records.get(key).is_some_and(|record| record.indexed);
        let now = record.is_some_and(|record| record.indexed);
        match record {
            Some(record) => self.records.insert(key.to_owned(), record),
            None => self.records.remove(key),
        };
        self.indexed = self.indexed - usize::from(was) + usize::from(now);
    }

    fn commit_cursor(&mut self) {
        if self.want.is_empty() && self.received.is_some() {
            self.committed = self.received;
        }
    }

    fn save_now(&mut self) -> bool {
        if self.pair_on_disk && self.files.as_ref().is_some_and(|files| !files.manifest.exists()) {
            // The saved index was deleted while this window was open, as the F2
            // list promises it is rebuilt next launch, so it is not written back.
            self.files = None;
        }
        let Some(files) = &self.files else {
            return false;
        };
        let saved = cache::save(
            &self.index,
            files,
            &self.fingerprint,
            self.committed,
            &self.records,
        )
        .is_ok();
        self.save.last_save = Instant::now();
        if saved {
            self.save.unsaved = 0;
            self.pair_on_disk = true;
        }
        saved
    }

    fn walk_if_stranded(
        &mut self,
        tick: impl FnMut(usize, &str) -> Result<bool, Box<dyn Error>>,
    ) -> Result<bool, Box<dyn Error>> {
        let silent = matches!(self.mode, Mode::Manual { .. })
            && self.last_feed.elapsed() >= self.manual_grace;
        if self.synced || self.mode == Mode::Gone || !(silent || self.link.is_none()) {
            return Ok(true);
        }
        self.walk_project(tick)
    }

    fn walk_project(
        &mut self,
        tick: impl FnMut(usize, &str) -> Result<bool, Box<dyn Error>>,
    ) -> Result<bool, Box<dyn Error>> {
        let Some(observed) = scan::walk(&self.root, &Scope::ROOT, tick)? else {
            return Ok(false);
        };
        self.take_snapshot(&observed.found, None);
        Ok(true)
    }

    #[cfg(test)]
    pub fn detached(root: &Path, library: &Path) -> Result<Live, Box<dyn Error>> {
        let mut live = Live::build(root, library, None, |_| None)?;
        live.connector = in_process;
        Ok(live)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(engine) = &self.engine {
            let _ = engine.cmd.send(Cmd::Stop);
        }
    }
}

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

        assert_eq!(warm.live.records, cold.live.records);
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
        assert_eq!(rig.live.records["a.md"].content, Content::Read(new));

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
        assert!(rig.live.records.is_empty());
    }

    fn tries(live: &Live, key: &str) -> Option<u8> {
        live.attempts.get(key).map(|(tries, _)| *tries)
    }

    #[test]
    fn a_requeued_job_keeps_its_backoff_only_while_the_job_is_the_same() {
        let project = Project::new("backoff");
        let (old, new) = (Stamp { size: 1, modified: 1 }, Stamp { size: 2, modified: 2 });
        let ghost = BTreeMap::from([("ghost.md".to_owned(), old)]);
        let mut rig = project.rig(None);
        rig.live.backoff = Duration::from_secs(60);
        rig.send(snapshot(&ghost, 1));
        rig.live.tick();
        assert_eq!(tries(&rig.live, "ghost.md"), Some(1), "a missing file is retried");
        assert!(rig.live.poll_wait() > Duration::ZERO, "the retry is not due yet");

        rig.send(snapshot(&ghost, 2));
        rig.send(delta(2, 3, vec![upsert("ghost.md", old)]));
        rig.live.tick();
        assert_eq!(tries(&rig.live, "ghost.md"), Some(1), "the same job keeps its backoff");

        rig.send(delta(3, 4, vec![upsert("ghost.md", new)]));
        rig.live.drain(Duration::ZERO);
        assert_eq!(tries(&rig.live, "ghost.md"), None, "a new job starts over");
        assert_eq!(rig.live.poll_wait(), Duration::ZERO);
        rig.live.tick();
        assert_eq!(tries(&rig.live, "ghost.md"), Some(1));

        rig.send(delta(4, 5, vec![upsert("ghost.md", new), upsert("ghost.md", old), upsert("ghost.md", new)]));
        rig.live.drain(Duration::ZERO);
        assert_eq!(tries(&rig.live, "ghost.md"), Some(1), "only the last change for a key counts");

        rig.send(delta(5, 6, vec![Change::Remove("ghost.md".into())]));
        rig.live.drain(Duration::ZERO);
        assert_eq!((rig.live.pending(), tries(&rig.live, "ghost.md")), (0, None));
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
        while rig.live.records.get("a.md").map(|record| record.content) != Some(Content::Unread) {
            rig.live.tick();
            assert!(Instant::now() < end, "never marked unread");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!rig.live.records["a.md"].indexed);
        assert!(found(&rig.live, "alpha").is_empty());
        assert_eq!(rig.live.pending(), 1, "it keeps being retried");

        drop(lock);
        rig.live.backoff = Duration::from_millis(1);
        rig.live.attempts.get_mut("a.md").unwrap().1 = Instant::now();
        rig.drive();
        assert_eq!(found(&rig.live, "alpha"), ["a.md"]);
        assert!(rig.live.records["a.md"].indexed);
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
            Some(Record {
                content: Content::Unread,
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
        assert!(reopened.live.records.contains_key("b.md"));
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
        assert!(!rig.live.records["bad.txt"].indexed);
        assert_eq!(rig.live.indexed_files(), 1);
    }

    #[test]
    fn a_vanished_unindexed_file_is_forgotten_without_a_job() {
        let project = Project::new("forget");
        project.write("c.txt", [0xff, 0xfe, 0x00, 0x9f]);
        let mut rig = project.rig(None);
        rig.send(snapshot(&project.walk(), 1));
        rig.drive();
        assert!(rig.live.records.contains_key("c.txt"));

        rig.send(delta(1, 2, vec![Change::Remove("c.txt".into())]));
        rig.live.drain(Duration::ZERO);
        assert_eq!(rig.live.pending(), 0);
        assert!(rig.live.records.is_empty());
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
    fn ctrl_r_asks_the_engine_and_reconnects_when_the_feed_is_gone() {
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
        live.connector = in_process_feed;
        live.rescan();
        assert_eq!(live.mode(), &Mode::Connecting);
        let end = Instant::now() + Duration::from_secs(6);
        while found(&live, "alpha").is_empty() || live.mode() != &(Mode::Live { via: Via::InProcess }) {
            live.tick();
            assert!(Instant::now() < end, "never reconnected: {:?}", live.mode());
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn in_process_feed(root: &Path, have: Option<Cursor>) -> Feed {
        in_process(root, have)
    }

    mod lifecycle {
        use super::*;
        use std::cell::RefCell;

        thread_local! {
            static HANDED: RefCell<Option<(Sender<Update>, Receiver<Cmd>)>> =
                const { RefCell::new(None) };
        }

        fn hand_fed(_: &Path, _: Option<Cursor>) -> Feed {
            let (commands, command_rx) = channel();
            let (updates, update_rx) = channel();
            HANDED.with(|handed| *handed.borrow_mut() = Some((updates, command_rx)));
            Feed {
                link: (commands, update_rx),
                via: Via::InProcess,
                engine: None,
            }
        }

        #[derive(Clone, Copy, Debug)]
        enum Start {
            Linked,
            Unlinked,
        }

        #[derive(Clone, Copy, Debug)]
        enum Event {
            Scan(usize),
            Snapshot,
            Delta,
            Manual,
            Gone,
            /// The watcher's side of the update channel closes.
            Stop,
            /// The watcher's side of the command channel closes.
            Deaf,
            /// Live walks the folder itself.
            Walk,
            CtrlR,
        }
        use Event::*;

        struct Driver {
            _project: Project,
            live: Live,
            updates: Option<Sender<Update>>,
            commands: Option<Receiver<Cmd>>,
            asked: usize,
        }

        impl Driver {
            fn new(start: Start) -> Self {
                let project = Project::new(&format!("lifecycle-{:?}", thread::current().id()));
                let mut live = Live::build(&project.root, &lux::library_path(), None, |have| {
                    matches!(start, Start::Linked).then(|| hand_fed(&project.root, have))
                })
                .expect("build libluxcore");
                live.connector = hand_fed;
                let mut driver = Self {
                    _project: project,
                    live,
                    updates: None,
                    commands: None,
                    asked: 0,
                };
                driver.take_handed();
                driver
            }

            fn take_handed(&mut self) {
                if let Some((updates, commands)) = HANDED.with(|handed| handed.borrow_mut().take()) {
                    self.updates = Some(updates);
                    self.commands = Some(commands);
                }
            }

            fn send(&mut self, update: Update) {
                self.updates.as_ref().expect("a link").send(update).unwrap();
                self.live.tick();
            }

            fn apply(&mut self, event: Event) {
                match event {
                    Scan(found) => self.send(Update::Scanning { found }),
                    Snapshot => self.send(snapshot(&BTreeMap::new(), 1)),
                    Delta => self.send(delta(0, 1, Vec::new())),
                    Manual => self.send(Update::Manual {
                        reason: "cannot watch".into(),
                    }),
                    Gone => self.send(Update::Gone),
                    Stop => {
                        self.updates = None;
                        self.live.tick();
                    }
                    Deaf => self.commands = None,
                    Walk => assert!(self.live.settle(|_| Ok(true)).unwrap()),
                    CtrlR => {
                        self.live.rescan();
                        let commands = self.commands.as_ref();
                        if commands.is_some_and(|commands| matches!(commands.try_recv(), Ok(Cmd::Rescan))) {
                            self.asked += 1;
                        }
                        self.take_handed();
                    }
                }
            }
        }

        fn live() -> Mode {
            Mode::Live {
                via: Via::InProcess,
            }
        }

        fn manual(reason: &str) -> Mode {
            Mode::Manual {
                reason: reason.into(),
            }
        }

        fn run(start: Start, events: &[Event]) -> Driver {
            let mut driver = Driver::new(start);
            for event in events {
                driver.apply(*event);
            }
            driver
        }

        /// Rows follow the reachable states and labelled steps of the
        /// LiveFeed statechart: (mode, synced, link).
        #[test]
        fn the_feed_lifecycle_follows_the_statechart() {
            let connecting = (Mode::Connecting, "… connecting");
            let live = (live(), "● live");
            let manual_watch = (manual("cannot watch"), "○ manual (Ctrl-R)");
            let manual_stopped = (manual("watcher stopped"), "○ manual (Ctrl-R)");
            let gone = (Mode::Gone, "○ project gone");
            let rows = vec![
                ("connecting, unsynced, attached", Start::Linked, vec![], connecting.clone()),
                ("connecting, unsynced, lost", Start::Unlinked, vec![], connecting.clone()),
                (
                    "scanning, unsynced, attached",
                    Start::Linked,
                    vec![Scan(3)],
                    (Mode::Scanning { found: 3 }, "scanning 3"),
                ),
                (
                    "a later scan count replaces the first",
                    Start::Linked,
                    vec![Scan(3), Scan(9)],
                    (Mode::Scanning { found: 9 }, "scanning 9"),
                ),
                ("live, synced, attached", Start::Linked, vec![Snapshot], live.clone()),
                ("a delta syncs too", Start::Linked, vec![Delta], live.clone()),
                ("scanning then a snapshot", Start::Linked, vec![Scan(3), Snapshot], live.clone()),
                ("a scan is ignored once synced", Start::Linked, vec![Snapshot, Scan(5)], live.clone()),
                ("manual, unsynced, attached", Start::Linked, vec![Manual], manual_watch.clone()),
                ("a scan is ignored in manual", Start::Linked, vec![Manual, Scan(2)], manual_watch.clone()),
                ("manual while scanning", Start::Linked, vec![Scan(2), Manual], manual_watch.clone()),
                ("gone, unsynced, attached", Start::Linked, vec![Gone], gone.clone()),
                ("a scan is ignored when gone", Start::Linked, vec![Gone, Scan(1)], gone.clone()),
                ("a manual update leaves gone", Start::Linked, vec![Gone, Manual], manual_watch.clone()),
                ("manual, unsynced, lost", Start::Linked, vec![Stop], manual_stopped.clone()),
                ("scanning, then the watcher stops", Start::Linked, vec![Scan(3), Stop], manual_stopped.clone()),
                ("manual, synced, attached", Start::Linked, vec![Snapshot, Manual], manual_watch.clone()),
                ("a snapshot keeps manual", Start::Linked, vec![Manual, Snapshot], manual_watch.clone()),
                ("a delta keeps manual", Start::Linked, vec![Manual, Delta], manual_watch.clone()),
                ("gone, synced, attached", Start::Linked, vec![Snapshot, Gone], gone.clone()),
                ("a snapshot keeps gone", Start::Linked, vec![Gone, Snapshot], gone.clone()),
                ("manual, synced, lost", Start::Linked, vec![Snapshot, Stop], manual_stopped.clone()),
                ("a stop keeps the manual reason", Start::Linked, vec![Manual, Stop], manual_watch.clone()),
                ("an own walk keeps manual", Start::Linked, vec![Stop, Walk], manual_stopped.clone()),
                ("gone, unsynced, lost", Start::Linked, vec![Gone, Stop], gone.clone()),
                ("gone, synced, lost", Start::Linked, vec![Snapshot, Gone, Stop], gone.clone()),
                ("Ctrl-R after a stop reconnects", Start::Linked, vec![Stop, CtrlR], connecting.clone()),
                ("Ctrl-R with no link connects", Start::Unlinked, vec![CtrlR], connecting.clone()),
                (
                    "a reconnect waits for a new snapshot",
                    Start::Linked,
                    vec![Snapshot, Stop, CtrlR, Scan(4)],
                    (Mode::Scanning { found: 4 }, "scanning 4"),
                ),
                ("a reconnect goes live again", Start::Linked, vec![Snapshot, Stop, CtrlR, Snapshot], live.clone()),
                ("Ctrl-R after an own walk reconnects", Start::Linked, vec![Stop, Walk, CtrlR], connecting.clone()),
                ("Ctrl-R asks a live watcher", Start::Linked, vec![Snapshot, CtrlR], live.clone()),
                ("Ctrl-R asks a manual watcher", Start::Linked, vec![Manual, CtrlR], manual_watch.clone()),
                ("Ctrl-R reconnects a deaf watcher", Start::Linked, vec![Snapshot, Deaf, CtrlR], connecting.clone()),
                ("gone ignores Ctrl-R", Start::Linked, vec![Gone, CtrlR], gone.clone()),
                ("gone and lost ignores Ctrl-R", Start::Linked, vec![Gone, Stop, CtrlR], gone.clone()),
            ];
            for (name, start, events, (mode, status)) in rows {
                let driver = run(start, &events);
                assert_eq!((driver.live.mode(), driver.live.status().as_str()), (&mode, status), "{name}: {events:?}");
            }
        }

        #[test]
        fn ctrl_r_asks_an_attached_watcher_and_reconnects_a_lost_one() {
            for (events, asked) in [
                (vec![Snapshot, CtrlR], 1),
                (vec![Manual, CtrlR], 1),
                (vec![Snapshot, Deaf, CtrlR, CtrlR], 1),
                (vec![Stop, CtrlR, CtrlR], 1),
                (vec![Gone, CtrlR], 0),
            ] {
                assert_eq!(run(Start::Linked, &events).asked, asked, "{events:?}");
            }
        }

        /// The one combination the statechart forbids: live with no watcher.
        /// It is reachable only when Live starts with no feed and walks.
        #[test]
        fn an_own_walk_with_no_feed_goes_live() {
            let driver = run(Start::Unlinked, &[Walk]);
            assert_eq!(driver.live.mode(), &live());
            assert_eq!(driver.live.status(), "● live");
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_by_permission_is_unindexed_not_retried() {
        let project = Project::new("denied");
        project.write("ok.md", "alpha");
        // Reading a folder fails with access denied, as an ACL-denied file does.
        fs::create_dir_all(project.root.join("denied.md")).unwrap();
        let mut rig = project.rig(None);
        let stamp = Stamp { size: 1, modified: 1 };
        let mut files = project.walk();
        files.insert("denied.md".into(), stamp);
        rig.send(snapshot(&files, 1));
        rig.drive();
        assert_eq!(rig.live.pending(), 0, "it is not retried");
        let record = rig.live.records["denied.md"];
        assert_eq!((record.content, record.indexed), (Content::Read(stamp), false));
        assert_eq!(found(&rig.live, "alpha"), ["ok.md"]);
        rig.settled();
        assert_eq!(rig.live.committed, Some(cursor(1)), "the cursor still advances");
    }

    #[test]
    fn cancelling_during_lives_own_walk_stops_it() {
        let project = Project::new("cancel-walk");
        for n in 0..50 {
            project.write(&format!("d{}/f{n}.md", n % 5), "x");
        }
        let Rig {
            mut live,
            commands,
            updates,
        } = project.rig(None);
        drop(commands);
        drop(updates);
        let mut asked = 0;
        let settled = live
            .settle(|progress| {
                asked += 1;
                assert!(matches!(progress.mode, Mode::Scanning { .. }), "{:?}", progress.mode);
                Ok(false)
            })
            .unwrap();
        assert!(!settled);
        assert_eq!(asked, 1);
        assert_eq!(live.pending(), 0, "no snapshot was taken from a cancelled walk");
    }

    #[test]
    fn a_saved_index_deleted_while_open_is_not_written_back() {
        let project = Project::new("deleted-open");
        project.write("a.md", "alpha");
        let mut first = project.rig(Some(&project.cache));
        first.send(snapshot(&project.walk(), 1));
        first.settled();
        first.live.close();
        let files = cache::cache_files(&project.cache, &project.root);
        assert!(files.manifest.exists());

        let mut rig = project.rig(Some(&project.cache));
        rig.send(snapshot(&project.walk(), 1));
        rig.settled();
        fs::remove_file(&files.index).unwrap();
        fs::remove_file(&files.manifest).unwrap();
        rig.live.save.unsaved = 1;
        rig.live.close();
        assert!(!files.manifest.exists() && !files.index.exists(), "the deleted pair came back");
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
            assert_eq!(live.records["a.md"].content, Content::Unread, "a close or crash re-reads it");
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
