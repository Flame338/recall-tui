use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    process, thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    lux::LuxIndex,
    truth::{Change, Cursor, Epoch, Gen},
};

const FORMAT: &str = "recall-cache 3";
// 2: the index stopped storing file bodies.
const RULES_VERSION: u32 = 2;

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

/// One manifest row as the file stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub stamp: Stamp,
    pub indexed: bool,
}

pub type Manifest = BTreeMap<String, Entry>;

/// A row stores an unread file's stamp as all ones, so the format stays `recall-cache 3`.
const UNREAD: Stamp = Stamp {
    size: u64::MAX,
    modified: u64::MAX,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    Read(Stamp),
    /// Reading it kept failing. Every plan reads it again.
    Unread,
}

/// What the index holds for one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub content: Content,
    pub indexed: bool,
}

pub type Records = BTreeMap<String, Record>;

impl Record {
    fn from_row(entry: Entry) -> Self {
        let content = if entry.stamp == UNREAD {
            Content::Unread
        } else {
            Content::Read(entry.stamp)
        };
        Record {
            content,
            indexed: entry.indexed,
        }
    }

    fn row(self) -> Entry {
        let stamp = match self.content {
            Content::Read(stamp) => stamp,
            Content::Unread => UNREAD,
        };
        Entry {
            stamp,
            indexed: self.indexed,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Seen<'a> {
    All(&'a BTreeMap<String, Stamp>),
    Changes(&'a [Change]),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub to_read: Vec<(String, Stamp)>,
    /// Indexed files that are gone. Remove them from the index and the manifest.
    pub to_remove: Vec<String>,
    /// Unindexed files that are gone. Drop them from the manifest only.
    pub to_forget: Vec<String>,
}

impl Plan {
    fn present(&mut self, cached: &Records, key: &str, stamp: Stamp) {
        if cached
            .get(key)
            .is_none_or(|record| record.content != Content::Read(stamp))
        {
            self.to_read.push((key.to_owned(), stamp));
        }
    }

    fn vanished(&mut self, cached: &Records, key: &str) {
        match cached.get(key) {
            Some(record) if record.indexed => self.to_remove.push(key.to_owned()),
            Some(_) => self.to_forget.push(key.to_owned()),
            None => {}
        }
    }
}

pub fn plan(cached: &Records, seen: Seen) -> Plan {
    let mut plan = Plan::default();
    match seen {
        Seen::All(files) => {
            for (key, stamp) in files {
                plan.present(cached, key, *stamp);
            }
            for key in cached.keys().filter(|key| !files.contains_key(*key)) {
                plan.vanished(cached, key);
            }
        }
        Seen::Changes(changes) => {
            for change in changes {
                match change {
                    Change::Upsert(key, stamp) => plan.present(cached, key, *stamp),
                    Change::Remove(key) => plan.vanished(cached, key),
                }
            }
        }
    }
    plan
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub fingerprint: String,
    pub root: String,
    pub index: Stamp,
    pub cursor: Option<Cursor>,
}

pub fn serialize(header: &Header, manifest: &Manifest) -> String {
    let Header {
        fingerprint,
        root,
        index,
        cursor,
    } = header;
    let root = root.replace(['\n', '\r'], " ");
    let cursor = cursor.map_or("-".to_owned(), |cursor| {
        format!("{:016x} {}", cursor.epoch.0, cursor.generation.0)
    });
    let mut text = format!(
        "{FORMAT}\n{fingerprint}\nroot {root}\nindex {} {}\ncursor {cursor}\n",
        index.size, index.modified
    );
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
    pub header: Header,
    pub manifest: Manifest,
}

fn parse_index(line: &str) -> Option<Stamp> {
    let (size, modified) = line.strip_prefix("index ")?.split_once(' ')?;
    Some(Stamp {
        size: size.parse().ok()?,
        modified: modified.parse().ok()?,
    })
}

fn parse_cursor(line: &str) -> Option<Option<Cursor>> {
    let value = line.strip_prefix("cursor ")?;
    if value == "-" {
        return Some(None);
    }
    let (epoch, generation) = value.split_once(' ')?;
    Some(Some(Cursor {
        epoch: Epoch(u64::from_str_radix(epoch, 16).ok()?),
        generation: Gen(generation.parse().ok()?),
    }))
}

pub fn parse_manifest(text: &str) -> Option<Parsed> {
    let mut lines = text.lines();
    if lines.next()? != FORMAT {
        return None;
    }
    let fingerprint = lines.next()?.to_owned();
    let root = lines.next()?.strip_prefix("root ")?.to_owned();
    let index = parse_index(lines.next()?)?;
    let cursor = parse_cursor(lines.next()?)?;
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
        header: Header {
            fingerprint,
            root,
            index,
            cursor,
        },
        manifest,
    })
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
    entry_files(dir, &name, root)
}

pub fn entry_files(dir: &Path, name: &str, root: &Path) -> CacheFiles {
    CacheFiles {
        root: root.to_path_buf(),
        index: dir.join(format!("{name}.lux")),
        manifest: dir.join(format!("{name}.manifest")),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntry {
    pub name: String,
    pub header: Option<Header>,
    pub files: usize,
    pub bytes: u64,
    pub modified: Option<SystemTime>,
}

impl CacheEntry {
    pub fn root(&self) -> Option<&str> {
        self.header.as_ref().map(|header| header.root.as_str())
    }
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
                header: parsed.map(|parsed| parsed.header),
                bytes: slot.bytes,
                modified: slot.modified,
            }
        })
        .collect();
    entries.sort_by_cached_key(|entry| {
        (
            entry.root().is_none(),
            entry.root().unwrap_or_default().to_lowercase(),
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

pub struct Loaded {
    pub records: Records,
    pub cursor: Option<Cursor>,
}

/// Whether the pair a manifest header describes was saved with this library
/// and is whole on disk, checked without reading the index.
pub fn restorable(header: &Header, files: &CacheFiles, fingerprint: &str) -> bool {
    header.fingerprint == fingerprint
        && stamp_of(&files.index).is_some_and(|stamp| stamp == header.index)
}

pub fn restore(index: &mut LuxIndex, files: &CacheFiles, fingerprint: &str) -> Option<Parsed> {
    let text = fs::read_to_string(&files.manifest).ok()?;
    let parsed = parse_manifest(&text)?;
    if !restorable(&parsed.header, files, fingerprint) {
        return None;
    }
    index.restore(&files.index).ok()?;
    restorable(&parsed.header, files, fingerprint).then_some(parsed)
}

/// Restores a saved pair, or `None` when it is missing, from another library,
/// or torn. A torn pair costs a cold start, never a wrong index.
pub fn load(index: &mut LuxIndex, files: &CacheFiles, fingerprint: &str) -> Option<Loaded> {
    let Parsed { header, manifest } = restore(index, files, fingerprint)?;
    Some(Loaded {
        records: manifest
            .into_iter()
            .map(|(key, entry)| (key, Record::from_row(entry)))
            .collect(),
        cursor: header.cursor,
    })
}

/// Saves the `.lux` first, then a manifest that records which `.lux` it belongs to.
pub fn save(
    index: &LuxIndex,
    files: &CacheFiles,
    fingerprint: &str,
    cursor: Option<Cursor>,
    records: &Records,
) -> Result<(), Box<dyn Error>> {
    if let Some(dir) = files.index.parent() {
        fs::create_dir_all(dir)?;
    }
    let saved = write_atomic(&files.index, |path| index.save(path))?;
    let header = Header {
        fingerprint: fingerprint.to_owned(),
        root: files.root.to_string_lossy().into_owned(),
        index: saved,
        cursor,
    };
    let manifest: Manifest = records
        .iter()
        .map(|(key, record)| (key.clone(), record.row()))
        .collect();
    write_atomic(&files.manifest, |path| {
        Ok(fs::write(path, serialize(&header, &manifest))?)
    })?;
    Ok(())
}

fn stamp_of(path: &Path) -> Option<Stamp> {
    fs::metadata(path).ok().map(|metadata| Stamp::of(&metadata))
}

fn write_atomic(
    target: &Path,
    write: impl FnOnce(&Path) -> Result<(), Box<dyn Error>>,
) -> Result<Stamp, Box<dyn Error>> {
    let mut temp = target.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", process::id()));
    let temp = PathBuf::from(temp);
    let written = write(&temp).and_then(|()| {
        stamp_of(&temp).ok_or_else(|| format!("{} vanished after writing", temp.display()).into())
    });
    let stamp = match written {
        Ok(stamp) => stamp,
        Err(error) => {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
    };
    if let Err(error) = rename_over(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(stamp)
}

/// Access denied or a sharing violation: another handle has the file open.
fn is_busy(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(32)
}

/// Windows refuses to replace a file that a scanner or another Recall window
/// still has open. That lasts moments, so a few quick retries save most of them.
fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    let mut tries = 0;
    loop {
        match fs::rename(from, to) {
            Err(error) if is_busy(&error) && tries < 8 => {
                tries += 1;
                thread::sleep(Duration::from_millis(20 * tries));
            }
            done => return done,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lux;

    fn stamp(size: u64, modified: u64) -> Stamp {
        Stamp { size, modified }
    }

    fn scanned(entries: &[(&str, Stamp)]) -> BTreeMap<String, Stamp> {
        entries
            .iter()
            .map(|(key, stamp)| ((*key).to_owned(), *stamp))
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

    fn records(entries: &[(&str, Stamp, bool)]) -> Records {
        cached(entries)
            .into_iter()
            .map(|(key, entry)| (key, Record::from_row(entry)))
            .collect()
    }

    fn header(root: &str, cursor: Option<Cursor>) -> Header {
        Header {
            fingerprint: "fp".into(),
            root: root.into(),
            index: stamp(70, 123_456_789),
            cursor,
        }
    }

    #[test]
    fn manifest_round_trips_with_its_index_stat_and_cursor() {
        let manifest = cached(&[
            ("a/b c.md", stamp(5, 9), true),
            ("z.txt", stamp(1, 2), false),
        ]);
        let cursor = Cursor {
            epoch: Epoch(u64::MAX - 3),
            generation: Gen(41),
        };
        for header in [header("F:/my proj", Some(cursor)), header("F:/proj", None)] {
            let parsed = parse_manifest(&serialize(&header, &manifest)).unwrap();
            assert_eq!(parsed.header, header);
            assert_eq!(parsed.manifest, manifest);
        }
    }

    #[test]
    fn garbage_and_old_formats_do_not_parse() {
        let text = serialize(&header("F:/proj", None), &Manifest::new());
        assert!(parse_manifest(&text).is_some());
        for bad in [
            "junk".to_owned(),
            "recall-cache 1\nfp\n5\t9\t1\ta.md\n".to_owned(),
            "recall-cache 2\nfp\nroot F:/proj\n5\t9\t1\ta.md\n".to_owned(),
            text.replace("index 70 ", "index x "),
            text.replace("cursor -", "cursor 12"),
            format!("{text}x\ty\n"),
        ] {
            assert_eq!(parse_manifest(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_root_with_a_line_break_still_round_trips() {
        let text = serialize(&header("odd\nname", None), &Manifest::new());
        assert_eq!(parse_manifest(&text).unwrap().header.root, "odd name");
    }

    fn keys(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|key| (*key).to_owned()).collect()
    }

    #[test]
    fn plan_reads_new_and_changed_files_and_drops_every_vanished_one() {
        let old = records(&[
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
            plan(&old, Seen::All(&now)),
            Plan {
                to_read: vec![("changed".into(), stamp(2, 1)), ("new".into(), stamp(1, 1))],
                to_remove: keys(&["gone"]),
                to_forget: keys(&["skipped-gone"]),
            }
        );
    }

    #[test]
    fn plan_of_changes_touches_only_the_named_keys() {
        let old = records(&[
            ("same", stamp(1, 1), true),
            ("changed", stamp(1, 1), true),
            ("gone", stamp(1, 1), true),
            ("skipped-gone", stamp(1, 1), false),
            ("untouched", stamp(1, 1), true),
        ]);
        let changes = [
            Change::Upsert("same".into(), stamp(1, 1)),
            Change::Upsert("changed".into(), stamp(2, 1)),
            Change::Upsert("new".into(), stamp(1, 1)),
            Change::Remove("gone".into()),
            Change::Remove("skipped-gone".into()),
            Change::Remove("never-seen".into()),
        ];
        assert_eq!(
            plan(&old, Seen::Changes(&changes)),
            Plan {
                to_read: vec![("changed".into(), stamp(2, 1)), ("new".into(), stamp(1, 1))],
                to_remove: keys(&["gone"]),
                to_forget: keys(&["skipped-gone"]),
            }
        );
    }

    #[test]
    fn plan_always_reads_an_unread_file_again() {
        let old = Records::from([(
            "locked".to_owned(),
            Record {
                content: Content::Unread,
                indexed: true,
            },
        )]);
        let now = scanned(&[("locked", stamp(5, 5))]);
        assert_eq!(
            plan(&old, Seen::All(&now)).to_read,
            [("locked".into(), stamp(5, 5))]
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

        fn src(&self) -> PathBuf {
            self.0.join("src")
        }

        fn scan(&self) -> BTreeMap<String, Stamp> {
            fs::read_dir(self.src())
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    let key = entry.file_name().to_string_lossy().into_owned();
                    (key, Stamp::of(&entry.metadata().unwrap()))
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

    fn run(workspace: &Workspace, fingerprint: &str) -> LuxIndex {
        run_in(workspace, &workspace.0.join("cache"), fingerprint)
    }

    fn run_in(workspace: &Workspace, dir: &Path, fingerprint: &str) -> LuxIndex {
        let mut index = LuxIndex::load(&lux::library_path()).expect("build libluxcore");
        let mut records = Records::new();
        for (key, stamp) in workspace.scan() {
            let body = fs::read_to_string(workspace.src().join(&key));
            if let Ok(body) = &body {
                index.add(&key, &key, body).unwrap();
            }
            records.insert(
                key,
                Record {
                    content: Content::Read(stamp),
                    indexed: body.is_ok(),
                },
            );
        }
        let files = cache_files(dir, &workspace.0);
        save(&index, &files, fingerprint, None, &records).unwrap();
        index
    }

    fn reusable(workspace: &Workspace, dir: &Path, fingerprint: &str) -> bool {
        let mut index = LuxIndex::load(&lux::library_path()).expect("build libluxcore");
        load(&mut index, &cache_files(dir, &workspace.0), fingerprint).is_some()
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
    fn a_saved_workspace_keeps_its_root_in_the_manifest() {
        let workspace = Workspace::new("root");
        workspace.write("a.md", b"alpha");
        run(&workspace, "fp");
        let text = fs::read_to_string(workspace.files().manifest).unwrap();
        let parsed = parse_manifest(&text).unwrap();
        assert_eq!(parsed.header.root, workspace.0.to_string_lossy());
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
                serialize(&header(root, None), &manifest),
            )
            .unwrap();
        }
    }

    #[test]
    fn listing_shows_the_root_file_count_and_size_of_a_saved_folder() {
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
        assert_eq!(entry.root(), workspace.0.to_str());
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
            .map(|entry| (entry.name.as_str(), entry.root(), entry.files))
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
    fn a_deleted_entry_is_gone_until_the_folder_is_saved_again() {
        let home = Workspace::new("delete-resync");
        let dir = home.0.join("cache");
        let workspace = Workspace::new("delete-resync-src");
        workspace.write("a.md", b"alpha");
        assert!(!reusable(&workspace, &dir, "fp"));
        run_in(&workspace, &dir, "fp");
        assert!(reusable(&workspace, &dir, "fp"));

        delete_entry(&dir, &stem_of(&workspace, &dir)).unwrap();
        assert!(list_entries(&dir).is_empty());
        assert!(!reusable(&workspace, &dir, "fp"));

        let index = run_in(&workspace, &dir, "fp");
        assert_eq!(found(&index, "alpha"), ["a.md"]);
        assert!(reusable(&workspace, &dir, "fp"));
    }

    #[test]
    fn a_lux_replaced_after_its_manifest_was_written_means_a_cold_start() {
        let workspace = Workspace::new("torn");
        workspace.write("a.md", b"alpha");
        let dir = workspace.0.join("cache");
        let index = run(&workspace, "fp");
        assert!(reusable(&workspace, &dir, "fp"));
        index.save(&workspace.files().index).unwrap();
        assert!(!reusable(&workspace, &dir, "fp"));
    }

    #[test]
    fn a_saved_cursor_comes_back_with_its_index() {
        let workspace = Workspace::new("cursor");
        workspace.write("a.md", b"alpha");
        let index = run(&workspace, "fp");
        let files = workspace.files();
        let cursor = Cursor {
            epoch: Epoch(7),
            generation: Gen(3),
        };
        let records = records(&[("a.md", stamp(5, 1), true)]);
        save(&index, &files, "fp", Some(cursor), &records).unwrap();

        let mut restored = LuxIndex::load(&lux::library_path()).unwrap();
        let loaded = load(&mut restored, &files, "fp").unwrap();
        assert_eq!(loaded.cursor, Some(cursor));
        assert_eq!(loaded.records, records);
        assert_eq!(found(&restored, "alpha"), ["a.md"]);
        assert!(load(&mut restored, &files, "other library").is_none());
    }

    #[test]
    fn an_unread_file_keeps_its_row_shape_on_disk_and_comes_back_unread() {
        let workspace = Workspace::new("unread");
        workspace.write("a.md", b"alpha");
        let index = run(&workspace, "fp");
        let files = workspace.files();
        let unread = Record {
            content: Content::Unread,
            indexed: false,
        };
        let records = Records::from([("a.md".to_owned(), unread)]);
        save(&index, &files, "fp", None, &records).unwrap();

        let text = fs::read_to_string(&files.manifest).unwrap();
        assert!(text.starts_with("recall-cache 3\n"));
        assert!(
            text.ends_with("\n18446744073709551615\t18446744073709551615\t0\ta.md\n"),
            "{text}"
        );
        let mut restored = LuxIndex::load(&lux::library_path()).unwrap();
        assert_eq!(load(&mut restored, &files, "fp").unwrap().records, records);
    }
}
