use std::{
    error::Error,
    ffi::{c_char, c_void},
    marker::PhantomData,
    path::Path,
    ptr::{self, NonNull},
    sync::Arc,
};

use libloading::Library;

pub const DEFAULT_LIBRARY: &str = "../LuxCore/build/release/libluxcore.dll";
pub const MARK_START: char = '\u{E000}';
pub const MARK_END: char = '\u{E001}';

// These signatures mirror the stable, flat C API in luxcore_c.h.
type IndexCreate = unsafe extern "C" fn(*mut *mut c_void) -> i32;
type IndexFree = unsafe extern "C" fn(*mut c_void);
type IndexSearch =
    unsafe extern "C" fn(*const c_void, *const c_char, usize, usize, *mut *mut c_void) -> i32;
type IndexRemove = unsafe extern "C" fn(*mut c_void, *const c_char, usize, *mut i32) -> i32;
type IndexSave = unsafe extern "C" fn(*const c_void, *const c_char, usize) -> i32;
type IndexLoad = unsafe extern "C" fn(*const c_char, usize, *mut *mut c_void) -> i32;
type IndexPrepare = unsafe extern "C" fn(
    *const c_void,
    *const c_char,
    usize,
    *const c_char,
    usize,
    u32,
    *mut *mut c_void,
) -> i32;
type IndexCommit = unsafe extern "C" fn(*mut c_void, *const c_char, usize, *mut c_void) -> i32;
type PreparedFree = unsafe extern "C" fn(*mut c_void);
#[repr(C)]
struct SnippetOptions {
    length: usize,
    start_marker: *const c_char,
    start_marker_length: usize,
    end_marker: *const c_char,
    end_marker_length: usize,
    ellipsis: *const c_char,
    ellipsis_length: usize,
}
type IndexSnippetText = unsafe extern "C" fn(
    *const c_void,
    *const c_char,
    usize,
    *const c_char,
    usize,
    *const SnippetOptions,
    *mut *mut c_void,
) -> i32;
type IndexSearchQuery = unsafe extern "C" fn(
    *const c_void,
    *const c_char,
    usize,
    usize,
    *mut *mut c_void,
    *mut usize,
) -> i32;
type IndexSnippetTextQuery = unsafe extern "C" fn(
    *const c_void,
    *const c_char,
    usize,
    *const c_char,
    usize,
    *const SnippetOptions,
    *mut *mut c_void,
    *mut usize,
) -> i32;
type IndexSuggest =
    unsafe extern "C" fn(*const c_void, *const c_char, usize, usize, *mut *mut c_void) -> i32;
type StringsCount = unsafe extern "C" fn(*const c_void) -> usize;
type StringsGet = unsafe extern "C" fn(*const c_void, usize, *mut usize) -> *const c_char;
type StringsFree = unsafe extern "C" fn(*mut c_void);
type HitsCount = unsafe extern "C" fn(*const c_void) -> usize;
type HitsKey = unsafe extern "C" fn(*const c_void, usize, *mut usize) -> *const c_char;
type HitsScore = unsafe extern "C" fn(*const c_void, usize) -> f64;
type HitsFree = unsafe extern "C" fn(*mut c_void);
type TextData = unsafe extern "C" fn(*const c_void) -> *const c_char;
type TextLength = unsafe extern "C" fn(*const c_void) -> usize;
type TextFree = unsafe extern "C" fn(*mut c_void);

struct LuxFns {
    create: IndexCreate,
    free: IndexFree,
    prepare: IndexPrepare,
    commit: IndexCommit,
    prepared_free: PreparedFree,
    search: IndexSearch,
    remove: IndexRemove,
    save: IndexSave,
    load: IndexLoad,
    snippet_text: IndexSnippetText,
    search_query: IndexSearchQuery,
    snippet_text_query: IndexSnippetTextQuery,
    suggest: IndexSuggest,
    strings_count: StringsCount,
    strings_get: StringsGet,
    strings_free: StringsFree,
    hits_count: HitsCount,
    hits_key: HitsKey,
    hits_score: HitsScore,
    hits_free: HitsFree,
    text_data: TextData,
    text_length: TextLength,
    text_free: TextFree,
}

/// One loaded LuxCore library. Whatever calls into it holds an `Arc` of it.
struct Lib {
    fns: LuxFns,
    library: Library,
}

/// One `lux_index`, freed when its last holder drops.
struct Handle {
    raw: NonNull<c_void>,
    lib: Arc<Lib>,
}

// The threading contract.
//
// `LuxIndex` is neither `Send` nor `Sync`, so the owning thread alone calls
// commit, remove, save, load, search, suggest and the snippet functions. Other
// threads get a `Handle` only inside a `Preparer` or a `Prepared`, and those
// make two calls: `lux_index_prepare` and `lux_prepared_free`.
//
// SAFETY, `Send` and `Sync` for `Handle`: luxcore_c.h says prepare is "safe to
// call from many threads at once, alongside searches and commits on the same
// index". LuxCore's prepare takes no lock and reads only the analyzer and the
// positions setting, which never change after the index is created. Its phase
// clock is atomic. So it is equally safe alongside the owner's remove and save.
// The raw index pointer is never freed while an `Arc<Handle>` exists, and the
// `Arc<Lib>` in each handle keeps the library loaded until the last handle,
// and so the last `Prepared`, is gone. `lux_index_free` may therefore run on
// any thread, and never while a call is in flight.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { (self.lib.fns.free)(self.raw.as_ptr()) };
    }
}

pub struct LuxIndex {
    handle: Arc<Handle>,
    /// Not `Send` or `Sync`: every call but prepare stays on the owning thread.
    _owner: PhantomData<*mut ()>,
}

/// Tokenizes documents for one index, from any thread. It can do nothing else
/// to the index, and keeps the index alive.
#[derive(Clone)]
pub struct Preparer {
    handle: Arc<Handle>,
}

/// A tokenized document waiting for `LuxIndex::commit`. Dropping it frees it.
pub struct Prepared {
    raw: Option<NonNull<c_void>>,
    handle: Arc<Handle>,
}

// SAFETY, `Send` for `Prepared` (it is not `Sync`): a `lux_prepared` is a
// heap value that owns its strings and vectors and points into neither the
// index nor a thread, so one thread at a time may own it and any thread may
// commit or free it. It is freed exactly once. `LuxIndex::commit` takes `raw`
// out first and LuxCore takes ownership of it whatever commit returns, so
// `Drop` finds `None`. Without a commit, `Drop` frees it. It keeps its index
// and library alive through `handle`, so neither can be freed under it.
unsafe impl Send for Prepared {}

impl Drop for Prepared {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            unsafe { (self.handle.lib.fns.prepared_free)(raw.as_ptr()) };
        }
    }
}

impl Preparer {
    /// The body is indexed but not stored.
    pub fn prepare(&self, title: &str, body: &str) -> Result<Prepared, Box<dyn Error>> {
        const INDEX_ONLY: u32 = 0;
        let mut raw = ptr::null_mut();
        let status = unsafe {
            (self.handle.lib.fns.prepare)(
                self.handle.raw.as_ptr(),
                title.as_ptr().cast(),
                title.len(),
                body.as_ptr().cast(),
                body.len(),
                INDEX_ONLY,
                &mut raw,
            )
        };
        check_status(status, "reading a file's words")?;
        let raw = NonNull::new(raw).ok_or("LuxCore returned a null prepared document")?;
        Ok(Prepared {
            raw: Some(raw),
            handle: Arc::clone(&self.handle),
        })
    }
}

fn load_library(path: &Path) -> Result<Lib, Box<dyn Error>> {
    // The library is retained for the lifetime of every function pointer and index handle.
    let library = unsafe { Library::new(path)? };
    let too_old = |_| {
        format!(
            "The LuxCore library at {} is older than this Recall needs. Rebuild LuxCore with `make shared MODE=release`.",
            path.display()
        )
    };
    let fns = unsafe {
        LuxFns {
            create: *library.get(b"lux_index_create\0")?,
            free: *library.get(b"lux_index_free\0")?,
            prepare: *library.get(b"lux_index_prepare\0").map_err(too_old)?,
            commit: *library.get(b"lux_index_commit\0").map_err(too_old)?,
            prepared_free: *library.get(b"lux_prepared_free\0").map_err(too_old)?,
            search: *library.get(b"lux_index_search\0")?,
            remove: *library.get(b"lux_index_remove\0")?,
            save: *library.get(b"lux_index_save\0")?,
            load: *library.get(b"lux_index_load\0")?,
            snippet_text: *library.get(b"lux_index_snippet_text\0").map_err(too_old)?,
            search_query: *library.get(b"lux_index_search_query\0")?,
            snippet_text_query: *library
                .get(b"lux_index_snippet_text_query\0")
                .map_err(too_old)?,
            suggest: *library.get(b"lux_index_suggest\0")?,
            strings_count: *library.get(b"lux_strings_count\0")?,
            strings_get: *library.get(b"lux_strings_get\0")?,
            strings_free: *library.get(b"lux_strings_free\0")?,
            hits_count: *library.get(b"lux_hits_count\0")?,
            hits_key: *library.get(b"lux_hits_key\0")?,
            hits_score: *library.get(b"lux_hits_score\0")?,
            hits_free: *library.get(b"lux_hits_free\0")?,
            text_data: *library.get(b"lux_text_data\0")?,
            text_length: *library.get(b"lux_text_length\0")?,
            text_free: *library.get(b"lux_text_free\0")?,
        }
    };
    Ok(Lib { fns, library })
}

impl LuxIndex {
    pub fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        let lib = Arc::new(load_library(path)?);
        let mut raw = ptr::null_mut();
        let status = unsafe { (lib.fns.create)(&mut raw) };
        check_status(status, "creating the LuxCore index")?;
        Ok(Self::owning(raw, lib)?)
    }

    fn owning(raw: *mut c_void, lib: Arc<Lib>) -> Result<Self, &'static str> {
        let raw = NonNull::new(raw).ok_or("LuxCore returned a null index")?;
        Ok(Self {
            handle: Arc::new(Handle { raw, lib }),
            _owner: PhantomData,
        })
    }

    fn fns(&self) -> &LuxFns {
        &self.handle.lib.fns
    }

    fn raw(&self) -> *mut c_void {
        self.handle.raw.as_ptr()
    }

    pub fn preparer(&self) -> Preparer {
        Preparer {
            handle: Arc::clone(&self.handle),
        }
    }

    /// Adds the document, or replaces the one `key` names.
    pub fn commit(&mut self, key: &str, mut prepared: Prepared) -> Result<(), Box<dyn Error>> {
        if !Arc::ptr_eq(&prepared.handle, &self.handle) {
            return Err("the document was prepared for another index".into());
        }
        let Some(raw) = prepared.raw.take() else {
            return Err("the prepared document was already committed".into());
        };
        // LuxCore frees the document whatever it returns.
        let status = unsafe {
            (self.fns().commit)(self.raw(), key.as_ptr().cast(), key.len(), raw.as_ptr())
        };
        check_status(status, "adding a file")
    }

    #[cfg(test)]
    pub fn add(&mut self, key: &str, title: &str, body: &str) -> Result<(), Box<dyn Error>> {
        let prepared = self.preparer().prepare(title, body)?;
        self.commit(key, prepared)
    }

    pub fn remove(&mut self, key: &str) -> Result<bool, Box<dyn Error>> {
        let mut removed = 0;
        let status = unsafe {
            (self.fns().remove)(self.raw(), key.as_ptr().cast(), key.len(), &mut removed)
        };
        check_status(status, "removing a file")?;
        Ok(removed != 0)
    }

    pub fn save(&self, path: &Path) -> Result<(), Box<dyn Error>> {
        let path = path.to_str().ok_or("cache path is not valid UTF-8")?;
        let status = unsafe { (self.fns().save)(self.raw(), path.as_ptr().cast(), path.len()) };
        check_status(status, "saving the index")
    }

    /// Replaces this index with a saved one. A `Preparer` made before this
    /// still belongs to the old index, and `commit` refuses its documents.
    pub fn restore(&mut self, path: &Path) -> Result<(), Box<dyn Error>> {
        let path = path.to_str().ok_or("cache path is not valid UTF-8")?;
        let mut loaded = ptr::null_mut();
        let status = unsafe { (self.fns().load)(path.as_ptr().cast(), path.len(), &mut loaded) };
        check_status(status, "loading the cached index")?;
        *self = Self::owning(loaded, Arc::clone(&self.handle.lib))?;
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, Box<dyn Error>> {
        let mut hits = ptr::null_mut();
        let status = unsafe {
            (self.fns().search)(
                self.raw(),
                query.as_ptr().cast(),
                query.len(),
                limit,
                &mut hits,
            )
        };
        check_status(status, "searching")?;
        self.take_hits(hits)
    }

    pub fn search_query(&self, query: &str, limit: usize) -> Result<Vec<Hit>, QueryFailure> {
        let mut hits = ptr::null_mut();
        let mut offset = 0;
        let status = unsafe {
            (self.fns().search_query)(
                self.raw(),
                query.as_ptr().cast(),
                query.len(),
                limit,
                &mut hits,
                &mut offset,
            )
        };
        check_query(status, offset, "searching")?;
        Ok(self.take_hits(hits)?)
    }

    pub fn suggest(&self, query: &str, limit: usize) -> Result<Vec<String>, Box<dyn Error>> {
        let mut strings = ptr::null_mut();
        let status = unsafe {
            (self.fns().suggest)(
                self.raw(),
                query.as_ptr().cast(),
                query.len(),
                limit,
                &mut strings,
            )
        };
        check_status(status, "suggesting completions")?;
        if strings.is_null() {
            return Err("LuxCore returned a null suggestion handle".into());
        }
        let count = unsafe { (self.fns().strings_count)(strings) };
        let mut result = Vec::with_capacity(count);
        for position in 0..count {
            let mut length = 0;
            let data = unsafe { (self.fns().strings_get)(strings, position, &mut length) };
            if data.is_null() {
                continue;
            }
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) };
            result.push(String::from_utf8_lossy(bytes).into_owned());
        }
        unsafe { (self.fns().strings_free)(strings) };
        Ok(result)
    }

    fn take_hits(&self, hits: *mut c_void) -> Result<Vec<Hit>, Box<dyn Error>> {
        if hits.is_null() {
            return Err("LuxCore returned a null result handle".into());
        }
        let count = unsafe { (self.fns().hits_count)(hits) };
        let mut result = Vec::with_capacity(count);
        for position in 0..count {
            let mut key_len = 0;
            let key = unsafe { (self.fns().hits_key)(hits, position, &mut key_len) };
            if key.is_null() {
                unsafe { (self.fns().hits_free)(hits) };
                return Err("LuxCore returned a null result key".into());
            }
            let key = unsafe { std::slice::from_raw_parts(key.cast::<u8>(), key_len) };
            let key = String::from_utf8_lossy(key).into_owned();
            let score = unsafe { (self.fns().hits_score)(hits, position) };
            result.push(Hit { key, score });
        }
        unsafe { (self.fns().hits_free)(hits) };
        Ok(result)
    }

    /// `text` with the words a fuzzy search for `query` matches marked.
    pub fn snippet_text(
        &self,
        text: &str,
        query: &str,
        length: usize,
    ) -> Result<String, Box<dyn Error>> {
        let mut marked = ptr::null_mut();
        let status = with_markers(length, |options| unsafe {
            (self.fns().snippet_text)(
                self.raw(),
                text.as_ptr().cast(),
                text.len(),
                query.as_ptr().cast(),
                query.len(),
                options,
                &mut marked,
            )
        });
        check_status(status, "building a snippet")?;
        self.take_text(marked)
    }

    /// `text` with the words the syntax query `query` matches marked.
    pub fn snippet_text_query(
        &self,
        text: &str,
        query: &str,
        length: usize,
    ) -> Result<String, QueryFailure> {
        let mut marked = ptr::null_mut();
        let mut offset = 0;
        let status = with_markers(length, |options| unsafe {
            (self.fns().snippet_text_query)(
                self.raw(),
                text.as_ptr().cast(),
                text.len(),
                query.as_ptr().cast(),
                query.len(),
                options,
                &mut marked,
                &mut offset,
            )
        });
        check_query(status, offset, "building a snippet")?;
        Ok(self.take_text(marked)?)
    }

    /// LuxCore's memory and phase-timer report as JSON, from a build that exports it.
    pub fn debug_stats(&self) -> Option<String> {
        type DebugStats = unsafe extern "C" fn(*const c_void, *mut *mut c_void) -> i32;
        let library = &self.handle.lib.library;
        let stats = unsafe { library.get::<DebugStats>(b"lux_index_debug_stats\0") }.ok()?;
        let mut text = ptr::null_mut();
        if unsafe { stats(self.raw(), &mut text) } != 0 {
            return None;
        }
        self.take_text(text).ok()
    }

    fn take_text(&self, text: *mut c_void) -> Result<String, Box<dyn Error>> {
        if text.is_null() {
            return Err("LuxCore returned a null text handle".into());
        }
        let length = unsafe { (self.fns().text_length)(text) };
        let data = unsafe { (self.fns().text_data)(text) };
        let value = if length == 0 {
            String::new()
        } else if data.is_null() {
            unsafe { (self.fns().text_free)(text) };
            return Err("LuxCore returned null text data".into());
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        unsafe { (self.fns().text_free)(text) };
        Ok(value)
    }
}

fn check_status(status: i32, action: &str) -> Result<(), Box<dyn Error>> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("LuxCore error {status} while {action}").into())
    }
}

const INVALID_QUERY: i32 = 7;

#[derive(Debug)]
pub enum QueryFailure {
    Invalid { offset: usize },
    Other(Box<dyn Error>),
}

impl std::fmt::Display for QueryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryFailure::Invalid { offset } => write!(f, "invalid query at byte {offset}"),
            QueryFailure::Other(error) => error.fmt(f),
        }
    }
}

impl Error for QueryFailure {}

impl From<Box<dyn Error>> for QueryFailure {
    fn from(error: Box<dyn Error>) -> Self {
        QueryFailure::Other(error)
    }
}

fn check_query(status: i32, offset: usize, action: &str) -> Result<(), QueryFailure> {
    if status == INVALID_QUERY {
        return Err(QueryFailure::Invalid { offset });
    }
    Ok(check_status(status, action)?)
}

fn with_markers(length: usize, call: impl FnOnce(&SnippetOptions) -> i32) -> i32 {
    let mut start = [0u8; 4];
    let mut end = [0u8; 4];
    let start = MARK_START.encode_utf8(&mut start);
    let end = MARK_END.encode_utf8(&mut end);
    call(&SnippetOptions {
        length,
        start_marker: start.as_ptr().cast(),
        start_marker_length: start.len(),
        end_marker: end.as_ptr().cast(),
        end_marker_length: end.len(),
        ellipsis: start.as_ptr().cast(),
        ellipsis_length: 0,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub key: String,
    pub score: f64,
}

#[cfg(test)]
pub fn library_path() -> std::path::PathBuf {
    std::env::var_os("LUXCORE_LIB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_LIBRARY))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notes() -> LuxIndex {
        let mut index = LuxIndex::load(&library_path()).expect("build libluxcore");
        index
            .add("a.md", "a.md", "the watcher debounces bursts of changes")
            .unwrap();
        index
            .add("b.md", "b.md", "a debt is paid down by the watcher")
            .unwrap();
        index
    }

    fn keys(hits: Vec<Hit>) -> Vec<String> {
        hits.into_iter().map(|hit| hit.key).collect()
    }

    #[test]
    fn documents_tokenized_on_other_threads_commit_on_the_owner() {
        let mut index = LuxIndex::load(&library_path()).expect("build libluxcore");
        let preparer = index.preparer();
        let made: Vec<(String, Prepared)> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|n| {
                    let preparer = preparer.clone();
                    scope.spawn(move || {
                        let body = format!("shared words and uniquetoken{n}");
                        (format!("k{n}.md"), preparer.prepare("t", &body).unwrap())
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect()
        });
        for (key, prepared) in made {
            index.commit(&key, prepared).unwrap();
        }
        assert_eq!(keys(index.search("uniquetoken5", 10).unwrap()), ["k5.md"]);
        assert_eq!(index.search("shared", 20).unwrap().len(), 8);
    }

    #[test]
    fn a_prepared_document_never_committed_changes_nothing_and_may_outlive_its_index() {
        let index = notes();
        let stray = index.preparer().prepare("t", "neverindexed words").unwrap();
        assert!(index.search("neverindexed", 5).unwrap().is_empty());
        drop(index);
        drop(stray);
    }

    #[test]
    fn a_document_prepared_for_another_index_is_refused() {
        let mut mine = notes();
        let other = notes();
        let foreign = other.preparer().prepare("t", "foreigntoken").unwrap();
        assert!(mine.commit("x.md", foreign).is_err());
        assert!(mine.search("foreigntoken", 5).unwrap().is_empty());
        assert!(other.search("foreigntoken", 5).unwrap().is_empty());
    }

    #[test]
    fn a_restored_index_refuses_documents_prepared_before_the_restore() {
        let dir = std::env::temp_dir().join(format!("recall-lux-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let saved = dir.join("saved.lux");
        let mut index = notes();
        index.save(&saved).unwrap();
        let before = index.preparer().prepare("t", "staletoken").unwrap();
        index.restore(&saved).unwrap();
        assert!(index.commit("s.md", before).is_err());
        assert!(index.search("staletoken", 5).unwrap().is_empty());
        assert_eq!(keys(index.search("watcher", 5).unwrap()).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggest_completes_the_unfinished_last_word() {
        let words = notes().suggest("watcher deb", 5).unwrap();
        assert!(words.iter().any(|word| word == "debt"), "{words:?}");
        assert!(
            words.iter().all(|word| word.starts_with("deb")),
            "{words:?}"
        );
    }

    #[test]
    fn query_syntax_requires_and_excludes_words() {
        let hits = notes().search_query("+watcher -debt", 5).unwrap();
        let keys: Vec<&str> = hits.iter().map(|hit| hit.key.as_str()).collect();
        assert_eq!(keys, ["a.md"]);
    }

    #[test]
    fn a_malformed_query_reports_the_byte_luxcore_rejected() {
        let index = notes();
        let failure = index.search_query("watcher AND (debt", 5).unwrap_err();
        assert!(
            matches!(failure, QueryFailure::Invalid { offset: 12 }),
            "{failure:?}"
        );
        let failure = index.snippet_text_query("a debt", "(debt", 80).unwrap_err();
        assert!(
            matches!(failure, QueryFailure::Invalid { offset: 0 }),
            "{failure:?}"
        );
    }

    #[test]
    fn snippet_query_marks_the_words_of_the_query() {
        let marked = notes()
            .snippet_text_query("a debt is paid down", "debt", 80)
            .unwrap();
        assert!(
            marked.contains(&format!("{MARK_START}debt{MARK_END}")),
            "{marked:?}"
        );
    }
}
