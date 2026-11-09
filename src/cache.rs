use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntry {
    pub name: String,
    pub root: Option<String>,
    pub files: usize,
    pub bytes: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Default)]
struct Found {
    bytes: u64,
    modified: Option<SystemTime>,
    manifest: Option<PathBuf>,
}

fn is_entry_name(name: &str) -> bool {
    name.len() == 16
        && name
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_cache_file(name: &str) -> bool {
    [".lux", ".manifest", ".tmp"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

struct CacheFile {
    name: String,
    metadata: fs::Metadata,
    path: PathBuf,
}

fn cache_files_in(dir: &Path) -> Result<Vec<CacheFile>, Box<dyn Error>> {
    let reader = match fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut files = Vec::new();
    for entry in reader {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if let (true, Some(name)) = (metadata.is_file(), entry.file_name().to_str())
            && is_cache_file(name)
        {
            files.push(CacheFile {
                name: name.to_owned(),
                metadata,
                path: entry.path(),
            });
        }
    }
    Ok(files)
}

pub fn list_entries(dir: &Path) -> Vec<CacheEntry> {
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    for file in cache_files_in(dir).unwrap_or_default() {
        let (stem, is_manifest) = match file.name.rsplit_once('.') {
            Some((stem, "lux")) => (stem, false),
            Some((stem, "manifest")) => (stem, true),
            _ => continue,
        };
        if !is_entry_name(stem) {
            continue;
        }
        let slot = found.entry(stem.to_owned()).or_default();
        slot.bytes += file.metadata.len();
        slot.modified = slot.modified.max(file.metadata.modified().ok());
        if is_manifest {
            slot.manifest = Some(file.path);
        }
    }
    let mut entries: Vec<CacheEntry> = found
        .into_iter()
        .map(|(name, slot)| {
            let parsed = slot
                .manifest
                .and_then(|path| fs::read_to_string(path).ok())
                .and_then(|text| parse_manifest(&text));
            CacheEntry {
                name,
                files: parsed.as_ref().map_or(0, |parsed| {
                    parsed
                        .manifest
                        .values()
                        .filter(|entry| entry.indexed)
                        .count()
                }),
                root: parsed.map(|parsed| parsed.root),
                bytes: slot.bytes,
                modified: slot.modified,
            }
        })
        .collect();
    entries.sort_by_cached_key(|entry| {
        (
            entry.root.is_none(),
            entry.root.as_deref().unwrap_or_default().to_lowercase(),
            entry.name.clone(),
        )
    });
    entries
}

pub fn delete_entry(dir: &Path, name: &str) -> Result<usize, Box<dyn Error>> {
    if !is_entry_name(name) {
        return Err(format!("{name:?} is not a saved index name").into());
    }
    let prefix = format!("{name}.");
    let mut removed = 0;
    for file in cache_files_in(dir)? {
        let ours = file
            .name
            .strip_prefix(&prefix)
            .is_some_and(|rest| matches!(rest, "lux" | "manifest") || rest.ends_with(".tmp"));
        if ours && remove_if_present(&file.path)? {
            removed += 1;
        }
    }
    Ok(removed)
}

pub fn delete_all(dir: &Path) -> Result<usize, Box<dyn Error>> {
    let mut removed = 0;
    for file in cache_files_in(dir)? {
        if remove_if_present(&file.path)? {
            removed += 1;
        }
    }
    Ok(removed)
}

fn remove_if_present(path: &Path) -> Result<bool, Box<dyn Error>> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
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
        run_in(workspace, &workspace.0.join("cache"), fingerprint)
    }

    fn run_in(workspace: &Workspace, dir: &Path, fingerprint: &str) -> (LuxIndex, Summary) {
        let mut index = LuxIndex::load(&lux::library_path()).expect("build libluxcore");
        let files = cache_files(dir, &workspace.0);
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

    fn stem_of(workspace: &Workspace, dir: &Path) -> String {
        let files = cache_files(dir, &workspace.0);
        files
            .index
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn fake_entry(dir: &Path, name: &str, root: Option<&str>, indexed: usize) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.lux")), vec![7u8; 100]).unwrap();
        if let Some(root) = root {
            let manifest: Manifest = (0..indexed)
                .map(|n| {
                    (
                        format!("f{n}.md"),
                        Entry {
                            stamp: stamp(1, 1),
                            indexed: true,
                        },
                    )
                })
                .collect();
            fs::write(
                dir.join(format!("{name}.manifest")),
                serialize("fp", Path::new(root), &manifest),
            )
            .unwrap();
        }
    }

    #[test]
    fn listing_shows_the_root_file_count_and_size_of_a_synced_folder() {
        let home = Workspace::new("list-home");
        let dir = home.0.join("cache");
        let workspace = Workspace::new("list-one");
        workspace.write("a.md", b"alpha");
        workspace.write("b.md", b"beta");
        workspace.write("c.txt", &[0xff, 0xfe, 0x00, 0x9f]);
        run_in(&workspace, &dir, "fp");

        let entries = list_entries(&dir);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.name, stem_of(&workspace, &dir));
        assert_eq!(entry.root.as_deref(), workspace.0.to_str());
        assert_eq!(entry.files, 2);
        let on_disk: u64 = fs::read_dir(&dir)
            .unwrap()
            .map(|file| file.unwrap().metadata().unwrap().len())
            .sum();
        assert_eq!(entry.bytes, on_disk);
        assert!(entry.modified.is_some());
    }

    #[test]
    fn listing_handles_orphans_garbage_and_sorts_by_folder() {
        let home = Workspace::new("list-sort");
        let dir = home.0.join("cache");
        fake_entry(&dir, "0000000000000003", Some("F:/beta"), 2);
        fake_entry(&dir, "0000000000000001", Some("F:/Alpha"), 5);
        fake_entry(&dir, "0000000000000002", None, 0);
        fake_entry(&dir, "0000000000000004", Some("F:/gamma"), 1);
        fs::write(dir.join("0000000000000004.manifest"), "garbage").unwrap();
        fs::write(dir.join("notes.txt"), "mine").unwrap();
        fs::write(dir.join("notahexname.lux"), "x").unwrap();

        let entries = list_entries(&dir);
        let seen: Vec<(&str, Option<&str>, usize)> = entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.root.as_deref(), entry.files))
            .collect();
        assert_eq!(
            seen,
            [
                ("0000000000000001", Some("F:/Alpha"), 5),
                ("0000000000000003", Some("F:/beta"), 2),
                ("0000000000000002", None, 0),
                ("0000000000000004", None, 0),
            ]
        );
        assert!(entries[2].modified.is_some());
    }

    #[test]
    fn listing_a_missing_folder_is_empty() {
        let home = Workspace::new("list-missing");
        assert!(list_entries(&home.0.join("nope")).is_empty());
    }

    #[test]
    fn deleting_an_entry_removes_only_its_files() {
        let home = Workspace::new("delete-one");
        let dir = home.0.join("cache");
        fake_entry(&dir, "00000000000000aa", Some("F:/one"), 1);
        fake_entry(&dir, "00000000000000bb", Some("F:/two"), 1);
        fs::write(dir.join("00000000000000aa.lux.123.tmp"), "half").unwrap();
        fs::write(dir.join("00000000000000aa.manifest.123.tmp"), "half").unwrap();
        fs::write(dir.join("00000000000000bb.lux.9.tmp"), "half").unwrap();
        fs::write(dir.join("keep.txt"), "mine").unwrap();
        fs::create_dir(dir.join("00000000000000aa.lux.d")).unwrap();

        assert_eq!(delete_entry(&dir, "00000000000000aa").unwrap(), 4);
        assert_eq!(
            names_in(&dir),
            [
                "00000000000000aa.lux.d",
                "00000000000000bb.lux",
                "00000000000000bb.lux.9.tmp",
                "00000000000000bb.manifest",
                "keep.txt",
            ]
        );
        assert_eq!(list_entries(&dir).len(), 1);
    }

    #[test]
    fn deleting_an_entry_refuses_names_that_could_escape_the_folder() {
        let home = Workspace::new("delete-guard");
        let dir = home.0.join("cache");
        fake_entry(&dir, "00000000000000aa", Some("F:/one"), 1);
        fs::write(home.0.join("x.lux"), "outside").unwrap();
        let before = names_in(&dir);
        for bad in [
            "..\\x",
            "../x",
            "..",
            "",
            "00000000000000a",
            "00000000000000aaa",
            "00000000000000AA",
            "0000000000000zzz",
            "00000000000000aa/",
        ] {
            assert!(
                delete_entry(&dir, bad).is_err(),
                "{bad:?} should be refused"
            );
        }
        assert_eq!(names_in(&dir), before);
        assert!(home.0.join("x.lux").exists());
    }

    #[test]
    fn deleting_an_entry_twice_is_fine() {
        let home = Workspace::new("delete-twice");
        let dir = home.0.join("cache");
        fake_entry(&dir, "00000000000000aa", Some("F:/one"), 1);
        assert_eq!(delete_entry(&dir, "00000000000000aa").unwrap(), 2);
        assert_eq!(delete_entry(&dir, "00000000000000aa").unwrap(), 0);
        assert_eq!(
            delete_entry(&dir.join("gone"), "00000000000000aa").unwrap(),
            0
        );
    }

    #[test]
    fn deleting_all_removes_cache_files_and_nothing_else() {
        let home = Workspace::new("delete-all");
        let dir = home.0.join("cache");
        fake_entry(&dir, "00000000000000aa", Some("F:/one"), 1);
        fake_entry(&dir, "00000000000000bb", None, 0);
        fs::write(dir.join("00000000000000aa.lux.77.tmp"), "half").unwrap();
        fs::write(dir.join("keep.txt"), "mine").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("deep.lux"), "nested").unwrap();

        assert_eq!(delete_all(&dir).unwrap(), 4);
        assert_eq!(names_in(&dir), ["keep.txt", "sub"]);
        assert!(dir.join("sub").join("deep.lux").exists());
        assert_eq!(delete_all(&dir).unwrap(), 0);
        assert_eq!(delete_all(&dir.join("gone")).unwrap(), 0);
        assert!(dir.is_dir());
    }

    #[test]
    fn a_deleted_entry_is_rebuilt_on_the_next_sync_and_reused_after() {
        let home = Workspace::new("delete-resync");
        let dir = home.0.join("cache");
        let workspace = Workspace::new("delete-resync-src");
        workspace.write("a.md", b"alpha");
        assert!(!run_in(&workspace, &dir, "fp").1.reused);
        assert!(run_in(&workspace, &dir, "fp").1.reused);

        delete_entry(&dir, &stem_of(&workspace, &dir)).unwrap();
        assert!(list_entries(&dir).is_empty());

        let (index, rebuilt) = run_in(&workspace, &dir, "fp");
        assert!(!rebuilt.reused);
        assert_eq!(found(&index, "alpha"), ["a.md"]);
        assert!(run_in(&workspace, &dir, "fp").1.reused);
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
