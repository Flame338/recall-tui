use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs::{self, File},
    io::{self, BufRead, BufReader},
    iter,
    num::NonZero,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder, Glob},
};

use crate::{
    cache::Stamp,
    truth::{Kind, Observed, Probe, Rel, Scope},
};

const INDEX_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx", "rs", "md", "txt", "rst", "py", "go", "java",
    "js", "jsx", "ts", "tsx", "json", "toml", "yaml", "yml", "ini", "sh", "bat", "ps1", "html",
    "css", "sql", "cmake",
];
pub const MAX_FILE_BYTES: u64 = 512 * 1024;
// The daemon walks in the background while the user works, so the walk takes
// at most half the logical CPUs. Past 4 threads, listing folders waits on the
// file system: on C:\ 8 threads cut the walk 4.6 s to 3.6 s for 30% more CPU.
const MAX_WALK_THREADS: usize = 4;
const TICK_EVERY: Duration = Duration::from_millis(15);

/// Lists the indexable files under `root` that `scope` covers, honouring
/// `.gitignore`, `.ignore`, `.git/info/exclude`, the global git excludes file
/// and the rule files of `root`'s parents. `tick` runs on the calling thread
/// about every 15 ms with the count so far; `false` cancels the walk.
pub fn walk(
    root: &Path,
    scope: &Scope,
    tick: impl FnMut(usize, &str) -> Result<bool, Box<dyn Error>>,
) -> Result<Option<Observed>, Box<dyn Error>> {
    let cpus = thread::available_parallelism().map_or(1, NonZero::get);
    walk_with(root, scope, (cpus / 2).clamp(1, MAX_WALK_THREADS), tick)
}

pub fn walk_with(
    root: &Path,
    scope: &Scope,
    threads: usize,
    mut tick: impl FnMut(usize, &str) -> Result<bool, Box<dyn Error>>,
) -> Result<Option<Observed>, Box<dyn Error>> {
    let outside = Outside::of(root);
    let queue = Queue::new(Dir {
        path: root.to_path_buf(),
        key: String::new(),
        rules: None,
    });
    let (sender, inbox) = mpsc::channel();
    let collected = thread::scope(|threads_scope| {
        for _ in 0..threads.max(1) {
            let sender = sender.clone();
            let (queue, outside) = (&queue, &outside);
            threads_scope.spawn(move || {
                let walker = Walker {
                    root,
                    scope,
                    outside,
                };
                walker.run(queue, &sender);
            });
        }
        drop(sender);
        let collected = collect(&inbox, &mut tick);
        queue.stop();
        collected
    })?;
    Ok(collected.map(|found| Observed {
        scope: scope.clone(),
        found,
    }))
}

type Batch = Result<Vec<(String, Stamp)>, io::Error>;

fn collect(
    inbox: &mpsc::Receiver<Batch>,
    tick: &mut impl FnMut(usize, &str) -> Result<bool, Box<dyn Error>>,
) -> Result<Option<BTreeMap<String, Stamp>>, Box<dyn Error>> {
    let mut found = BTreeMap::new();
    let mut latest = String::new();
    loop {
        if !tick(found.len(), &latest)? {
            return Ok(None);
        }
        let due = Instant::now() + TICK_EVERY;
        loop {
            match inbox.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(Ok(batch)) => {
                    if let Some((key, _)) = batch.last() {
                        latest.clone_from(key);
                    }
                    found.extend(batch);
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(Some(found)),
            }
        }
    }
}

struct Dir {
    path: PathBuf,
    key: String,
    rules: Option<Arc<Level>>,
}

/// Rule files of one folder inside the root, linked to the nearest enclosing
/// folder that has any. Folders without rule files add no level.
struct Level {
    rules: Rules,
    parent: Option<Arc<Level>>,
}

struct Rules {
    ignore: Gitignore,
    git: Gitignore,
    exclude: Gitignore,
}

impl Rules {
    /// `listed` says whether the folder may hold an entry by that name. The
    /// file is then opened by its lower-case name, so a case-insensitive disk
    /// finds `.GITIGNORE` the way git does.
    fn read(dir: &Path, listed: impl Fn(&str) -> bool) -> Rules {
        let load = |name: &str| {
            if listed(name) {
                compile(dir, &dir.join(name))
            } else {
                Gitignore::empty()
            }
        };
        let exclude = match listed(".git").then(|| git_dir(dir)).flatten() {
            Some(git) => compile(dir, &git.join("info/exclude")),
            None => Gitignore::empty(),
        };
        Rules {
            ignore: load(".ignore"),
            git: load(".gitignore"),
            exclude,
        }
    }

    fn is_empty(&self) -> bool {
        self.ignore.is_empty() && self.git.is_empty() && self.exclude.is_empty()
    }
}

fn compile(dir: &Path, file: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(dir);
    builder.add(file);
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// The folder holding `info/exclude`: `.git` itself, or for a worktree whose
/// `.git` is a file, the common dir its `gitdir:` line leads to.
fn git_dir(dir: &Path) -> Option<PathBuf> {
    let git = dir.join(".git");
    if !git.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Some(git);
    }
    let first_line = |path: &Path| BufReader::new(File::open(path).ok()?).lines().next()?.ok();
    let real = PathBuf::from(first_line(&git)?.strip_prefix("gitdir: ")?);
    let common = first_line(&real.join("commondir"))?;
    Some(if common.starts_with('.') {
        real.join(common)
    } else {
        PathBuf::from(common)
    })
}

/// Rules that come from outside the root: the root's parent folders,
/// nearest first, and the global git excludes file.
struct Outside {
    canonical: PathBuf,
    parents: Vec<Rules>,
    global: Gitignore,
}

impl Outside {
    fn of(root: &Path) -> Outside {
        let canonical = root.canonicalize().ok();
        let parents = canonical
            .iter()
            .flat_map(|path| path.ancestors().skip(1))
            .map(|dir| Rules::read(dir, |_| true))
            .filter(|rules| !rules.is_empty())
            .collect();
        let global = env::current_dir().map_or_else(
            |_| Gitignore::empty(),
            |cwd| GitignoreBuilder::new(cwd).build_global().0,
        );
        Outside {
            canonical: canonical.unwrap_or_default(),
            parents,
            global,
        }
    }
}

struct Walker<'a> {
    root: &'a Path,
    scope: &'a Scope,
    outside: &'a Outside,
}

impl Walker<'_> {
    fn run(&self, queue: &Queue, sender: &mpsc::Sender<Batch>) {
        // A thread that dies mid-folder never finishes it, so without this the
        // others would wait for it forever instead of letting the panic surface.
        struct StopOnPanic<'a>(&'a Queue);
        impl Drop for StopOnPanic<'_> {
            fn drop(&mut self) {
                if thread::panicking() {
                    self.0.stop();
                }
            }
        }
        let _guard = StopOnPanic(queue);
        while let Some(dir) = queue.next() {
            let mut subdirs = Vec::new();
            let mut found = Vec::new();
            let listed = self.list(&dir, &mut subdirs, &mut found);
            queue.finish(subdirs);
            let sent = match listed {
                Err(error) => {
                    queue.stop();
                    sender.send(Err(error))
                }
                Ok(()) if found.is_empty() => Ok(()),
                Ok(()) => sender.send(Ok(found)),
            };
            if sent.is_err() {
                queue.stop();
            }
        }
    }

    fn list(
        &self,
        dir: &Dir,
        subdirs: &mut Vec<Dir>,
        found: &mut Vec<(String, Stamp)>,
    ) -> Result<(), io::Error> {
        let mut wanted = Vec::new();
        let mut dotted = Vec::new();
        for entry in self.check(fs::read_dir(&dir.path))?.into_iter().flatten() {
            let Some(entry) = self.check(entry)? else {
                continue;
            };
            let Some(kind) = self.check(entry.file_type())? else {
                continue;
            };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') {
                dotted.push(name.clone());
            }
            let admitted = if kind.is_dir() {
                enters_dir(&name)
            } else {
                kind.is_file() && is_indexable(&name)
            };
            if admitted {
                wanted.push((entry, kind, name));
            }
        }
        let rules = Rules::read(&dir.path, |rule| {
            dotted
                .iter()
                .any(|name| name.to_uppercase() == rule.to_uppercase())
        });
        let rules = if rules.is_empty() {
            dir.rules.clone()
        } else {
            Some(Arc::new(Level {
                rules,
                parent: dir.rules.clone(),
            }))
        };
        for (entry, kind, name) in wanted {
            let key = if dir.key.is_empty() {
                name.to_owned()
            } else {
                format!("{}/{name}", dir.key)
            };
            let path = entry.path();
            if kind.is_dir() {
                if self.scope.enters(&key) && !self.ignored(rules.as_deref(), &path, true) {
                    subdirs.push(Dir {
                        path,
                        key,
                        rules: rules.clone(),
                    });
                }
                continue;
            }
            if !self.scope.covers(&key) || self.ignored(rules.as_deref(), &path, false) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() <= MAX_FILE_BYTES {
                found.push((key, Stamp::of(&metadata)));
            }
        }
        Ok(())
    }

    fn check<T>(&self, result: io::Result<T>) -> io::Result<Option<T>> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error) if walk_error_is_fatal(&error, self.root) => Err(error),
            Err(_) => Ok(None),
        }
    }

    /// The nearest rule wins within each source, and `.ignore` beats
    /// `.gitignore`, which beats `.git/info/exclude`, which beats the global
    /// excludes file.
    fn ignored(&self, level: Option<&Level>, path: &Path, is_dir: bool) -> bool {
        let mut outside_path = None;
        let mut nearest = |source: fn(&Rules) -> &Gitignore| -> Match<&Glob> {
            let levels = iter::successors(level, |level| level.parent.as_deref());
            let inside = levels
                .map(|level| source(&level.rules).matched(path, is_dir))
                .find(|found| !found.is_none());
            if let Some(found) = inside {
                return found;
            }
            if self.outside.parents.is_empty() {
                return Match::None;
            }
            let path = outside_path.get_or_insert_with(|| {
                let relative = path.strip_prefix(self.root).unwrap_or(path);
                self.outside.canonical.join(relative)
            });
            self.outside
                .parents
                .iter()
                .map(|rules| source(rules).matched(&*path, is_dir))
                .find(|found| !found.is_none())
                .unwrap_or(Match::None)
        };
        let sources: [fn(&Rules) -> &Gitignore; 3] = [
            |rules| &rules.ignore,
            |rules| &rules.git,
            |rules| &rules.exclude,
        ];
        sources
            .into_iter()
            .map(&mut nearest)
            .find(|found| !found.is_none())
            .unwrap_or_else(|| self.outside.global.matched(path, is_dir))
            .is_ignore()
    }
}

/// Folders waiting to be listed, shared by the walk's threads. The walk is
/// over when nothing is waiting and no thread is listing.
struct Queue {
    state: Mutex<Waiting>,
    wake: Condvar,
}

struct Waiting {
    dirs: Vec<Dir>,
    listing: usize,
    stopped: bool,
}

impl Queue {
    fn new(root: Dir) -> Queue {
        Queue {
            state: Mutex::new(Waiting {
                dirs: vec![root],
                listing: 0,
                stopped: false,
            }),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Waiting> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next(&self) -> Option<Dir> {
        let mut state = self.lock();
        loop {
            if state.stopped {
                return None;
            }
            if let Some(dir) = state.dirs.pop() {
                state.listing += 1;
                return Some(dir);
            }
            if state.listing == 0 {
                return None;
            }
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn finish(&self, subdirs: Vec<Dir>) {
        let mut state = self.lock();
        state.listing -= 1;
        let woken = !subdirs.is_empty() || state.listing == 0;
        state.dirs.extend(subdirs);
        drop(state);
        if woken {
            self.wake.notify_all();
        }
    }

    fn stop(&self) {
        self.lock().stopped = true;
        self.wake.notify_all();
    }
}

// Failing the walk keeps files under an unreadable folder in the index. A permission
// error never clears on its own, so it counts as absent instead of stalling every walk.
fn walk_error_is_fatal(error: &io::Error, root: &Path) -> bool {
    match error.kind() {
        io::ErrorKind::PermissionDenied => false,
        io::ErrorKind::NotFound => !root.exists(),
        _ => true,
    }
}

pub struct Disk<'a>(pub &'a Path);

impl Probe for Disk<'_> {
    fn kind(&self, rel: &Rel) -> Kind {
        match fs::symlink_metadata(self.0.join(rel.as_str())) {
            Ok(metadata) if metadata.is_dir() => Kind::Dir,
            Ok(_) => Kind::File,
            Err(_) => Kind::Missing,
        }
    }
}

pub fn enters_dir(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !has_control(name)
        && !matches!(
            name,
            "build" | "target" | "node_modules" | "vendor" | "venv" | "__pycache__"
        )
}

pub fn is_indexable(name: &str) -> bool {
    if has_control(name) {
        return false;
    }
    if matches!(
        name,
        "Makefile" | "GNUmakefile" | "Dockerfile" | "Justfile" | ".gitignore" | "CMakeLists.txt"
    ) {
        return true;
    }
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            INDEX_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

pub fn is_rule_file(name: &str) -> bool {
    // NTFS opens `.GITIGNORE` when the walk asks for `.gitignore`.
    let case = |rule: &str| {
        if cfg!(windows) {
            name.eq_ignore_ascii_case(rule)
        } else {
            name == rule
        }
    };
    case(".gitignore") || case(".ignore")
}

fn has_control(name: &str) -> bool {
    name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::truth::{Debounce, Dirty, Epoch, Truth};
    use std::{collections::BTreeSet, env, path::PathBuf, process, time::Instant};

    fn full_walk(root: &Path) -> BTreeMap<String, Stamp> {
        let found = walk(root, &Scope::ROOT, |_, _| Ok(true))
            .unwrap()
            .unwrap()
            .found;
        let old = crate::bench::reference_walk(root).unwrap();
        assert_eq!(found, old, "ORACLE");
        found
    }

    struct Project(PathBuf);

    impl Project {
        fn new(name: &str) -> Self {
            let dir = env::temp_dir().join(format!("recall-scan-{}-{name}", process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn scan(&self) -> Vec<String> {
            full_walk(&self.0).into_keys().collect()
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn gitignored_files_and_folders_are_not_scanned_even_outside_a_git_repo() {
        let project = Project::new("gitignore");
        project.write(".gitignore", "generated/\nsecret.md\n");
        project.write("src/lib.rs", "fn main() {}");
        project.write("src/secret.md", "hidden by pattern at any depth");
        project.write("generated/api.md", "ignored folder");
        project.write("docs/readme.md", "kept");
        assert_eq!(
            project.scan(),
            [".gitignore", "docs/readme.md", "src/lib.rs"]
        );
    }

    #[test]
    fn a_nested_gitignore_only_covers_its_own_folder() {
        let project = Project::new("nested");
        project.write("a/.gitignore", "local.md\n");
        project.write("a/local.md", "ignored here");
        project.write("a/keep.md", "kept");
        project.write("b/local.md", "kept because the rule is in a/");
        assert_eq!(project.scan(), ["a/.gitignore", "a/keep.md", "b/local.md"]);
    }

    #[test]
    fn hidden_folders_build_output_big_files_and_other_extensions_are_skipped() {
        let project = Project::new("rules");
        project.write(".claude/worktrees/x/notes.md", "hidden folder");
        project.write("target/out.md", "build output");
        project.write("node_modules/p/readme.md", "package");
        project.write("image.bin", "not text");
        project.write("big.md", &"x".repeat(600 * 1024));
        project.write("Makefile", "all:");
        project.write("ok.md", "indexed");
        assert_eq!(project.scan(), ["Makefile", "ok.md"]);
    }

    #[test]
    fn a_missing_root_fails_the_walk_instead_of_looking_empty() {
        let project = Project::new("missing-root");
        let gone = project.0.join("gone");
        assert!(walk(&gone, &Scope::ROOT, |_, _| Ok(true)).is_err());
    }

    #[test]
    fn a_tick_that_returns_false_cancels_the_walk() {
        let project = Project::new("cancel");
        project.write("a.md", "a");
        project.write("b.md", "b");
        let outcome = walk(&project.0, &Scope::ROOT, |_, _| Ok(false)).unwrap();
        assert!(outcome.is_none());
    }

    #[test]
    fn an_error_from_tick_fails_the_walk() {
        let project = Project::new("tick-error");
        project.write("a.md", "a");
        assert!(walk(&project.0, &Scope::ROOT, |_, _| Err("stop".into())).is_err());
    }

    #[test]
    fn a_walk_on_many_threads_finds_exactly_what_the_rules_allow() {
        let project = Project::new("wide");
        project.write(".gitignore", "d1*/\n*.log.md\n");
        let mut expected = vec![".gitignore".to_owned()];
        for n in 0..40 {
            project.write(&format!("d{n:02}/x.md"), "x");
            project.write(&format!("d{n:02}/x.log.md"), "log");
            project.write(&format!("d{n:02}/deep/er/y.rs"), "y");
            if !(10..20).contains(&n) {
                expected.push(format!("d{n:02}/deep/er/y.rs"));
                expected.push(format!("d{n:02}/x.md"));
            }
        }
        expected.sort();
        let mut counts = Vec::new();
        let found = walk_with(&project.0, &Scope::ROOT, 8, |count, _| {
            counts.push(count);
            Ok(true)
        })
        .unwrap()
        .unwrap()
        .found;
        assert_eq!(found.into_keys().collect::<Vec<_>>(), expected);
        assert!(counts.is_sorted(), "{counts:?}");
    }

    #[test]
    fn ignore_files_beat_gitignore_files() {
        let project = Project::new("precedence");
        project.write(".gitignore", "*.md\n");
        project.write(".ignore", "!keep.md\n");
        project.write("keep.md", "kept by .ignore");
        project.write("drop.md", "dropped by .gitignore");
        assert_eq!(project.scan(), [".gitignore", "keep.md"]);
    }

    #[test]
    fn git_info_exclude_applies() {
        let project = Project::new("exclude");
        project.write(".git/info/exclude", "secret.md\n");
        project.write("secret.md", "excluded");
        project.write("keep.md", "kept");
        assert_eq!(project.scan(), ["keep.md"]);
    }

    #[test]
    fn a_worktree_applies_the_exclude_file_of_its_common_dir() {
        let project = Project::new("worktree");
        let common = project.0.join("repo.git");
        let gitdir = common.join("worktrees/wt");
        project.write("repo.git/info/exclude", "secret.md\n");
        project.write("repo.git/worktrees/wt/commondir", "../..\n");
        project.write("wt/.git", &format!("gitdir: {}\n", gitdir.display()));
        project.write("wt/secret.md", "excluded");
        project.write("wt/keep.md", "kept");
        let found: Vec<String> = full_walk(&project.0.join("wt")).into_keys().collect();
        assert_eq!(found, ["keep.md"]);
    }

    #[cfg(windows)]
    #[test]
    fn a_rule_file_named_in_another_case_applies() {
        let project = Project::new("case");
        project.write(".GITIGNORE", "private.md\n");
        project.write("sub/.Ignore", "local.md\n");
        project.write("private.md", "ignored");
        project.write("sub/local.md", "ignored");
        project.write("keep.md", "kept");
        assert_eq!(project.scan(), ["keep.md"]);
    }

    #[test]
    fn hidden_and_build_directories_are_not_descended() {
        for skipped in [
            ".git",
            ".claude",
            ".venv",
            "target",
            "node_modules",
            "build",
            "",
            "bad\tname",
        ] {
            assert!(!enters_dir(skipped), "{skipped:?} should be skipped");
        }
        for kept in ["src", "docs", "tests"] {
            assert!(enters_dir(kept), "{kept} should be indexed");
        }
    }

    #[test]
    fn a_walk_scoped_inside_an_ignored_folder_finds_nothing() {
        let project = Project::new("scoped-ignored");
        project.write(".gitignore", "generated/\n");
        project.write("generated/api.md", "ignored");
        project.write("generated/deep/more.md", "ignored");
        project.write("src/lib.rs", "kept");
        for scope in [
            Scope::Deep(Rel::from_event("generated").unwrap()),
            Scope::Shallow(Rel::from_event("generated").unwrap()),
            Scope::Deep(Rel::from_event("generated/deep").unwrap()),
        ] {
            let found = walk(&project.0, &scope, |_, _| Ok(true))
                .unwrap()
                .unwrap()
                .found;
            assert!(found.is_empty(), "{scope:?} found {found:?}");
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len())]
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.next() % 100 < percent
        }
    }

    const DIRS: &[&str] = &[
        "", "a", "a/b", "a/b/e", "a/local", "gen", "gen/sub", "c", "c/d", "C2", "target", ".hidden",
    ];
    const NAMES: &[&str] = &[
        "x.md",
        "y.rs",
        "z.txt",
        "secret1.md",
        "n.log.md",
        "Makefile",
        "w.bin",
        "keep.md",
    ];
    const ROOT_RULES: &[&str] = &["gen/\n*.log.md\n", "", "c/d/\n", "*.rs\n!a/**/*.rs\n"];
    const MID_RULES: &[&str] = &["local/\n", "", "*.txt\n", "e/\n"];
    const PARENT_RULES: &[&str] = &["secret*.md\n", "", "z.txt\n"];

    struct Fixture {
        outer: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new(seed: u64) -> Self {
            let outer = env::temp_dir().join(format!("recall-converge-{}-{seed}", process::id()));
            let _ = fs::remove_dir_all(&outer);
            let root = outer.join("proj");
            fs::create_dir_all(&root).unwrap();
            fs::write(outer.join(".gitignore"), PARENT_RULES[0]).unwrap();
            fs::write(root.join(".gitignore"), ROOT_RULES[0]).unwrap();
            fs::create_dir_all(root.join("a")).unwrap();
            fs::write(root.join("a/.gitignore"), MID_RULES[0]).unwrap();
            Self { outer, root }
        }

        fn paths(&self) -> Vec<String> {
            fn visit(dir: &Path, prefix: &str, out: &mut Vec<String>) {
                let Ok(entries) = fs::read_dir(dir) else {
                    return;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let key = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    };
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        visit(&entry.path(), &key, out);
                    }
                    out.push(key);
                }
            }
            let mut out = Vec::new();
            visit(&self.root, "", &mut out);
            out
        }

        fn ensure_dir(&self, rel: &str, events: &mut Vec<String>) {
            let mut path = String::new();
            for part in rel.split('/').filter(|part| !part.is_empty()) {
                path = if path.is_empty() {
                    part.to_owned()
                } else {
                    format!("{path}/{part}")
                };
                if !self.root.join(&path).exists() {
                    fs::create_dir(self.root.join(&path)).unwrap();
                    events.push(path.clone());
                }
            }
        }

        fn mutate(&self, rng: &mut Rng) -> Option<Vec<String>> {
            let mut events = Vec::new();
            let join = |dir: &str, name: &str| {
                if dir.is_empty() {
                    name.to_owned()
                } else {
                    format!("{dir}/{name}")
                }
            };
            match rng.below(9) {
                0..=2 => {
                    let dir = *rng.pick(DIRS);
                    let key = join(dir, rng.pick(NAMES));
                    if self.root.join(&key).is_dir() {
                        return Some(events);
                    }
                    self.ensure_dir(dir, &mut events);
                    let body = "x".repeat(1 + rng.below(40));
                    fs::write(self.root.join(&key), body).unwrap();
                    events.push(key);
                }
                3 => {
                    let files: Vec<String> = self
                        .paths()
                        .into_iter()
                        .filter(|path| self.root.join(path).is_file())
                        .collect();
                    if !files.is_empty() {
                        let key = rng.pick(&files).clone();
                        fs::remove_file(self.root.join(&key)).unwrap();
                        events.push(key);
                    }
                }
                4 => {
                    let files: Vec<String> = self
                        .paths()
                        .into_iter()
                        .filter(|path| self.root.join(path).is_file())
                        .collect();
                    if !files.is_empty() {
                        let from = rng.pick(&files).clone();
                        let dir = *rng.pick(DIRS);
                        let to = join(dir, rng.pick(NAMES));
                        if !self.root.join(&to).exists() {
                            self.ensure_dir(dir, &mut events);
                            fs::rename(self.root.join(&from), self.root.join(&to)).unwrap();
                            events.extend([from, to]);
                        }
                    }
                }
                5 => {
                    let dirs: Vec<String> = self
                        .paths()
                        .into_iter()
                        .filter(|path| self.root.join(path).is_dir())
                        .collect();
                    if !dirs.is_empty() {
                        let from = rng.pick(&dirs).clone();
                        let parent = *rng.pick(DIRS);
                        let to = join(parent, rng.pick(&["moved", "gen", "local", "e", "q"]));
                        let inside = to == from || to.starts_with(&format!("{from}/"));
                        if !inside && !self.root.join(&to).exists() {
                            self.ensure_dir(parent, &mut events);
                            if self.root.join(parent).is_dir()
                                && fs::rename(self.root.join(&from), self.root.join(&to)).is_ok()
                            {
                                events.extend([from, to]);
                            }
                        }
                    }
                }
                6 => {
                    let dirs: Vec<String> = self
                        .paths()
                        .into_iter()
                        .filter(|path| self.root.join(path).is_dir())
                        .collect();
                    if !dirs.is_empty() {
                        let gone = rng.pick(&dirs).clone();
                        fs::remove_dir_all(self.root.join(&gone)).unwrap();
                        events.push(gone);
                    }
                }
                7 => {
                    let (dir, rules) = if rng.chance(50) {
                        ("", rng.pick(ROOT_RULES))
                    } else {
                        (*rng.pick(&["a", "c", "a/b", "gen"]), rng.pick(MID_RULES))
                    };
                    self.ensure_dir(dir, &mut events);
                    let key = join(dir, ".gitignore");
                    fs::write(self.root.join(&key), rules).unwrap();
                    events.push(key);
                }
                _ => {
                    fs::write(self.outer.join(".gitignore"), rng.pick(PARENT_RULES)).unwrap();
                    return None;
                }
            }
            Some(events)
        }

        fn scopes(&self) -> Vec<Scope> {
            let mut names: BTreeSet<String> = DIRS.iter().map(|dir| (*dir).to_owned()).collect();
            names.extend(self.paths());
            names.insert("missing/nowhere".into());
            names
                .into_iter()
                .filter_map(|name| {
                    if name.is_empty() {
                        Some(Rel::ROOT)
                    } else {
                        Rel::from_event(&name)
                    }
                })
                .flat_map(|rel| [Scope::Shallow(rel.clone()), Scope::Deep(rel)])
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.outer);
        }
    }

    fn assert_scoped_walks_match(fixture: &Fixture, step: usize) {
        let full = full_walk(&fixture.root);
        for scope in fixture.scopes() {
            let scoped = walk(&fixture.root, &scope, |_, _| Ok(true))
                .unwrap()
                .unwrap()
                .found;
            let expected: BTreeMap<String, Stamp> = full
                .iter()
                .filter(|(key, _)| scope.covers(key))
                .map(|(key, stamp)| (key.clone(), *stamp))
                .collect();
            assert_eq!(scoped, expected, "step {step}, {scope:?}");
        }
    }

    #[test]
    fn the_parent_folder_gitignore_applies_to_the_project() {
        let fixture = Fixture::new(0);
        fs::write(fixture.root.join("secret1.md"), "x").unwrap();
        fs::write(fixture.root.join("keep.md"), "x").unwrap();
        let keys: Vec<String> = full_walk(&fixture.root).into_keys().collect();
        assert_eq!(keys, [".gitignore", "a/.gitignore", "keep.md"]);
    }

    #[test]
    fn a_scoped_walk_equals_the_full_walk_filtered_to_its_scope() {
        for seed in [0x9e37_79b9_7f4a_7c15, 0xdead_beef, 42] {
            let fixture = Fixture::new(seed);
            let mut rng = Rng(seed);
            for step in 0..60 {
                fixture.mutate(&mut rng);
                if step % 3 == 0 {
                    assert_scoped_walks_match(&fixture, step);
                }
            }
            assert_scoped_walks_match(&fixture, 60);
        }
    }

    #[test]
    fn truth_fed_any_superset_of_changed_paths_converges_to_a_full_walk() {
        for seed in [7, 0x1234_5678_9abc_def1, 0xfeed_f00d] {
            let fixture = Fixture::new(seed);
            let mut rng = Rng(seed);
            let limits = Debounce::default();
            let mut truth = Truth::new(Epoch(seed));
            let mut dirty = Dirty::default();
            dirty.overflow(Instant::now());
            for batch in 0..50 {
                let mut changed = Vec::new();
                let mut watched = true;
                for _ in 0..1 + rng.below(4) {
                    match fixture.mutate(&mut rng) {
                        Some(events) => changed.extend(events),
                        None => watched = false,
                    }
                }
                let on_disk = fixture.paths();
                for _ in 0..rng.below(4) {
                    if !on_disk.is_empty() {
                        changed.push(rng.pick(&on_disk).clone());
                    }
                }
                for _ in 0..rng.below(changed.len() + 1) {
                    let again = rng.pick(&changed).clone();
                    changed.push(again);
                }
                for index in (1..changed.len()).rev() {
                    changed.swap(index, rng.below(index + 1));
                }
                let now = Instant::now();
                if !watched || rng.chance(10) {
                    dirty.overflow(now);
                }
                for raw in &changed {
                    let raw = if rng.chance(50) {
                        raw.replace('/', "\\")
                    } else {
                        raw.clone()
                    };
                    if let Some(rel) = Rel::from_event(&raw) {
                        dirty.note(rel, now, &limits);
                    }
                }
                for scope in dirty.resolve(&truth, &Disk(&fixture.root), &limits) {
                    let observed = walk(&fixture.root, &scope, |_, _| Ok(true))
                        .unwrap()
                        .unwrap();
                    truth.apply(observed);
                }
                assert_eq!(
                    truth.files(),
                    &full_walk(&fixture.root),
                    "seed {seed:#x}, batch {batch}, events {changed:?}"
                );
            }
        }
    }
}
