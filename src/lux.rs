use std::{
    error::Error,
    ffi::{c_char, c_void},
    path::Path,
    ptr,
};

use libloading::Library;

pub const DEFAULT_LIBRARY: &str = "../LuxCore/build/release/libluxcore.dll";
pub const MARK_START: char = '\u{E000}';
pub const MARK_END: char = '\u{E001}';

// These signatures mirror the stable, flat C API in luxcore_c.h.
type IndexCreate = unsafe extern "C" fn(*mut *mut c_void) -> i32;
type IndexFree = unsafe extern "C" fn(*mut c_void);
type IndexAdd = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    usize,
    *const c_char,
    usize,
    *const c_char,
    usize,
) -> i32;
type IndexSearch =
    unsafe extern "C" fn(*const c_void, *const c_char, usize, usize, *mut *mut c_void) -> i32;
type IndexRemove = unsafe extern "C" fn(*mut c_void, *const c_char, usize, *mut i32) -> i32;
type IndexSave = unsafe extern "C" fn(*const c_void, *const c_char, usize) -> i32;
type IndexLoad = unsafe extern "C" fn(*const c_char, usize, *mut *mut c_void) -> i32;
type IndexStored = unsafe extern "C" fn(
    *const c_void,
    *const c_char,
    usize,
    *const c_char,
    usize,
    *mut *mut c_void,
) -> i32;
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
type IndexSnippet = unsafe extern "C" fn(
    *const c_void,
    *const c_char,
    usize,
    *const c_char,
    usize,
    *const c_char,
    usize,
    *const SnippetOptions,
    *mut *mut c_void,
) -> i32;
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
    add: IndexAdd,
    search: IndexSearch,
    stored: IndexStored,
    remove: IndexRemove,
    save: IndexSave,
    load: IndexLoad,
    snippet: IndexSnippet,
    hits_count: HitsCount,
    hits_key: HitsKey,
    hits_score: HitsScore,
    hits_free: HitsFree,
    text_data: TextData,
    text_length: TextLength,
    text_free: TextFree,
}

pub struct LuxIndex {
    raw: *mut c_void,
    _library: Library,
    fns: LuxFns,
}

impl LuxIndex {
    pub fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        // The library is retained for the lifetime of every function pointer and index handle.
        let library = unsafe { Library::new(path)? };
        let fns = unsafe {
            LuxFns {
                create: *library.get(b"lux_index_create\0")?,
                free: *library.get(b"lux_index_free\0")?,
                add: *library.get(b"lux_index_add\0")?,
                search: *library.get(b"lux_index_search\0")?,
                stored: *library.get(b"lux_index_stored\0")?,
                remove: *library.get(b"lux_index_remove\0")?,
                save: *library.get(b"lux_index_save\0")?,
                load: *library.get(b"lux_index_load\0")?,
                snippet: *library.get(b"lux_index_snippet\0")?,
                hits_count: *library.get(b"lux_hits_count\0")?,
                hits_key: *library.get(b"lux_hits_key\0")?,
                hits_score: *library.get(b"lux_hits_score\0")?,
                hits_free: *library.get(b"lux_hits_free\0")?,
                text_data: *library.get(b"lux_text_data\0")?,
                text_length: *library.get(b"lux_text_length\0")?,
                text_free: *library.get(b"lux_text_free\0")?,
            }
        };
        let mut raw = ptr::null_mut();
        let status = unsafe { (fns.create)(&mut raw) };
        check_status(status, "creating the LuxCore index")?;
        if raw.is_null() {
            return Err("LuxCore returned a null index".into());
        }
        Ok(Self {
            raw,
            _library: library,
            fns,
        })
    }

    pub fn remove(&mut self, key: &str) -> Result<bool, Box<dyn Error>> {
        let mut removed = 0;
        let status =
            unsafe { (self.fns.remove)(self.raw, key.as_ptr().cast(), key.len(), &mut removed) };
        check_status(status, "removing a file")?;
        Ok(removed != 0)
    }

    pub fn save(&self, path: &Path) -> Result<(), Box<dyn Error>> {
        let path = path.to_str().ok_or("cache path is not valid UTF-8")?;
        let status = unsafe { (self.fns.save)(self.raw, path.as_ptr().cast(), path.len()) };
        check_status(status, "saving the index")
    }

    pub fn restore(&mut self, path: &Path) -> Result<(), Box<dyn Error>> {
        let path = path.to_str().ok_or("cache path is not valid UTF-8")?;
        let mut loaded = ptr::null_mut();
        let status = unsafe { (self.fns.load)(path.as_ptr().cast(), path.len(), &mut loaded) };
        check_status(status, "loading the cached index")?;
        if loaded.is_null() {
            return Err("LuxCore returned a null index".into());
        }
        unsafe { (self.fns.free)(self.raw) };
        self.raw = loaded;
        Ok(())
    }

    pub fn add(&mut self, key: &str, title: &str, body: &str) -> Result<(), Box<dyn Error>> {
        let status = unsafe {
            (self.fns.add)(
                self.raw,
                key.as_ptr().cast(),
                key.len(),
                title.as_ptr().cast(),
                title.len(),
                body.as_ptr().cast(),
                body.len(),
            )
        };
        check_status(status, "adding a file")
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, Box<dyn Error>> {
        let mut hits = ptr::null_mut();
        let status = unsafe {
            (self.fns.search)(
                self.raw,
                query.as_ptr().cast(),
                query.len(),
                limit,
                &mut hits,
            )
        };
        check_status(status, "searching")?;
        if hits.is_null() {
            return Err("LuxCore returned a null result handle".into());
        }
        let count = unsafe { (self.fns.hits_count)(hits) };
        let mut result = Vec::with_capacity(count);
        for position in 0..count {
            let mut key_len = 0;
            let key = unsafe { (self.fns.hits_key)(hits, position, &mut key_len) };
            if key.is_null() {
                unsafe { (self.fns.hits_free)(hits) };
                return Err("LuxCore returned a null result key".into());
            }
            let key = unsafe { std::slice::from_raw_parts(key.cast::<u8>(), key_len) };
            let key = String::from_utf8_lossy(key).into_owned();
            let score = unsafe { (self.fns.hits_score)(hits, position) };
            result.push(Hit { key, score });
        }
        unsafe { (self.fns.hits_free)(hits) };
        Ok(result)
    }

    pub fn stored(&self, key: &str, field: &str) -> Result<String, Box<dyn Error>> {
        let mut text = ptr::null_mut();
        let status = unsafe {
            (self.fns.stored)(
                self.raw,
                key.as_ptr().cast(),
                key.len(),
                field.as_ptr().cast(),
                field.len(),
                &mut text,
            )
        };
        check_status(status, "reading a stored field")?;
        self.take_text(text)
    }

    pub fn snippet(
        &self,
        key: &str,
        field: &str,
        query: &str,
        length: usize,
    ) -> Result<String, Box<dyn Error>> {
        let mut start = [0u8; 4];
        let mut end = [0u8; 4];
        let start = MARK_START.encode_utf8(&mut start);
        let end = MARK_END.encode_utf8(&mut end);
        let options = SnippetOptions {
            length,
            start_marker: start.as_ptr().cast(),
            start_marker_length: start.len(),
            end_marker: end.as_ptr().cast(),
            end_marker_length: end.len(),
            ellipsis: start.as_ptr().cast(),
            ellipsis_length: 0,
        };
        let mut text = ptr::null_mut();
        let status = unsafe {
            (self.fns.snippet)(
                self.raw,
                key.as_ptr().cast(),
                key.len(),
                field.as_ptr().cast(),
                field.len(),
                query.as_ptr().cast(),
                query.len(),
                &options,
                &mut text,
            )
        };
        check_status(status, "building a snippet")?;
        self.take_text(text)
    }

    fn take_text(&self, text: *mut c_void) -> Result<String, Box<dyn Error>> {
        if text.is_null() {
            return Err("LuxCore returned a null text handle".into());
        }
        let length = unsafe { (self.fns.text_length)(text) };
        let data = unsafe { (self.fns.text_data)(text) };
        let value = if length == 0 {
            String::new()
        } else if data.is_null() {
            unsafe { (self.fns.text_free)(text) };
            return Err("LuxCore returned null text data".into());
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        unsafe { (self.fns.text_free)(text) };
        Ok(value)
    }
}

impl Drop for LuxIndex {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { (self.fns.free)(self.raw) };
            self.raw = ptr::null_mut();
        }
    }
}

fn check_status(status: i32, action: &str) -> Result<(), Box<dyn Error>> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("LuxCore error {status} while {action}").into())
    }
}

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
