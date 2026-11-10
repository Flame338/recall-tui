use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    hash::BuildHasher,
    time::{Duration, Instant},
};

use crate::{cache::Stamp, scan};

/// A project-relative path, `/` separated, with no leading or trailing `/`.
/// The root is the empty path. A `Rel` built from an event names something a
/// walk could produce, so noise such as `target/` or `.git/` never becomes one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rel(String);

impl Rel {
    pub const ROOT: Rel = Rel(String::new());

    /// Parses a name reported relative to the watched root, with `\` or `/`
    /// separators. Expects long names: short 8.3 names must be expanded first.
    pub fn from_event(raw: &str) -> Option<Rel> {
        let parts: Vec<&str> = raw.split(['\\', '/']).collect();
        let (last, dirs) = parts.split_last()?;
        let admitted = dirs.iter().all(|dir| scan::enters_dir(dir))
            && (scan::enters_dir(last) || scan::is_indexable(last) || scan::is_rule_file(last));
        admitted.then(|| Rel(parts.join("/")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parent(&self) -> Rel {
        Rel(parent_of(&self.0).to_owned())
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or_default()
    }

    pub fn is_rule_file(&self) -> bool {
        scan::is_rule_file(self.file_name())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Shallow(Rel),
    Deep(Rel),
}

impl Scope {
    pub const ROOT: Scope = Scope::Deep(Rel::ROOT);

    pub fn dir(&self) -> &Rel {
        match self {
            Scope::Shallow(dir) | Scope::Deep(dir) => dir,
        }
    }

    pub fn covers(&self, key: &str) -> bool {
        match self {
            Scope::Shallow(dir) => same(parent_of(key).as_bytes(), dir.as_str().as_bytes()),
            Scope::Deep(path) => within(key, path.as_str()),
        }
    }

    pub fn enters(&self, dir: &str) -> bool {
        match self {
            Scope::Shallow(target) => within(target.as_str(), dir),
            Scope::Deep(path) => within(path.as_str(), dir) || within(dir, path.as_str()),
        }
    }
}

fn fold(path: &str) -> String {
    if cfg!(windows) {
        path.to_ascii_lowercase()
    } else {
        path.to_owned()
    }
}

fn within(key: &str, dir: &str) -> bool {
    if dir.is_empty() {
        return true;
    }
    let (key, dir) = (key.as_bytes(), dir.as_bytes());
    key.len() >= dir.len()
        && same(&key[..dir.len()], dir)
        && (key.len() == dir.len() || key[dir.len()] == b'/')
}

/// Windows paths ignore case, so a scope must match a key whatever its case.
fn same(a: &[u8], b: &[u8]) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn parent_of(key: &str) -> &str {
    key.rsplit_once('/').map_or("", |(parent, _)| parent)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    pub scope: Scope,
    pub found: BTreeMap<String, Stamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Missing,
}

pub trait Probe {
    fn kind(&self, rel: &Rel) -> Kind;
}

impl<F: Fn(&Rel) -> Kind> Probe for F {
    fn kind(&self, rel: &Rel) -> Kind {
        self(rel)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Debounce {
    pub quiet: Duration,
    pub ceiling: Duration,
    pub max_scopes: usize,
    pub max_paths: usize,
}

impl Default for Debounce {
    fn default() -> Self {
        Self {
            quiet: Duration::from_millis(200),
            ceiling: Duration::from_secs(1),
            max_scopes: 64,
            max_paths: 50_000,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Window {
    first: Instant,
    last: Instant,
}

#[derive(Debug, Default)]
pub struct Dirty {
    paths: BTreeSet<Rel>,
    all: bool,
    window: Option<Window>,
}

impl Dirty {
    pub fn note(&mut self, rel: Rel, now: Instant, limits: &Debounce) {
        self.touch(now);
        if self.all {
            return;
        }
        self.paths.insert(rel);
        if self.paths.len() > limits.max_paths {
            self.paths.clear();
            self.all = true;
        }
    }

    pub fn overflow(&mut self, now: Instant) {
        self.touch(now);
        self.paths.clear();
        self.all = true;
    }

    fn touch(&mut self, now: Instant) {
        let window = self.window.get_or_insert(Window {
            first: now,
            last: now,
        });
        window.last = now;
    }

    /// When the engine should resolve: after `quiet` with no event, or at the
    /// `ceiling` after the first event, whichever is sooner. `None` when clean.
    pub fn deadline(&self, limits: &Debounce) -> Option<Instant> {
        self.window
            .map(|window| (window.last + limits.quiet).min(window.first + limits.ceiling))
    }

    pub fn resolve(&mut self, truth: &Truth, probe: &impl Probe, limits: &Debounce) -> Vec<Scope> {
        let paths = std::mem::take(&mut self.paths);
        let all = std::mem::take(&mut self.all);
        self.window = None;
        if all {
            return vec![Scope::ROOT];
        }
        let scopes = paths.into_iter().map(|rel| {
            if rel.is_rule_file() {
                return Scope::Deep(rel.parent());
            }
            match probe.kind(&rel) {
                Kind::Dir => Scope::Deep(rel),
                _ if truth.has_under(&rel) => Scope::Deep(rel),
                Kind::File | Kind::Missing => Scope::Shallow(rel.parent()),
            }
        });
        coalesce(scopes, limits.max_scopes)
    }
}

fn coalesce(scopes: impl Iterator<Item = Scope>, max: usize) -> Vec<Scope> {
    let mut unique: BTreeMap<(bool, String), Scope> = BTreeMap::new();
    for scope in scopes {
        let key = (
            matches!(scope, Scope::Shallow(_)),
            fold(scope.dir().as_str()),
        );
        unique.entry(key).or_insert(scope);
    }
    let deep: BTreeSet<&str> = unique
        .iter()
        .filter(|((shallow, _), _)| !shallow)
        .map(|((_, dir), _)| dir.as_str())
        .collect();
    if deep.contains("") {
        return vec![Scope::ROOT];
    }
    let absorbed = |dir: &str, include_self: bool| {
        (include_self && deep.contains(dir)) || ancestors(dir).any(|above| deep.contains(above))
    };
    let kept: Vec<Scope> = unique
        .iter()
        .filter(|((shallow, dir), _)| !absorbed(dir, *shallow))
        .map(|(_, scope)| scope.clone())
        .collect();
    if kept.len() > max {
        vec![Scope::ROOT]
    } else {
        kept
    }
}

fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors((!path.is_empty()).then_some(path), |current| {
        (!current.is_empty()).then(|| parent_of(current))
    })
    .skip(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Epoch(pub u64);

impl Epoch {
    pub fn fresh() -> Epoch {
        let random = std::collections::hash_map::RandomState::new();
        Epoch(random.hash_one((std::process::id(), Instant::now())))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Gen(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cursor {
    pub epoch: Epoch,
    pub generation: Gen,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Upsert(String, Stamp),
    Remove(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    Scanning { found: usize },
    Snapshot {
        at: Cursor,
        files: BTreeMap<String, Stamp>,
    },
    /// What changed between `from` and `at`, one net change per key.
    Delta {
        from: Gen,
        at: Cursor,
        changes: Vec<Change>,
    },
    Manual { reason: String },
    Gone,
}

const LOG_CAP: usize = 100_000;

pub struct Truth {
    epoch: Epoch,
    generation: Gen,
    files: BTreeMap<String, Stamp>,
    log: VecDeque<(Gen, Change)>,
    floor: Gen,
    log_cap: usize,
}

impl Truth {
    pub fn new(epoch: Epoch) -> Truth {
        Truth::with_log_cap(epoch, LOG_CAP)
    }

    fn with_log_cap(epoch: Epoch, log_cap: usize) -> Truth {
        Truth {
            epoch,
            generation: Gen::default(),
            files: BTreeMap::new(),
            log: VecDeque::new(),
            floor: Gen::default(),
            log_cap,
        }
    }

    pub fn cursor(&self) -> Cursor {
        Cursor {
            epoch: self.epoch,
            generation: self.generation,
        }
    }

    #[cfg(test)]
    pub fn files(&self) -> &BTreeMap<String, Stamp> {
        &self.files
    }

    pub fn has_under(&self, dir: &Rel) -> bool {
        if dir.as_str().is_empty() {
            return !self.files.is_empty();
        }
        let prefix = format!("{}/", dir.as_str());
        self.files
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(key, _)| key.starts_with(&prefix))
    }

    /// Replaces everything `observed.scope` covers with `observed.found`.
    /// Returns the new generation, or `None` when nothing changed.
    pub fn apply(&mut self, observed: Observed) -> Option<Gen> {
        let mut changes: Vec<Change> = self
            .files
            .keys()
            .filter(|key| observed.scope.covers(key) && !observed.found.contains_key(*key))
            .map(|key| Change::Remove(key.clone()))
            .collect();
        changes.extend(
            observed
                .found
                .into_iter()
                .filter(|(key, stamp)| self.files.get(key) != Some(stamp))
                .map(|(key, stamp)| Change::Upsert(key, stamp)),
        );
        if changes.is_empty() {
            return None;
        }
        self.generation = Gen(self.generation.0 + 1);
        for change in changes {
            match &change {
                Change::Upsert(key, stamp) => self.files.insert(key.clone(), *stamp),
                Change::Remove(key) => self.files.remove(key),
            };
            self.log.push_back((self.generation, change));
        }
        while self.log.len() > self.log_cap {
            if let Some((generation, _)) = self.log.pop_front() {
                self.floor = generation;
            }
        }
        Some(self.generation)
    }

    /// A delta from `have`, or a snapshot when `have` is missing, from another
    /// epoch, older than the log keeps, or ahead of this truth.
    pub fn since(&self, have: Option<Cursor>) -> Update {
        let usable = have.filter(|cursor| {
            cursor.epoch == self.epoch
                && cursor.generation >= self.floor
                && cursor.generation <= self.generation
        });
        let Some(cursor) = usable else {
            return Update::Snapshot {
                at: self.cursor(),
                files: self.files.clone(),
            };
        };
        let start = self
            .log
            .partition_point(|(generation, _)| *generation <= cursor.generation);
        let mut net: BTreeMap<&str, Option<Stamp>> = BTreeMap::new();
        for (_, change) in self.log.range(start..) {
            match change {
                Change::Upsert(key, stamp) => net.insert(key, Some(*stamp)),
                Change::Remove(key) => net.insert(key, None),
            };
        }
        let changes = net
            .into_iter()
            .map(|(key, stamp)| match stamp {
                Some(stamp) => Change::Upsert(key.to_owned(), stamp),
                None => Change::Remove(key.to_owned()),
            })
            .collect();
        Update::Delta {
            from: cursor.generation,
            at: self.cursor(),
            changes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(path: &str) -> Rel {
        Rel::from_event(path).unwrap_or_else(|| panic!("{path} should be a Rel"))
    }

    fn stamp(size: u64) -> Stamp {
        Stamp { size, modified: 1 }
    }

    fn observed(scope: Scope, files: &[(&str, u64)]) -> Observed {
        Observed {
            scope,
            found: files
                .iter()
                .map(|(key, size)| ((*key).to_owned(), stamp(*size)))
                .collect(),
        }
    }

    fn files_probe(_: &Rel) -> Kind {
        Kind::File
    }

    fn resolve(events: &[&str], truth: &Truth, probe: &impl Probe) -> Vec<Scope> {
        let mut dirty = Dirty::default();
        let now = Instant::now();
        let limits = Debounce::default();
        for event in events {
            dirty.note(rel(event), now, &limits);
        }
        dirty.resolve(truth, probe, &limits)
    }

    fn burst(dirs: usize) -> Vec<String> {
        (0..5_000)
            .map(|n| format!("src\\d{}\\f{}.rs", n % dirs, n % 7))
            .collect()
    }

    #[test]
    fn five_thousand_events_in_forty_dirs_resolve_to_at_most_forty_shallow_scopes() {
        let events = burst(40);
        let events: Vec<&str> = events.iter().map(String::as_str).collect();
        let scopes = resolve(&events, &Truth::new(Epoch(1)), &files_probe);
        assert_eq!(scopes.len(), 40);
        assert!(
            scopes
                .iter()
                .all(|scope| matches!(scope, Scope::Shallow(_)))
        );
    }

    #[test]
    fn five_thousand_events_in_four_hundred_dirs_collapse_to_one_root_walk() {
        let events = burst(400);
        let events: Vec<&str> = events.iter().map(String::as_str).collect();
        let scopes = resolve(&events, &Truth::new(Epoch(1)), &files_probe);
        assert_eq!(scopes, [Scope::ROOT]);
    }

    #[test]
    fn a_deep_ancestor_absorbs_shallow_and_deep_children() {
        let probe = |rel: &Rel| match rel.as_str() {
            "src" | "src/a" => Kind::Dir,
            _ => Kind::File,
        };
        let scopes = resolve(
            &["src/a/x.rs", "src/a", "src/b/y.rs", "src", "docs/z.md"],
            &Truth::new(Epoch(1)),
            &probe,
        );
        assert_eq!(
            scopes,
            [Scope::Deep(rel("src")), Scope::Shallow(rel("docs"))]
        );
    }

    #[test]
    fn a_gitignore_event_dirties_its_whole_directory_subtree() {
        let scopes = resolve(&["a/b/.gitignore"], &Truth::new(Epoch(1)), &files_probe);
        assert_eq!(scopes, [Scope::Deep(rel("a/b"))]);
        let scopes = resolve(&[".gitignore"], &Truth::new(Epoch(1)), &files_probe);
        assert_eq!(scopes, [Scope::ROOT]);
    }

    #[test]
    fn a_vanished_folder_with_known_files_is_walked_deep_and_a_vanished_file_shallow() {
        let mut truth = Truth::new(Epoch(1));
        truth.apply(observed(Scope::ROOT, &[("old/x.md", 1), ("top.md", 1)]));
        let missing = |_: &Rel| Kind::Missing;
        assert_eq!(
            resolve(&["old", "top.md"], &truth, &missing),
            [Scope::Deep(rel("old")), Scope::Shallow(Rel::ROOT)]
        );
    }

    #[test]
    fn overflow_resolves_to_root_regardless_of_recorded_paths() {
        let mut dirty = Dirty::default();
        let limits = Debounce::default();
        let now = Instant::now();
        dirty.note(rel("a/x.md"), now, &limits);
        dirty.overflow(now);
        dirty.note(rel("b/y.md"), now, &limits);
        assert_eq!(
            dirty.resolve(&Truth::new(Epoch(1)), &files_probe, &limits),
            [Scope::ROOT]
        );
        assert_eq!(dirty.deadline(&limits), None);
    }

    #[test]
    fn too_many_distinct_paths_become_an_overflow() {
        let limits = Debounce {
            max_paths: 3,
            ..Debounce::default()
        };
        let mut dirty = Dirty::default();
        let now = Instant::now();
        for name in ["a.md", "b.md", "c.md", "d.md"] {
            dirty.note(rel(name), now, &limits);
        }
        assert_eq!(
            dirty.resolve(&Truth::new(Epoch(1)), &files_probe, &limits),
            [Scope::ROOT]
        );
    }

    #[test]
    fn the_deadline_waits_for_quiet_but_never_past_the_ceiling() {
        let limits = Debounce::default();
        let mut dirty = Dirty::default();
        assert_eq!(dirty.deadline(&limits), None);
        let start = Instant::now();
        dirty.note(rel("a.md"), start, &limits);
        assert_eq!(dirty.deadline(&limits), Some(start + limits.quiet));
        let later = start + Duration::from_millis(900);
        dirty.note(rel("b.md"), later, &limits);
        assert_eq!(dirty.deadline(&limits), Some(start + limits.ceiling));
        dirty.resolve(&Truth::new(Epoch(1)), &files_probe, &limits);
        assert_eq!(dirty.deadline(&limits), None);
    }

    #[test]
    fn noise_paths_never_construct_a_rel() {
        for noise in [
            "",
            "target\\debug\\x.rs",
            "node_modules/p/readme.md",
            ".git\\index",
            ".git",
            "src/.hidden/x.md",
            "a\\\\b.md",
            "a/../b.md",
            "a/./b.md",
            "bad\nname.md",
            "dir\u{7}/x.md",
            "build",
            ".env",
        ] {
            assert_eq!(Rel::from_event(noise), None, "{noise:?} should be noise");
        }
        for kept in [
            "src\\lib.rs",
            "src",
            "a/.gitignore",
            ".ignore",
            ".hidden.md",
            "renamed-folder",
        ] {
            assert!(Rel::from_event(kept).is_some(), "{kept:?} should be kept");
        }
        assert_eq!(rel("src\\a\\b.rs").as_str(), "src/a/b.rs");
    }

    #[test]
    fn apply_replaces_only_what_the_scope_covers_and_twice_is_a_no_op() {
        let mut truth = Truth::new(Epoch(1));
        truth.apply(observed(
            Scope::ROOT,
            &[("a/x.md", 1), ("a/b/y.md", 1), ("z.md", 1)],
        ));
        let shallow = observed(Scope::Shallow(rel("a")), &[("a/new.md", 2)]);
        assert_eq!(truth.apply(shallow.clone()), Some(Gen(2)));
        assert_eq!(truth.apply(shallow), None);
        let keys: Vec<&str> = truth.files().keys().map(String::as_str).collect();
        assert_eq!(keys, ["a/b/y.md", "a/new.md", "z.md"]);
        assert_eq!(truth.cursor().generation, Gen(2));
    }

    #[test]
    fn a_delta_carries_one_net_change_per_key_since_the_cursor() {
        let mut truth = Truth::new(Epoch(1));
        truth.apply(observed(Scope::ROOT, &[("a.md", 1), ("b.md", 1)]));
        let have = truth.cursor();
        truth.apply(observed(Scope::ROOT, &[("a.md", 2), ("c.md", 1)]));
        truth.apply(observed(Scope::ROOT, &[("a.md", 3)]));
        assert_eq!(
            truth.since(Some(have)),
            Update::Delta {
                from: Gen(1),
                at: truth.cursor(),
                changes: vec![
                    Change::Upsert("a.md".into(), stamp(3)),
                    Change::Remove("b.md".into()),
                    Change::Remove("c.md".into()),
                ],
            }
        );
    }

    fn is_snapshot(update: &Update) -> bool {
        matches!(update, Update::Snapshot { .. })
    }

    #[test]
    fn a_cursor_from_another_epoch_or_none_gets_a_snapshot() {
        let mut truth = Truth::new(Epoch(1));
        truth.apply(observed(Scope::ROOT, &[("a.md", 1)]));
        let foreign = Cursor {
            epoch: Epoch(2),
            generation: truth.cursor().generation,
        };
        assert!(is_snapshot(&truth.since(Some(foreign))));
        assert!(is_snapshot(&truth.since(None)));
        let ahead = Cursor {
            epoch: Epoch(1),
            generation: Gen(9),
        };
        assert!(is_snapshot(&truth.since(Some(ahead))));
        assert!(!is_snapshot(&truth.since(Some(truth.cursor()))));
    }

    #[test]
    fn a_cursor_below_the_log_floor_gets_a_snapshot() {
        let mut truth = Truth::with_log_cap(Epoch(1), 2);
        let start = truth.cursor();
        truth.apply(observed(Scope::ROOT, &[("a.md", 1)]));
        let after_one = truth.cursor();
        truth.apply(observed(Scope::ROOT, &[("a.md", 2), ("b.md", 1)]));
        truth.apply(observed(Scope::ROOT, &[("a.md", 3), ("b.md", 1)]));
        assert!(is_snapshot(&truth.since(Some(start))));
        assert!(is_snapshot(&truth.since(Some(after_one))));
        let Update::Snapshot { files, at } = truth.since(None) else {
            unreachable!()
        };
        assert_eq!(at, truth.cursor());
        assert_eq!(&files, truth.files());
    }

    #[test]
    fn scopes_match_keys_by_folder_boundary() {
        let deep = Scope::Deep(rel("src/a"));
        assert!(deep.covers("src/a"));
        assert!(deep.covers("src/a/b/c.rs"));
        assert!(!deep.covers("src/ab/c.rs"));
        assert!(deep.enters("src") && deep.enters("src/a/b") && !deep.enters("docs"));
        let shallow = Scope::Shallow(rel("src"));
        assert!(shallow.covers("src/x.rs") && !shallow.covers("src/a/x.rs"));
        assert!(shallow.enters("src") && !shallow.enters("src/a"));
        assert!(Scope::Shallow(Rel::ROOT).covers("top.md"));
        assert_eq!(cfg!(windows), deep.covers("SRC/A/x.rs"));
    }
}
