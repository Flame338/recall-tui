use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    process,
    time::UNIX_EPOCH,
};

use crate::lux::LuxIndex;

const FORMAT: &str = "recall-cache 2";
const RULES_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    pub modified: u64,
}

impl Stamp {
    pub fn of(metadata: &fs::Metadata) -> Self {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_nanos() as u64);
        Self {
            size: metadata.len(),
            modified,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub stamp: Stamp,
    pub indexed: bool,
}

pub type Manifest = BTreeMap<String, Entry>;

pub struct Scanned {
    pub path: PathBuf,
    pub stamp: Stamp,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub to_read: Vec<String>,
    pub to_remove: Vec<String>,
}

pub fn plan(cached: &Manifest, scanned: &BTreeMap<String, Scanned>) -> Plan {
    let to_read = scanned
        .iter()
        .filter(|(key, file)| {
            cached
                .get(*key)
                .is_none_or(|entry| entry.stamp != file.stamp)
        })
        .map(|(key, _)| key.clone())
        .collect();
    let to_remove = cached
        .iter()
        .filter(|(key, entry)| entry.indexed && !scanned.contains_key(*key))
        .map(|(key, _)| key.clone())
        .collect();
    Plan { to_read, to_remove }
}

pub fn serialize(fingerprint: &str, root: &Path, manifest: &Manifest) -> String {
    let root = root.to_string_lossy().replace(['\n', '\r'], " ");
    let mut text = format!("{FORMAT}\n{fingerprint}\nroot {root}\n");
    for (key, entry) in manifest {
        text.push_str(&format!(
            "{}\t{}\t{}\t{key}\n",
            entry.stamp.size,
            entry.stamp.modified,
            u8::from(entry.indexed)
        ));
    }
    text
}

#[derive(Debug, PartialEq, Eq)]
pub struct Parsed {
    pub fingerprint: String,
    pub root: String,
    pub manifest: Manifest,
}

pub fn parse_manifest(text: &str) -> Option<Parsed> {
    let mut lines = text.lines();
    if lines.next()? != FORMAT {
        return None;
    }
    let fingerprint = lines.next()?.to_owned();
    let root = lines.next()?.strip_prefix("root ")?.to_owned();
    let mut manifest = Manifest::new();
    for line in lines {
        let mut fields = line.splitn(4, '\t');
        let size = fields.next()?.parse().ok()?;
        let modified = fields.next()?.parse().ok()?;
        let indexed = match fields.next()? {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        let key = fields.next()?.to_owned();
        manifest.insert(
            key,
            Entry {
                stamp: Stamp { size, modified },
                indexed,
            },
        );
    }
    Some(Parsed {
        fingerprint,
        root,
        manifest,
    })
}

pub fn parse(text: &str, fingerprint: &str) -> Option<Manifest> {
    let parsed = parse_manifest(text)?;
    (parsed.fingerprint == fingerprint).then_some(parsed.manifest)
}

pub fn fingerprint(library: &Path) -> String {
    let stamp = fs::metadata(library).map(|metadata| Stamp::of(&metadata));
    match stamp {
        Ok(stamp) => format!(
            "rules {RULES_VERSION} lib {} {}",
            stamp.size, stamp.modified
        ),
        Err(_) => format!("rules {RULES_VERSION} lib unknown"),
    }
}

pub struct CacheFiles {
    pub root: PathBuf,
    pub index: PathBuf,
    pub manifest: PathBuf,
}

pub fn cache_files(dir: &Path, root: &Path) -> CacheFiles {
    let name = format!("{:016x}", fnv1a(root.to_string_lossy().as_bytes()));
    CacheFiles {
        root: root.to_path_buf(),
        index: dir.join(format!("{name}.lux")),
        manifest: dir.join(format!("{name}.manifest")),
    }
}

pub fn default_dir() -> Option<PathBuf> {
    let from_env = |name: &str| env::var_os(name).filter(|value| !value.is_empty());
    if let Some(base) = from_env("LOCALAPPDATA") {
        return Some(PathBuf::from(base).join("Recall"));
    }
    if let Some(base) = from_env("XDG_CACHE_HOME") {
        return Some(PathBuf::from(base).join("recall"));
    }
    from_env("HOME").map(|home| PathBuf::from(home).join(".cache").join("recall"))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub indexed: usize,
    pub reused: bool,
    pub updated: usize,
    pub removed: usize,
    pub saved: bool,
}

pub fn sync(
    index: &mut LuxIndex,
    files: Option<&CacheFiles>,
    fingerprint: &str,
    scanned: &BTreeMap<String, Scanned>,
    mut progress: impl FnMut(usize, usize, &str) -> Result<bool, Box<dyn Error>>,
) -> Result<Option<Summary>, Box<dyn Error>> {
    let cached = files.and_then(|files| load(index, files, fingerprint));
    let reused = cached.is_some();
    let mut manifest = cached.unwrap_or_default();
    let plan = plan(&manifest, scanned);

    for key in &plan.to_remove {
        index.remove(key)?;
        manifest.remove(key);
    }
    let total = plan.to_read.len();
    for (position, key) in plan.to_read.iter().enumerate() {
        if !progress(position, total, key)? {
            return Ok(None);
        }
        let file = &scanned[key];
        let was_indexed = manifest.get(key).is_some_and(|entry| entry.indexed);
        let indexed = match fs::read_to_string(&file.path) {
            Ok(body) => {
                let title = file.path.file_name().unwrap_or_default().to_string_lossy();
                index.add(key, &title, &body)?;
                true
            }
            Err(_) => {
                if was_indexed {
                    index.remove(key)?;
                }
                false
            }
        };
        manifest.insert(
            key.clone(),
            Entry {
                stamp: file.stamp,
                indexed,
            },
        );
    }

    let changed = !reused || total > 0 || !plan.to_remove.is_empty();
    let saved =
        changed && files.is_some_and(|files| save(index, files, fingerprint, &manifest).is_ok());
    Ok(Some(Summary {
        indexed: manifest.values().filter(|entry| entry.indexed).count(),
        reused,
        updated: total,
        removed: plan.to_remove.len(),
        saved,
    }))
}

fn load(index: &mut LuxIndex, files: &CacheFiles, fingerprint: &str) -> Option<Manifest> {
    let text = fs::read_to_string(&files.manifest).ok()?;
    let manifest = parse(&text, fingerprint)?;
    index.restore(&files.index).ok()?;
    Some(manifest)
}

fn save(
    index: &LuxIndex,
    files: &CacheFiles,
    fingerprint: &str,
    manifest: &Manifest,
) -> Result<(), Box<dyn Error>> {
    if let Some(dir) = files.index.parent() {
        fs::create_dir_all(dir)?;
    }
    write_atomic(&files.index, |path| index.save(path))?;
    write_atomic(&files.manifest, |path| {
        Ok(fs::write(
            path,
            serialize(fingerprint, &files.root, manifest),
        )?)
    })
}

fn write_atomic(
    target: &Path,
    write: impl FnOnce(&Path) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let mut temp = target.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", process::id()));
    let temp = PathBuf::from(temp);
    if let Err(error) = write(&temp) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lux;

    fn stamp(size: u64, modified: u64) -> Stamp {
        Stamp { size, modified }
    }

    fn scanned(entries: &[(&str, Stamp)]) -> BTreeMap<String, Scanned> {
        entries
            .iter()
            .map(|(key, stamp)| {
                (
                    (*key).to_owned(),
                    Scanned {
                        path: PathBuf::from(key),
                        stamp: *stamp,
                    },
                )
            })
            .collect()
    }

    fn cached(entries: &[(&str, Stamp, bool)]) -> Manifest {
        entries
            .iter()
            .map(|(key, stamp, indexed)| {
                (
                    (*key).to_owned(),
                    Entry {
                        stamp: *stamp,
                        indexed: *indexed,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn manifest_round_trips_and_rejects_other_fingerprints_and_garbage() {
        let manifest = cached(&[
            ("a/b c.md", stamp(5, 9), true),
            ("z.txt", stamp(1, 2), false),
        ]);
        let text = serialize("fp", Path::new("F:/proj"), &manifest);
        assert_eq!(parse(&text, "fp"), Some(manifest));
        assert_eq!(parse(&text, "other"), None);
        assert_eq!(parse("junk", "fp"), None);
        assert_eq!(
            parse("recall-cache 2\nfp\nroot F:/proj\nx\ty\n", "fp"),
            None
        );
    }

    #[test]
    fn the_manifest_remembers_its_root_and_old_formats_do_not_parse() {
        let manifest = cached(&[("a.md", stamp(5, 9), true)]);
        let text = serialize("fp", Path::new("F:/my proj"), &manifest);
        let parsed = parse_manifest(&text).unwrap();
        assert_eq!(parsed.fingerprint, "fp");
        assert_eq!(parsed.root, "F:/my proj");
        assert_eq!(parsed.manifest, manifest);
        assert_eq!(parse_manifest("recall-cache 1\nfp\n5\t9\t1\ta.md\n"), None);
        assert_eq!(parse_manifest("recall-cache 2\nfp\n5\t9\t1\ta.md\n"), None);
    }

    #[test]
    fn a_root_with_a_line_break_still_round_trips() {
        let text = serialize("fp", Path::new("odd\nname"), &Manifest::new());
        assert!(parse(&text, "fp").is_some());
    }

    #[test]
    fn plan_reads_new_and_changed_files_and_removes_vanished_indexed_ones() {
        let old = cached(&[
            ("same", stamp(1, 1), true),
            ("changed", stamp(1, 1), true),
            ("gone", stamp(1, 1), true),
            ("skipped-gone", stamp(1, 1), false),
            ("skipped-same", stamp(1, 1), false),
        ]);
        let now = scanned(&[
            ("same", stamp(1, 1)),
            ("changed", stamp(2, 1)),
            ("new", stamp(1, 1)),
            ("skipped-same", stamp(1, 1)),
        ]);
        assert_eq!(
            plan(&old, &now),
            Plan {
                to_read: vec!["changed".into(), "new".into()],
                to_remove: vec!["gone".into()],
            }
        );
    }

    #[test]
    fn cache_names_depend_on_the_root() {
        let dir = Path::new("cache");
        let a = cache_files(dir, Path::new("F:/one"));
        let b = cache_files(dir, Path::new("F:/two"));
        assert_ne!(a.index, b.index);
        assert_eq!(a.index, cache_files(dir, Path::new("F:/one")).index);
        assert_eq!(a.root, Path::new("F:/one"));
    }

    struct Workspace(PathBuf);

    impl Workspace {
        fn new(name: &str) -> Self {
            let dir = env::temp_dir().join(format!("recall-test-{}-{name}", process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("src")).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, bytes: &[u8]) {
            fs::write(self.0.join("src").join(name), bytes).unwrap();
        }

        fn scan(&self) -> BTreeMap<String, Scanned> {
            fs::read_dir(self.0.join("src"))
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    let key = entry.file_name().to_string_lossy().into_owned();
                    let stamp = Stamp::of(&entry.metadata().unwrap());
                    (
                        key,
                        Scanned {
                            path: entry.path(),
                            stamp,
                        },
                    )
                })
                .collect()
        }

        fn files(&self) -> CacheFiles {
            cache_files(&self.0.join("cache"), &self.0)
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run(workspace: &Workspace, fingerprint: &str) -> (LuxIndex, Summary) {
        let mut index = LuxIndex::load(&lux::library_path()).expect("build libluxcore");
        let files = workspace.files();
        let summary = sync(
            &mut index,
            Some(&files),
            fingerprint,
            &workspace.scan(),
            |_, _, _| Ok(true),
        )
        .unwrap()
        .unwrap();
        (index, summary)
    }

    fn found(index: &LuxIndex, query: &str) -> Vec<String> {
        index
            .search(query, 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.key)
            .collect()
    }

    #[test]
    fn a_saved_index_is_reused_and_only_changed_files_are_reread() {
        let workspace = Workspace::new("sync");
        workspace.write("a.md", b"alpha unique words");
        workspace.write("b.md", b"beta other words");
        workspace.write("c.txt", &[0xff, 0xfe, 0x00, 0x9f]);

        let (index, first) = run(&workspace, "fp");
        assert_eq!(
            first,
            Summary {
                indexed: 2,
                reused: false,
                updated: 3,
                removed: 0,
                saved: true
            }
        );
        assert_eq!(found(&index, "alpha"), ["a.md"]);

        let (index, second) = run(&workspace, "fp");
        assert_eq!(
            second,
            Summary {
                indexed: 2,
                reused: true,
                updated: 0,
                removed: 0,
                saved: false
            }
        );
        assert_eq!(found(&index, "alpha"), ["a.md"]);

        workspace.write("b.md", b"gamma replaced the old text entirely");
        let (index, third) = run(&workspace, "fp");
        assert_eq!((third.reused, third.updated, third.removed), (true, 1, 0));
        assert_eq!(found(&index, "gamma"), ["b.md"]);
        assert!(found(&index, "beta").is_empty());

        fs::remove_file(workspace.0.join("src").join("a.md")).unwrap();
        let (index, fourth) = run(&workspace, "fp");
        assert_eq!((fourth.updated, fourth.removed, fourth.indexed), (0, 1, 1));
        assert!(found(&index, "alpha").is_empty());

        let (_, rebuilt) = run(&workspace, "different library");
        assert!(!rebuilt.reused);
        assert_eq!(rebuilt.updated, 2);
    }

    #[test]
    fn a_synced_workspace_saves_its_root_in_the_manifest() {
        let workspace = Workspace::new("root");
        workspace.write("a.md", b"alpha");
        run(&workspace, "fp");
        let text = fs::read_to_string(workspace.files().manifest).unwrap();
        let parsed = parse_manifest(&text).unwrap();
        assert_eq!(parsed.root, workspace.0.to_string_lossy());
    }

    #[test]
    fn cancelling_stops_before_saving() {
        let workspace = Workspace::new("cancel");
        workspace.write("a.md", b"alpha");
        let mut index = LuxIndex::load(&lux::library_path()).unwrap();
        let files = workspace.files();
        let outcome = sync(
            &mut index,
            Some(&files),
            "fp",
            &workspace.scan(),
            |_, _, _| Ok(false),
        )
        .unwrap();
        assert!(outcome.is_none());
        assert!(!files.manifest.exists());
    }
}
