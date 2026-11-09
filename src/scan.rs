use std::{collections::BTreeMap, error::Error, fs, path::Path};

use ignore::WalkBuilder;

use crate::{
    cache::Stamp,
    truth::{Kind, Observed, Probe, Rel, Scope},
};

const INDEX_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx", "rs", "md", "txt", "rst", "py", "go", "java",
    "js", "jsx", "ts", "tsx", "json", "toml", "yaml", "yml", "ini", "sh", "bat", "ps1", "html",
    "css", "sql", "cmake",
];
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// Walks the files `scope` covers. The walk always starts at the project root
/// and prunes toward the scope, because `ignore` never filters the folder it
/// starts in and only learns a folder's rules by passing through it. A walk
/// started inside an ignored folder would index what a full walk skips.
pub fn walk(
    root: &Path,
    scope: &Scope,
    mut tick: impl FnMut(usize, &str) -> Result<bool, Box<dyn Error>>,
) -> Result<Option<Observed>, Box<dyn Error>> {
    let prune_root = root.to_path_buf();
    let prune_scope = scope.clone();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|kind| kind.is_dir())
                || entry.file_name().to_str().is_some_and(enters_dir)
                    && key_of(&prune_root, entry.path()).is_some_and(|dir| prune_scope.enters(&dir))
        })
        .build();
    let mut found = BTreeMap::new();
    let mut latest = String::new();
    for entry in walker.filter_map(Result::ok) {
        if !tick(found.len(), &latest)? {
            return Ok(None);
        }
        let admitted = entry.file_type().is_some_and(|kind| kind.is_file())
            && entry.file_name().to_str().is_some_and(is_indexable);
        let Some(key) = admitted.then(|| key_of(root, entry.path())).flatten() else {
            continue;
        };
        if !scope.covers(&key) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        found.insert(key.clone(), Stamp::of(&metadata));
        latest = key;
    }
    Ok(Some(Observed {
        scope: scope.clone(),
        found,
    }))
}

fn key_of(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = relative.iter().map(|part| part.to_str()).collect();
    Some(parts?.join("/"))
}

/// The filesystem as `Dirty::resolve` sees it.
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

/// Folders a walk never enters. Shared with `Rel::from_event` so an event
/// inside one is dropped as noise.
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

/// Files whose edits change what is indexable without touching any indexed file.
pub fn is_rule_file(name: &str) -> bool {
    matches!(name, ".gitignore" | ".ignore")
}

/// Keys go into a line-based manifest and pipe protocol, so they never hold one.
fn has_control(name: &str) -> bool {
    name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::truth::{Debounce, Dirty, Epoch, Truth};
    use std::{collections::BTreeSet, env, path::PathBuf, process, time::Instant};

    fn full_walk(root: &Path) -> BTreeMap<String, Stamp> {
        walk(root, &Scope::ROOT, |_, _| Ok(true))
            .unwrap()
            .unwrap()
            .found
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
    fn a_tick_that_returns_false_cancels_the_walk() {
        let project = Project::new("cancel");
        project.write("a.md", "a");
        project.write("b.md", "b");
        let outcome = walk(&project.0, &Scope::ROOT, |_, _| Ok(false)).unwrap();
        assert!(outcome.is_none());
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

    /// A project under a parent folder whose own `.gitignore` applies to it.
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

        /// Every path on disk under the root, as a raw event name.
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

        /// One random edit. Returns the names an RDCW handle on the root would report,
        /// or `None` when the edit sits above the root, where nothing is watched.
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

        /// Scopes worth checking: every folder pool entry and every path on disk,
        /// shallow and deep, including ignored, hidden and missing ones.
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
