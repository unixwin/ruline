//! In-process history store backing the GNU history ABI.
//!
//! The layout of `HIST_ENTRY` is ABI-visible, so each entry owns a libc
//! allocation for its line and timestamp and keeps the boxed handle at a
//! stable address.  All mutation is serialized by a mutex; callers receive
//! pointers into the store, matching GNU's "valid until the history changes"
//! contract.

use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::mem::ManuallyDrop;
use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[allow(non_camel_case_types)]
#[repr(C)]
pub struct HIST_ENTRY {
    pub line: *mut c_char,
    pub timestamp: *mut c_char,
    pub data: *mut c_void,
}

#[allow(non_camel_case_types)]
#[repr(C)]
pub struct HISTORY_STATE {
    pub entries: *mut *mut HIST_ENTRY,
    pub offset: c_int,
    pub length: c_int,
    pub size: c_int,
    pub flags: c_int,
}

pub const HS_STIFLED: c_int = 0x01;

/// Signature of GNU's `history_inhibit_expansion_function`.
pub type HistoryInhibitExpansionFunc = Option<unsafe extern "C" fn(*mut c_char, c_int) -> c_int>;

// --- Exported history state ----------------------------------------------

#[no_mangle]
pub static mut history_base: c_int = 1;
#[no_mangle]
pub static mut history_length: c_int = 0;
#[no_mangle]
pub static mut history_max_entries: c_int = 0;
#[no_mangle]
pub static mut history_offset: c_int = 0;
#[no_mangle]
pub static mut history_lines_read_from_file: c_int = 0;
#[no_mangle]
pub static mut history_lines_written_to_file: c_int = 0;
#[no_mangle]
pub static mut max_input_history: c_int = 0;

#[no_mangle]
pub static mut history_expansion_char: c_char = b'!' as c_char;
#[no_mangle]
pub static mut history_subst_char: c_char = b'^' as c_char;
#[no_mangle]
pub static mut history_comment_char: c_char = 0;
#[no_mangle]
pub static mut history_quotes_inhibit_expansion: c_int = 0;
#[no_mangle]
pub static mut history_quoting_state: c_int = 0;
#[no_mangle]
pub static mut history_write_timestamps: c_int = 0;
#[no_mangle]
pub static mut history_multiline_entries: c_int = 0;
#[no_mangle]
pub static mut history_file_version: c_int = 1;

static WORD_DELIMITERS: &[u8] = b" \t\n;&()|<>\0";
static NO_EXPAND_CHARS: &[u8] = b" \t\n\r=\0";

#[no_mangle]
pub static mut history_word_delimiters: *mut c_char = WORD_DELIMITERS.as_ptr() as *mut c_char;
#[no_mangle]
pub static mut history_no_expand_chars: *mut c_char = NO_EXPAND_CHARS.as_ptr() as *mut c_char;
#[no_mangle]
pub static mut history_search_delimiter_chars: *mut c_char = ptr::null_mut();

#[no_mangle]
pub static mut history_inhibit_expansion_function: HistoryInhibitExpansionFunc = None;

const HISTORY_QUOTE_CHARACTERS: &[u8] = b"\"'`";
const HISTORY_EVENT_DELIMITERS: &[u8] = b"^$*%-";
const SLASHIFY_IN_QUOTES: &[u8] = b"\\`\"$";
const WORD_SPECIAL_CHARS: &[u8] = b":$*%^";

static HISTORY_STIFLED: AtomicBool = AtomicBool::new(false);

// --- Internal storage -----------------------------------------------------

struct Entry {
    handle: Box<HIST_ENTRY>,
}

impl Entry {
    fn new(line: &[u8], timestamp: &[u8], data: *mut c_void) -> Option<Self> {
        let line_ptr = duplicate_bytes(line);
        if line_ptr.is_null() {
            return None;
        }
        let timestamp_ptr = duplicate_bytes(timestamp);
        if timestamp_ptr.is_null() {
            unsafe { libc::free(line_ptr as *mut c_void) };
            return None;
        }
        Some(Self {
            handle: Box::new(HIST_ENTRY {
                line: line_ptr,
                timestamp: timestamp_ptr,
                data,
            }),
        })
    }

    fn as_ptr(&mut self) -> *mut HIST_ENTRY {
        &mut *self.handle
    }

    /// Hand ownership of the entry to a C caller, exactly as GNU
    /// `remove_history` does.  The strings and the returned struct are libc
    /// allocations so the caller may release them with `free_history_entry`
    /// (or `free`), matching GNU's ownership rules.
    fn into_raw(self) -> *mut HIST_ENTRY {
        let entry = ManuallyDrop::new(self);
        let handle = Box::into_raw(unsafe { ptr::read(&entry.handle) });
        let copy = unsafe { libc::malloc(std::mem::size_of::<HIST_ENTRY>()) as *mut HIST_ENTRY };
        if copy.is_null() {
            return handle;
        }
        unsafe {
            ptr::write(copy, ptr::read(handle));
            libc::free(handle as *mut c_void);
        }
        copy
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        unsafe {
            libc::free(self.handle.line as *mut c_void);
            libc::free(self.handle.timestamp as *mut c_void);
        }
    }
}

// SAFETY: all access to the entries is serialized by `HISTORY`.  The raw
// pointers only point at libc allocations owned by the entry itself.
unsafe impl Send for Entry {}

struct HistoryState {
    entries: Vec<Entry>,
    /// GNU hands callers the actual `the_history` array, NULL terminated.  The
    /// cached vector is rebuilt whenever the entry set changes, so pointers
    /// stay valid until history changes, as the ABI promises.
    list: Vec<*mut HIST_ENTRY>,
}

impl HistoryState {
    fn rebuild_list(&mut self) {
        let mut list = Vec::with_capacity(self.entries.len() + 1);
        for entry in &mut self.entries {
            list.push(entry.as_ptr());
        }
        list.push(ptr::null_mut());
        self.list = list;
    }

    fn rebuild_list_if_allocated(&mut self) {
        if !self.list.is_empty() || !self.entries.is_empty() {
            self.rebuild_list();
        }
    }
}

// SAFETY: access to the entry and list arrays is serialized by `HISTORY`;
// the pointers refer to libc allocations owned by the entries.
unsafe impl Send for HistoryState {}

static HISTORY: Mutex<HistoryState> = Mutex::new(HistoryState {
    entries: Vec::new(),
    list: Vec::new(),
});

fn history() -> std::sync::MutexGuard<'static, HistoryState> {
    HISTORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn duplicate_bytes(bytes: &[u8]) -> *mut c_char {
    let allocation = unsafe { libc::malloc(bytes.len() + 1) as *mut c_char };
    if allocation.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            crate::state::char_ptr_as_u8(allocation),
            bytes.len(),
        );
        *allocation.add(bytes.len()) = 0;
    }
    allocation
}

fn c_str_bytes<'a>(value: *const c_char) -> &'a [u8] {
    if value.is_null() {
        &[]
    } else {
        // SAFETY: ABI callers pass NUL-terminated strings valid for the call.
        unsafe { CStr::from_ptr(value).to_bytes() }
    }
}

fn c_str_len(value: *const c_char) -> usize {
    c_str_bytes(value).len()
}

fn at(bytes: &[u8], index: usize) -> u8 {
    bytes.get(index).copied().unwrap_or(0)
}

fn member(byte: u8, set: &[u8]) -> bool {
    byte != 0 && set.contains(&byte)
}

fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

fn digit_value(byte: u8) -> c_int {
    c_int::from(byte - b'0')
}

fn field_delim(byte: u8) -> bool {
    byte == b' ' || byte == b'\t' || byte == b'\n'
}

fn word_delimiters() -> Vec<u8> {
    c_str_bytes(unsafe { ptr::addr_of!(history_word_delimiters).read() }).to_vec()
}

fn search_delimiter_chars() -> Vec<u8> {
    c_str_bytes(unsafe { ptr::addr_of!(history_search_delimiter_chars).read() }).to_vec()
}

fn history_expansion_character() -> u8 {
    #[allow(clippy::unnecessary_cast)]
    unsafe {
        ptr::addr_of!(history_expansion_char).read() as u8
    }
}

fn history_subst_character() -> u8 {
    #[allow(clippy::unnecessary_cast)]
    unsafe {
        ptr::addr_of!(history_subst_char).read() as u8
    }
}

fn no_expand_characters() -> Vec<u8> {
    c_str_bytes(unsafe { ptr::addr_of!(history_no_expand_chars).read() }).to_vec()
}

fn quotes_inhibit_expansion() -> bool {
    unsafe { ptr::addr_of!(history_quotes_inhibit_expansion).read() != 0 }
}

fn quoting_state() -> u8 {
    #[allow(clippy::unnecessary_cast)]
    unsafe {
        ptr::addr_of!(history_quoting_state).read() as u8
    }
}

fn inhibit_expansion_function() -> HistoryInhibitExpansionFunc {
    unsafe { ptr::addr_of!(history_inhibit_expansion_function).read() }
}

fn alloc_hist_entry(
    line: Option<&[u8]>,
    timestamp: Option<&[u8]>,
    data: *mut c_void,
) -> *mut HIST_ENTRY {
    let line_ptr = match line {
        Some(bytes) => duplicate_bytes(bytes),
        None => ptr::null_mut(),
    };
    if line.is_some() && line_ptr.is_null() {
        return ptr::null_mut();
    }
    let timestamp_ptr = match timestamp {
        Some(bytes) => duplicate_bytes(bytes),
        None => ptr::null_mut(),
    };
    if timestamp.is_some() && timestamp_ptr.is_null() {
        unsafe { libc::free(line_ptr as *mut c_void) };
        return ptr::null_mut();
    }
    let entry = unsafe { libc::malloc(std::mem::size_of::<HIST_ENTRY>()) as *mut HIST_ENTRY };
    if entry.is_null() {
        unsafe {
            libc::free(line_ptr as *mut c_void);
            libc::free(timestamp_ptr as *mut c_void);
        }
        return ptr::null_mut();
    }
    unsafe {
        ptr::write(
            entry,
            HIST_ENTRY {
                line: line_ptr,
                timestamp: timestamp_ptr,
                data,
            },
        );
    }
    entry
}

#[derive(Default)]
struct ExpansionStatics {
    search_string: Option<Vec<u8>>,
    search_match: Option<Vec<u8>>,
    subst_lhs: Option<Vec<u8>>,
    subst_rhs: Vec<u8>,
    subst_lhs_len: usize,
    subst_rhs_len: usize,
}

static EXPANSION: Mutex<ExpansionStatics> = Mutex::new(ExpansionStatics {
    search_string: None,
    search_match: None,
    subst_lhs: None,
    subst_rhs: Vec::new(),
    subst_lhs_len: 0,
    subst_rhs_len: 0,
});

fn expansion() -> std::sync::MutexGuard<'static, ExpansionStatics> {
    EXPANSION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn expansion_search_string() -> Option<Vec<u8>> {
    expansion().search_string.clone()
}

fn set_expansion_search_string(value: Option<Vec<u8>>) {
    expansion().search_string = value;
}

fn expansion_search_match() -> Option<Vec<u8>> {
    expansion().search_match.clone()
}

fn set_expansion_search_match(value: Option<Vec<u8>>) {
    expansion().search_match = value;
}

fn set_history_length_from_store(state: &HistoryState) {
    let len = state.entries.len() as c_int;
    unsafe { ptr::addr_of_mut!(history_length).write(len) };
}

fn history_base_value() -> c_int {
    unsafe { ptr::addr_of!(history_base).read() }
}

fn history_offset_value() -> c_int {
    unsafe { ptr::addr_of!(history_offset).read() }
}

fn history_length_value() -> c_int {
    unsafe { ptr::addr_of!(history_length).read() }
}

fn set_history_offset(value: c_int) {
    unsafe { ptr::addr_of_mut!(history_offset).write(value) };
}

fn history_max_entries_value() -> c_int {
    unsafe { ptr::addr_of!(history_max_entries).read() }
}

fn set_history_max_entries(value: c_int) {
    unsafe { ptr::addr_of_mut!(history_max_entries).write(value) };
}

fn increment_history_base(amount: c_int) {
    unsafe {
        let base = ptr::addr_of!(history_base).read();
        ptr::addr_of_mut!(history_base).write(base + amount);
    }
}

fn history_comment() -> u8 {
    #[allow(clippy::unnecessary_cast)]
    unsafe {
        ptr::addr_of!(history_comment_char).read() as u8
    }
}

fn set_history_comment(value: u8) {
    unsafe { ptr::addr_of_mut!(history_comment_char).write(value as c_char) };
}

fn initial_timestamp() -> Vec<u8> {
    let comment = history_comment();
    if comment == 0 {
        return Vec::new();
    }
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("{}{seconds}", comment as char).into_bytes()
}

fn timestamp_for_new_entry() -> Vec<u8> {
    initial_timestamp()
}

fn push_entry(
    state: &mut HistoryState,
    line: &[u8],
    timestamp: Option<&[u8]>,
    data: *mut c_void,
) -> bool {
    let stifled = HISTORY_STIFLED.load(Ordering::Relaxed);
    let max = history_max_entries_value();

    if stifled && max <= 0 {
        return false;
    }
    if stifled && state.entries.len() as c_int >= max {
        state.entries.remove(0);
        increment_history_base(1);
    }

    let generated = timestamp_for_new_entry();
    let timestamp = timestamp.unwrap_or(generated.as_slice());
    let Some(entry) = Entry::new(line, timestamp, data) else {
        return false;
    };
    state.entries.push(entry);
    state.rebuild_list();
    set_history_length_from_store(state);
    true
}

fn is_timestamp_line(line: &[u8]) -> bool {
    line.first() == Some(&b'#') && line.get(1).is_some_and(u8::is_ascii_digit)
}

fn strip_line_ending(mut line: &[u8]) -> &[u8] {
    if line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    line
}

fn history_path(filename: *const c_char) -> Option<PathBuf> {
    if filename.is_null() {
        let home = std::env::var_os("HOME")?;
        return Some(PathBuf::from(home).join(".history"));
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = c_str_bytes(filename);
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }

    #[cfg(not(unix))]
    {
        Some(PathBuf::from(
            String::from_utf8_lossy(c_str_bytes(filename)).into_owned(),
        ))
    }
}

fn errno_from(error: &io::Error) -> c_int {
    error.raw_os_error().unwrap_or(libc::EIO)
}

fn write_entry(file: &mut File, entry: &HIST_ENTRY, timestamps: bool) -> io::Result<usize> {
    let mut written = 0;
    if timestamps {
        let timestamp = c_str_bytes(entry.timestamp);
        if !timestamp.is_empty() {
            file.write_all(timestamp)?;
            file.write_all(b"\n")?;
            written += 1;
        }
    }
    file.write_all(c_str_bytes(entry.line))?;
    file.write_all(b"\n")?;
    Ok(written + 1)
}

// --- Session and list management -----------------------------------------

#[no_mangle]
pub unsafe extern "C" fn using_history() {
    let length = ptr::addr_of!(history_length).read();
    set_history_offset(length);
}

#[no_mangle]
pub unsafe extern "C" fn add_history(string: *const c_char) {
    if string.is_null() {
        return;
    }
    let line = c_str_bytes(string);
    let mut state = history();
    push_entry(&mut state, line, None, ptr::null_mut());
}

#[no_mangle]
pub unsafe extern "C" fn add_history_time(string: *const c_char) {
    if string.is_null() {
        return;
    }
    let timestamp = c_str_bytes(string);
    let mut state = history();
    let Some(entry) = state.entries.last_mut() else {
        return;
    };

    let replacement = duplicate_bytes(timestamp);
    if replacement.is_null() {
        return;
    }
    libc::free(entry.handle.timestamp as *mut c_void);
    entry.handle.timestamp = replacement;
}

#[no_mangle]
pub unsafe extern "C" fn clear_history() {
    let mut state = history();
    state.entries.clear();
    // GNU clear_history frees the array and leaves `the_history == NULL`;
    // removing the last entry one by one, by contrast, keeps the allocated
    // NULL-terminated vector.
    state.list.clear();
    set_history_length_from_store(&state);
    set_history_offset(0);
    ptr::addr_of_mut!(history_base).write(1);
}

#[no_mangle]
pub unsafe extern "C" fn stifle_history(max: c_int) {
    let mut state = history();
    let max = max.max(0);
    let length = state.entries.len() as c_int;

    if length > max {
        let dropped = (length - max) as usize;
        state.entries.drain(..dropped);
        ptr::addr_of_mut!(history_base).write(dropped as c_int);
        set_history_length_from_store(&state);
        let offset = history_offset_value().min(max);
        set_history_offset(offset);
    }
    state.rebuild_list_if_allocated();

    HISTORY_STIFLED.store(true, Ordering::Relaxed);
    set_history_max_entries(max);
    ptr::addr_of_mut!(max_input_history).write(max);
}

#[no_mangle]
pub unsafe extern "C" fn unstifle_history() -> c_int {
    let max = history_max_entries_value();
    if HISTORY_STIFLED.swap(false, Ordering::Relaxed) {
        max
    } else {
        -max
    }
}

#[no_mangle]
pub unsafe extern "C" fn history_is_stifled() -> c_int {
    c_int::from(HISTORY_STIFLED.load(Ordering::Relaxed))
}

// --- Snapshot state, list vector and entry ownership -----------------------

#[no_mangle]
pub unsafe extern "C" fn history_get_history_state() -> *mut HISTORY_STATE {
    let mut state = history();
    let snapshot = libc::malloc(std::mem::size_of::<HISTORY_STATE>()) as *mut HISTORY_STATE;
    if snapshot.is_null() {
        return ptr::null_mut();
    }
    let entries = if state.list.is_empty() {
        ptr::null_mut()
    } else {
        state.list.as_mut_ptr()
    };
    let size = if state.list.is_empty() {
        0
    } else {
        state.list.capacity().max(state.entries.len() + 1) as c_int
    };
    ptr::write(
        snapshot,
        HISTORY_STATE {
            entries,
            offset: history_offset_value(),
            length: state.entries.len() as c_int,
            size,
            flags: if HISTORY_STIFLED.load(Ordering::Relaxed) {
                HS_STIFLED
            } else {
                0
            },
        },
    );
    snapshot
}

#[no_mangle]
pub unsafe extern "C" fn history_set_history_state(snapshot: *mut HISTORY_STATE) {
    if snapshot.is_null() {
        return;
    }
    let requested = ptr::read(snapshot);
    let mut state = history();
    let existing = state.entries.len();
    let requested_length = requested.length.max(0) as usize;
    if requested_length < existing {
        state.entries.truncate(requested_length);
    }
    let length = state.entries.len() as c_int;
    state.rebuild_list_if_allocated();
    set_history_length_from_store(&state);
    set_history_offset(requested.offset.clamp(0, length));
    if requested.flags & HS_STIFLED != 0 {
        HISTORY_STIFLED.store(true, Ordering::Relaxed);
    }
}

#[no_mangle]
pub unsafe extern "C" fn history_list() -> *mut *mut HIST_ENTRY {
    let mut state = history();
    if state.list.is_empty() {
        ptr::null_mut()
    } else {
        state.list.as_mut_ptr()
    }
}

#[no_mangle]
pub unsafe extern "C" fn alloc_history_entry(
    string: *mut c_char,
    timestamp: *mut c_char,
) -> *mut HIST_ENTRY {
    let line = if string.is_null() {
        None
    } else {
        Some(c_str_bytes(string))
    };
    // GNU takes ownership of TIMESTAMP without copying it.
    let entry = alloc_hist_entry(line, None, ptr::null_mut());
    if !entry.is_null() {
        (*entry).timestamp = timestamp;
    }
    entry
}

#[no_mangle]
pub unsafe extern "C" fn copy_history_entry(entry: *mut HIST_ENTRY) -> *mut HIST_ENTRY {
    if entry.is_null() {
        return ptr::null_mut();
    }
    let line = if (*entry).line.is_null() {
        None
    } else {
        Some(c_str_bytes((*entry).line))
    };
    let timestamp = if (*entry).timestamp.is_null() {
        None
    } else {
        Some(c_str_bytes((*entry).timestamp))
    };
    alloc_hist_entry(line, timestamp, (*entry).data)
}

#[no_mangle]
pub unsafe extern "C" fn free_history_entry(entry: *mut HIST_ENTRY) -> *mut c_void {
    if entry.is_null() {
        return ptr::null_mut();
    }
    let data = (*entry).data;
    libc::free((*entry).line as *mut c_void);
    libc::free((*entry).timestamp as *mut c_void);
    libc::free(entry as *mut c_void);
    data
}

#[no_mangle]
pub unsafe extern "C" fn remove_history(which: c_int) -> *mut HIST_ENTRY {
    let mut state = history();
    if which < 0 || which >= state.entries.len() as c_int {
        return ptr::null_mut();
    }
    let entry = state.entries.remove(which as usize);
    state.rebuild_list();
    set_history_length_from_store(&state);
    entry.into_raw()
}

#[no_mangle]
pub unsafe extern "C" fn remove_history_range(first: c_int, last: c_int) -> *mut *mut HIST_ENTRY {
    let mut state = history();
    let length = state.entries.len() as c_int;
    if length == 0 || first < 0 || first >= length || last < 0 || last >= length || first > last {
        return ptr::null_mut();
    }

    let count = (last - first + 1) as usize;
    let array =
        libc::malloc((count + 1) * std::mem::size_of::<*mut HIST_ENTRY>()) as *mut *mut HIST_ENTRY;
    if array.is_null() {
        return ptr::null_mut();
    }

    let removed: Vec<Entry> = state
        .entries
        .drain(first as usize..=last as usize)
        .collect();
    for (index, entry) in removed.into_iter().enumerate() {
        *array.add(index) = entry.into_raw();
    }
    *array.add(count) = ptr::null_mut();

    state.rebuild_list();
    set_history_length_from_store(&state);
    array
}

#[no_mangle]
pub unsafe extern "C" fn replace_history_entry(
    which: c_int,
    line: *const c_char,
    data: *mut c_void,
) -> *mut HIST_ENTRY {
    let mut state = history();
    if which < 0 || which >= state.entries.len() as c_int {
        return ptr::null_mut();
    }

    let index = which as usize;
    let old_timestamp = state.entries[index].handle.timestamp;
    let timestamp = if old_timestamp.is_null() {
        None
    } else {
        Some(c_str_bytes(old_timestamp))
    };
    let new_line = c_str_bytes(line);

    let line_ptr = duplicate_bytes(new_line);
    if line_ptr.is_null() {
        return ptr::null_mut();
    }
    let timestamp_ptr = match timestamp {
        Some(bytes) => duplicate_bytes(bytes),
        None => ptr::null_mut(),
    };
    if timestamp.is_some() && timestamp_ptr.is_null() {
        libc::free(line_ptr as *mut c_void);
        return ptr::null_mut();
    }

    let old = state.entries.remove(index);
    state.entries.insert(
        index,
        Entry {
            handle: Box::new(HIST_ENTRY {
                line: line_ptr,
                timestamp: timestamp_ptr,
                data,
            }),
        },
    );
    state.rebuild_list();
    old.into_raw()
}

// --- Traversal and lookup -------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn where_history() -> c_int {
    history_offset_value()
}

#[no_mangle]
pub unsafe extern "C" fn current_history() -> *mut HIST_ENTRY {
    let mut state = history();
    let offset = history_offset_value();
    let length = state.entries.len() as c_int;
    if offset < 0 || offset >= length {
        return ptr::null_mut();
    }
    state.entries[offset as usize].as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn history_get(offset: c_int) -> *mut HIST_ENTRY {
    let mut state = history();
    let local = offset - history_base_value();
    if local < 0 || local >= state.entries.len() as c_int {
        return ptr::null_mut();
    }
    state.entries[local as usize].as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn history_get_time(entry: *mut HIST_ENTRY) -> libc::time_t {
    if entry.is_null() {
        return 0;
    }
    let timestamp = c_str_bytes((*entry).timestamp);
    if !is_timestamp_line(timestamp) {
        return 0;
    }
    std::str::from_utf8(&timestamp[1..])
        .ok()
        .and_then(|digits| digits.parse::<libc::time_t>().ok())
        .unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn history_set_pos(position: c_int) -> c_int {
    let length = ptr::addr_of!(history_length).read();
    if position < 0 || position > length {
        return 0;
    }
    set_history_offset(position);
    1
}

#[no_mangle]
pub unsafe extern "C" fn previous_history() -> *mut HIST_ENTRY {
    let mut state = history();
    let length = state.entries.len() as c_int;
    let offset = history_offset_value();
    if offset <= 0 {
        return ptr::null_mut();
    }
    let next_offset = offset - 1;
    set_history_offset(next_offset);
    if next_offset >= length {
        return ptr::null_mut();
    }
    state.entries[next_offset as usize].as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn next_history() -> *mut HIST_ENTRY {
    let mut state = history();
    let length = state.entries.len() as c_int;
    let offset = history_offset_value();
    if offset < 0 || offset >= length {
        return ptr::null_mut();
    }
    let next_offset = offset + 1;
    set_history_offset(next_offset);
    if next_offset >= length {
        return ptr::null_mut();
    }
    state.entries[next_offset as usize].as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn history_total_bytes() -> c_int {
    let state = history();
    let total: usize = state
        .entries
        .iter()
        .map(|entry| c_str_len(entry.handle.line) + c_str_len(entry.handle.timestamp))
        .sum();
    total.min(c_int::MAX as usize) as c_int
}

// --- Searching the history list -------------------------------------------

#[derive(Clone, Copy)]
struct SearchFlags {
    anchored: bool,
}

/// Port of GNU `histsearch.c:history_search_internal()` for the public flags.
/// `listdir < 0` searches towards older entries, otherwise towards newer
/// entries.  `linedir < 0` scans each line from its end.  Returns the byte
/// offset of the match within the line, or -1.
fn search_internal(
    state: &mut HistoryState,
    needle: &[u8],
    listdir: c_int,
    linedir: c_int,
    flags: SearchFlags,
) -> c_int {
    if needle.is_empty() {
        return -1;
    }

    let length = state.entries.len() as c_int;
    if length == 0 {
        return -1;
    }

    let reverse = listdir < 0;
    let mut index = history_offset_value();

    if index < 0 {
        return -1;
    }
    if !reverse && index >= length {
        return -1;
    }
    if reverse && index >= length {
        index = length - 1;
    }

    loop {
        if (reverse && index < 0) || (!reverse && index == length) {
            return -1;
        }

        let line = c_str_bytes(state.entries[index as usize].handle.line);
        if needle.len() <= line.len() {
            if flags.anchored {
                if line.starts_with(needle) {
                    set_history_offset(index);
                    return 0;
                }
            } else if linedir < 0 {
                let last = line.len() - needle.len();
                for start in (0..=last).rev() {
                    if line[start..].starts_with(needle) {
                        set_history_offset(index);
                        return start as c_int;
                    }
                }
            } else {
                let last = line.len() - needle.len();
                for start in 0..=last {
                    if line[start..].starts_with(needle) {
                        set_history_offset(index);
                        return start as c_int;
                    }
                }
            }
        }

        if reverse {
            index -= 1;
        } else {
            index += 1;
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn history_search(string: *const c_char, direction: c_int) -> c_int {
    let needle = c_str_bytes(string);
    let mut state = history();
    search_internal(
        &mut state,
        needle,
        direction,
        direction,
        SearchFlags { anchored: false },
    )
}

#[no_mangle]
pub unsafe extern "C" fn history_search_prefix(string: *const c_char, direction: c_int) -> c_int {
    let needle = c_str_bytes(string);
    let mut state = history();
    search_internal(
        &mut state,
        needle,
        direction,
        direction,
        SearchFlags { anchored: true },
    )
}

#[no_mangle]
pub unsafe extern "C" fn history_search_pos(
    string: *const c_char,
    direction: c_int,
    pos: c_int,
) -> c_int {
    let needle = c_str_bytes(string);
    let old = history_offset_value();
    let mut state = history();

    if pos >= 0 && pos <= state.entries.len() as c_int {
        set_history_offset(pos);
    }
    if search_internal(
        &mut state,
        needle,
        direction,
        direction,
        SearchFlags { anchored: false },
    ) < 0
    {
        set_history_offset(old);
        return -1;
    }
    let found = history_offset_value();
    set_history_offset(old);
    found
}

// --- History expansion and tokenization -----------------------------------

fn hist_string_extract_single_quoted(string: &[u8], index: &mut usize, backslash_escapes: bool) {
    let mut i = *index;
    while at(string, i) != 0 && at(string, i) != b'\'' {
        if backslash_escapes && at(string, i) == b'\\' && at(string, i + 1) != 0 {
            i += 1;
        }
        i += 1;
    }
    *index = i;
}

fn quote_breaks(source: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    output.push(b'\'');
    for &byte in source {
        if byte == b'\'' {
            output.extend_from_slice(b"'\\''");
        } else if byte == b' ' || byte == b'\t' || byte == b'\n' {
            output.push(b'\'');
            output.push(byte);
            output.push(b'\'');
        } else {
            output.push(byte);
        }
    }
    output.push(b'\'');
    output
}

fn sh_single_quote(source: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    output.push(b'\'');
    for &byte in source {
        output.push(byte);
        if byte == b'\'' {
            output.extend_from_slice(b"\\''");
        }
    }
    output.push(b'\'');
    output
}

fn history_event_error(string: &[u8], start: usize, current: usize, kind: c_int) -> Vec<u8> {
    let message: &[u8] = match kind {
        0 => b"event not found",
        1 => b"bad word specifier",
        2 => b"substitution failed",
        3 => b"unrecognized history modifier",
        4 => b"no previous substitution",
        _ => b"unknown expansion error",
    };
    let mut output = Vec::new();
    if at(string, start) != 0 {
        let length = current
            .saturating_sub(start)
            .min(string.len().saturating_sub(start));
        output.extend_from_slice(&string[start..start + length]);
    }
    output.push(b':');
    output.push(b' ');
    output.extend_from_slice(message);
    output
}

fn get_subst_pattern(
    string: &[u8],
    index: &mut usize,
    delimiter: u8,
    is_rhs: bool,
) -> Option<Vec<u8>> {
    let start = *index;
    let mut end = start;
    while at(string, end) != 0 && at(string, end) != delimiter {
        if at(string, end) == b'\\' && at(string, end + 1) == delimiter {
            end += 1;
        }
        end += 1;
    }

    let pattern = if end > start || is_rhs {
        let mut value = Vec::new();
        let mut i = start;
        while i < end {
            if at(string, i) == b'\\' && at(string, i + 1) == delimiter {
                i += 1;
            }
            value.push(at(string, i));
            i += 1;
        }
        Some(value)
    } else {
        None
    };

    *index = end;
    if at(string, *index) != 0 {
        *index += 1;
    }
    pattern
}

fn postproc_subst_rhs(lhs: &[u8], rhs: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut i = 0;
    while i < rhs.len() {
        if rhs[i] == b'&' {
            output.extend_from_slice(lhs);
        } else {
            if rhs[i] == b'\\' && at(rhs, i + 1) == b'&' {
                i += 1;
            }
            output.push(rhs[i]);
        }
        i += 1;
    }
    output
}

fn history_tokenize_word(string: &[u8], ind: usize) -> usize {
    let delimiters = word_delimiters();
    let mut i = ind;
    let mut delimiter = 0_u8;
    let mut nestdelim = 0_i32;
    let mut delimopen = 0_u8;
    let mut need_word = false;

    if member(at(string, i), b"()\n") {
        return i + 1;
    }

    if is_digit(at(string, i)) {
        let mut j = i;
        while is_digit(at(string, j)) {
            j += 1;
        }
        if at(string, j) == 0 {
            return j;
        }
        if at(string, j) == b'<' || at(string, j) == b'>' {
            i = j;
        } else {
            i = j;
            need_word = true;
        }
    }

    if !need_word {
        let character = at(string, i);
        if member(character, b"<>;&|") {
            let peek = at(string, i + 1);
            if peek == character {
                if peek == b'<' && (at(string, i + 2) == b'-' || at(string, i + 2) == b'<') {
                    i += 1;
                }
                return i + 2;
            } else if peek == b'&' && (character == b'>' || character == b'<') {
                let mut j = i + 2;
                while is_digit(at(string, j)) {
                    j += 1;
                }
                if at(string, j) == b'-' {
                    j += 1;
                }
                return j;
            } else if (peek == b'>' && character == b'&') || (peek == b'|' && character == b'>') {
                return i + 2;
            } else if peek == b'(' && (character == b'>' || character == b'<') {
                i += 2;
                delimopen = b'(';
                delimiter = b')';
                nestdelim = 1;
                need_word = true;
            } else {
                return i + 1;
            }
        }
    }

    if !need_word && !member(at(string, i), b"<>;&|") {
        // Fall through into the ordinary word scanner.
    }

    if delimiter == 0 && member(at(string, i), HISTORY_QUOTE_CHARACTERS) {
        delimiter = at(string, i);
        i += 1;
    }

    loop {
        let character = at(string, i);
        if character == 0 {
            break;
        }
        if character == b'\\' && at(string, i + 1) == b'\n' {
            i += 2;
            continue;
        }
        if character == b'\\'
            && delimiter != b'\''
            && (delimiter != b'"' || member(character, SLASHIFY_IN_QUOTES))
        {
            i += 1;
            if at(string, i) == 0 {
                break;
            }
            i += 1;
            continue;
        }
        if nestdelim > 0 && character == delimopen {
            nestdelim += 1;
            i += 1;
            continue;
        }
        if nestdelim > 0 && character == delimiter {
            nestdelim -= 1;
            if nestdelim == 0 {
                delimiter = 0;
            }
            i += 1;
            continue;
        }
        if delimiter != 0 && character == delimiter {
            delimiter = 0;
            i += 1;
            continue;
        }
        if nestdelim == 0
            && delimiter == 0
            && member(character, b"<>$!@?+*")
            && at(string, i + 1) == b'('
        {
            i += 1;
            if at(string, i + 1) == 0 {
                break;
            }
            delimopen = b'(';
            delimiter = b')';
            nestdelim = 1;
            i += 1;
            continue;
        }
        if delimiter == 0 && member(character, &delimiters) {
            break;
        }
        if delimiter == 0 && member(character, HISTORY_QUOTE_CHARACTERS) {
            delimiter = character;
        }
        i += 1;
    }

    i
}

fn tokenize_internal(string: &[u8], wind: c_int) -> (Vec<*mut c_char>, c_int) {
    let mut result: Vec<*mut c_char> = Vec::new();
    let mut word_index = -1;
    let delimiters = word_delimiters();
    let has_delimiters = !unsafe { ptr::addr_of!(history_word_delimiters).read() }.is_null();
    let comment = history_comment();
    let mut i = 0_usize;

    while i < string.len() {
        while i < string.len() && field_delim(string[i]) {
            i += 1;
        }
        if i >= string.len() {
            return (result, word_index);
        }
        if comment != 0 && string[i] == comment {
            return (result, word_index);
        }

        let start = i;
        i = history_tokenize_word(string, start);
        if i == start && has_delimiters {
            i += 1;
            while i < string.len() && member(string[i], &delimiters) {
                i += 1;
            }
        }

        if wind != -1 && wind >= start as c_int && wind < i as c_int {
            word_index = result.len() as c_int;
        }

        let end = i.min(string.len());
        result.push(duplicate_bytes(&string[start..end]));
    }

    (result, word_index)
}

fn free_word_list(words: &[*mut c_char]) {
    for &word in words {
        unsafe { libc::free(word as *mut c_void) };
    }
}

fn history_find_word(line: &[u8], index: c_int) -> Option<Vec<u8>> {
    let (words, word_index) = tokenize_internal(line, index);
    let found = if word_index >= 0 && (word_index as usize) < words.len() {
        Some(c_str_bytes(words[word_index as usize]).to_vec())
    } else {
        None
    };
    free_word_list(&words);
    found
}

#[no_mangle]
pub unsafe extern "C" fn history_tokenize(string: *const c_char) -> *mut *mut c_char {
    let (words, _) = tokenize_internal(c_str_bytes(string), -1);
    if words.is_empty() {
        return ptr::null_mut();
    }
    let array =
        libc::malloc((words.len() + 1) * std::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
    if array.is_null() {
        free_word_list(&words);
        return ptr::null_mut();
    }
    for (index, word) in words.iter().enumerate() {
        *array.add(index) = *word;
    }
    *array.add(words.len()) = ptr::null_mut();
    array
}

fn arg_extract_bytes(first: c_int, last: c_int, string: &[u8]) -> Option<Vec<u8>> {
    let (words, _) = tokenize_internal(string, -1);
    let length = words.len() as c_int;
    let mut first = first;
    let mut last = last;

    if last < 0 {
        last = length + last - 1;
    }
    if first < 0 {
        first = length + first - 1;
    }
    let dollar = c_int::from(b'$');
    if last == dollar {
        last = length - 1;
    }
    if first == dollar {
        first = length - 1;
    }
    last += 1;

    let result = if first >= length || last > length || first < 0 || last < 0 || first > last {
        None
    } else {
        let mut value = Vec::new();
        for index in first..last {
            if index > first {
                value.push(b' ');
            }
            value.extend_from_slice(c_str_bytes(words[index as usize]));
        }
        Some(value)
    };

    free_word_list(&words);
    result
}

#[no_mangle]
pub unsafe extern "C" fn history_arg_extract(
    first: c_int,
    last: c_int,
    string: *const c_char,
) -> *mut c_char {
    arg_extract_bytes(first, last, c_str_bytes(string))
        .map_or(ptr::null_mut(), |value| duplicate_bytes(&value))
}

fn lookup_history_event(
    string: &[u8],
    index: &mut c_int,
    delimiting_quote: c_int,
) -> Option<Vec<u8>> {
    let mut buffer = Vec::with_capacity(string.len() + 1);
    buffer.extend_from_slice(string);
    buffer.push(0);
    let line = unsafe {
        get_history_event(
            buffer.as_ptr() as *const c_char,
            index as *mut c_int,
            delimiting_quote,
        )
    };
    if line.is_null() {
        None
    } else {
        Some(c_str_bytes(line).to_vec())
    }
}

fn history_line_for_which(which: c_int) -> *mut c_char {
    let state = history();
    let local = which - history_base_value();
    if local < 0 || local >= state.entries.len() as c_int {
        return ptr::null_mut();
    }
    state.entries[local as usize].handle.line
}

#[no_mangle]
pub unsafe extern "C" fn get_history_event(
    string: *const c_char,
    caller_index: *mut c_int,
    delimiting_quote: c_int,
) -> *mut c_char {
    if caller_index.is_null() {
        return ptr::null_mut();
    }
    let bytes = c_str_bytes(string);
    let expansion = history_expansion_character();
    let mut i = *caller_index as usize;
    if at(bytes, i) != expansion {
        return ptr::null_mut();
    }
    i += 1;

    let mut sign = 1_i32;
    let mut substring_okay = false;

    if at(bytes, i) == expansion {
        i += 1;
        *caller_index = i as c_int;
        let state = history();
        return state
            .entries
            .last()
            .map_or(ptr::null_mut(), |entry| entry.handle.line);
    }

    if at(bytes, i) == b'-' && is_digit(at(bytes, i + 1)) {
        sign = -1;
        i += 1;
    }

    if is_digit(at(bytes, i)) {
        let mut which = 0_i32;
        while is_digit(at(bytes, i)) {
            which = which * 10 + digit_value(at(bytes, i));
            i += 1;
        }
        *caller_index = i as c_int;
        if sign < 0 {
            let length = history_length_value();
            which = (length + history_base_value()) - which;
        }
        return history_line_for_which(which);
    }

    if at(bytes, i) == b'?' {
        substring_okay = true;
        i += 1;
    }

    let local_index = i;
    let event_delimiters = HISTORY_EVENT_DELIMITERS;
    let search_delimiters = search_delimiter_chars();
    while at(bytes, i) != 0 {
        let character = at(bytes, i);
        let delimiter = (!substring_okay
            && (character == b' '
                || character == b'\t'
                || character == b':'
                || (i > local_index && character == b'-')
                || (character != b'-' && member(character, event_delimiters))
                || member(character, &search_delimiters)
                || character as c_int == delimiting_quote))
            || character == b'\n'
            || (substring_okay && character == b'?');
        if delimiter {
            break;
        }
        i += 1;
    }

    let mut event = bytes[local_index..i].to_vec();
    if substring_okay && at(bytes, i) == b'?' {
        i += 1;
    }
    *caller_index = i as c_int;

    if event.is_empty() && substring_okay {
        match expansion_search_string() {
            Some(previous) => event = previous,
            None => return ptr::null_mut(),
        }
    }

    let old_offset = history_offset_value();
    let mut state = history();
    let found = search_internal(
        &mut state,
        &event,
        -1,
        -1,
        SearchFlags {
            anchored: !substring_okay,
        },
    );
    if found < 0 {
        set_history_offset(old_offset);
        return ptr::null_mut();
    }

    let index = history_offset_value();
    let line = if index >= 0 && index < state.entries.len() as c_int {
        state.entries[index as usize].handle.line
    } else {
        ptr::null_mut()
    };
    if line.is_null() {
        set_history_offset(old_offset);
        return ptr::null_mut();
    }
    set_history_offset(old_offset);

    if substring_okay {
        set_expansion_search_string(Some(event));
        set_expansion_search_match(history_find_word(c_str_bytes(line), found));
    }

    line
}

enum WordSpecifier {
    None,
    Error,
    Value(Vec<u8>),
}

fn get_history_word_specifier(spec: &[u8], from: &[u8], caller_index: &mut usize) -> WordSpecifier {
    let mut i = *caller_index;
    let mut first = 0_i32;
    let mut last = 0_i32;
    let mut expecting_word_spec = false;

    if at(spec, i) == b':' {
        i += 1;
        expecting_word_spec = true;
    }

    if at(spec, i) == b'%' {
        *caller_index = i + 1;
        return WordSpecifier::Value(expansion_search_match().unwrap_or_default());
    }

    if at(spec, i) == b'*' {
        *caller_index = i + 1;
        return WordSpecifier::Value(
            arg_extract_bytes(1, c_int::from(b'$'), from).unwrap_or_default(),
        );
    }

    if at(spec, i) == b'$' {
        *caller_index = i + 1;
        return match arg_extract_bytes(c_int::from(b'$'), c_int::from(b'$'), from) {
            Some(value) => WordSpecifier::Value(value),
            None => WordSpecifier::None,
        };
    }

    if at(spec, i) == b'-' {
        first = 0;
    } else if at(spec, i) == b'^' {
        first = 1;
        i += 1;
    } else if is_digit(at(spec, i)) && expecting_word_spec {
        while is_digit(at(spec, i)) {
            first = first * 10 + digit_value(at(spec, i));
            i += 1;
        }
    } else {
        return WordSpecifier::None;
    }

    if at(spec, i) == b'^' || at(spec, i) == b'*' {
        last = if at(spec, i) == b'^' {
            1
        } else {
            c_int::from(b'$')
        };
        i += 1;
    } else if at(spec, i) != b'-' {
        last = first;
    } else {
        i += 1;
        if is_digit(at(spec, i)) {
            while is_digit(at(spec, i)) {
                last = last * 10 + digit_value(at(spec, i));
                i += 1;
            }
        } else if at(spec, i) == b'$' {
            i += 1;
            last = c_int::from(b'$');
        } else if at(spec, i) == b'^' {
            i += 1;
            last = 1;
        } else {
            last = -1;
        }
    }

    *caller_index = i;

    if last >= first || last == c_int::from(b'$') || last < 0 {
        match arg_extract_bytes(first, last, from) {
            Some(value) => WordSpecifier::Value(value),
            None => WordSpecifier::Error,
        }
    } else {
        WordSpecifier::None
    }
}

#[allow(clippy::too_many_arguments)]
fn history_expand_internal(
    string: &[u8],
    start: usize,
    delimiting_quote: c_int,
    end_index: &mut usize,
    current_line: &[u8],
) -> Result<(Vec<u8>, bool), Vec<u8>> {
    let mut i = start;

    let event = if member(at(string, i + 1), WORD_SPECIAL_CHARS) {
        i += 1;
        let mut fake_index: c_int = 0;
        lookup_history_event(b"!!", &mut fake_index, 0)
    } else if at(string, i + 1) == b'#' {
        i += 2;
        Some(current_line.to_vec())
    } else {
        let mut index = i as c_int;
        let event = lookup_history_event(string, &mut index, delimiting_quote);
        i = index.max(0) as usize;
        event
    };

    let Some(event) = event else {
        return Err(history_event_error(string, start, i, 0));
    };

    let mut starting_index = i;
    let word_specifier = get_history_word_specifier(string, &event, &mut i);
    let mut temp = match word_specifier {
        WordSpecifier::Error => {
            return Err(history_event_error(string, starting_index, i, 1));
        }
        WordSpecifier::Value(value) => value,
        WordSpecifier::None => event,
    };

    let mut want_quotes = 0_u8;
    let mut substitute_globally = false;
    let mut substitute_by_words = false;
    let mut print_only = false;
    starting_index = i;

    'modifiers: while at(string, i) == b':' {
        let mut character = at(string, i + 1);

        if character == b'g' || character == b'a' {
            substitute_globally = true;
            i += 1;
            character = at(string, i + 1);
        } else if character == b'G' {
            substitute_by_words = true;
            i += 1;
            character = at(string, i + 1);
        }

        match character {
            b'q' => want_quotes = b'q',
            b'x' => want_quotes = b'x',
            b'p' => print_only = true,
            b't' => {
                if let Some(position) = temp.iter().rposition(|&byte| byte == b'/') {
                    temp = temp[position + 1..].to_vec();
                }
            }
            b'h' => {
                if let Some(position) = temp.iter().rposition(|&byte| byte == b'/') {
                    temp.truncate(position);
                }
            }
            b'r' => {
                if let Some(position) = temp.iter().rposition(|&byte| byte == b'.') {
                    temp.truncate(position);
                }
            }
            b'e' => {
                if let Some(position) = temp.iter().rposition(|&byte| byte == b'.') {
                    temp = temp[position..].to_vec();
                }
            }
            b'&' | b's' => {
                if character == b's' {
                    if i + 2 < string.len() {
                        let delimiter = at(string, i + 2);
                        i += 3;
                        let mut statics = expansion();
                        let lhs_pattern = get_subst_pattern(string, &mut i, delimiter, false);
                        if let Some(pattern) = lhs_pattern {
                            statics.subst_lhs = Some(pattern);
                        } else if statics.subst_lhs.is_none() {
                            match statics.search_string.clone() {
                                Some(previous) if !previous.is_empty() => {
                                    statics.subst_lhs = Some(previous);
                                }
                                _ => {
                                    statics.subst_lhs = None;
                                }
                            }
                        }
                        statics.subst_lhs_len = statics.subst_lhs.as_ref().map_or(0, Vec::len);

                        let rhs_pattern =
                            get_subst_pattern(string, &mut i, delimiter, true).unwrap_or_default();
                        statics.subst_rhs = rhs_pattern;
                        statics.subst_rhs_len = statics.subst_rhs.len();

                        if statics.subst_lhs.is_some() && statics.subst_rhs.contains(&b'&') {
                            let lhs = statics.subst_lhs.clone().unwrap_or_default();
                            let rhs = statics.subst_rhs.clone();
                            let processed = postproc_subst_rhs(&lhs, &rhs);
                            statics.subst_rhs = processed;
                            statics.subst_rhs_len = statics.subst_rhs.len();
                        }
                    } else {
                        i += 2;
                        continue 'modifiers;
                    }
                } else {
                    i += 2;
                }

                let (lhs, rhs, lhs_len, rhs_len) = {
                    let statics = expansion();
                    (
                        statics.subst_lhs.clone(),
                        statics.subst_rhs.clone(),
                        statics.subst_lhs_len,
                        statics.subst_rhs_len,
                    )
                };

                if lhs_len == 0 {
                    return Err(history_event_error(string, starting_index, i, 4));
                }

                let lhs = lhs.unwrap_or_default();
                let mut l_temp = temp.len();
                if lhs_len > l_temp {
                    return Err(history_event_error(string, starting_index, i, 2));
                }

                let mut si = 0_usize;
                let mut we = 0_usize;
                let mut failed = true;
                let mut global_substitutions = 0_usize;

                while si + lhs_len <= l_temp {
                    if substitute_by_words && si > we {
                        while si < temp.len() && field_delim(temp[si]) {
                            si += 1;
                        }
                        we = history_tokenize_word(&temp, si);
                    }

                    if temp[si..].starts_with(&lhs) {
                        let mut new_event = Vec::with_capacity(rhs_len + l_temp - lhs_len);
                        new_event.extend_from_slice(&temp[..si]);
                        new_event.extend_from_slice(&rhs);
                        new_event.extend_from_slice(&temp[si + lhs_len..]);
                        temp = new_event;
                        failed = false;

                        if substitute_globally {
                            si += rhs_len;
                            l_temp = temp.len();
                            global_substitutions += 1;
                            continue;
                        } else if substitute_by_words {
                            si = we + 1;
                            l_temp = temp.len();
                            continue;
                        } else {
                            break;
                        }
                    }
                    si += 1;
                }

                if global_substitutions > 1 {
                    continue 'modifiers;
                }
                if !failed {
                    continue 'modifiers;
                }

                return Err(history_event_error(string, starting_index, i, 2));
            }
            _ => {
                return Err(history_event_error(string, i + 1, i + 2, 3));
            }
        }

        i += 2;
    }

    i = i.saturating_sub(1);

    if want_quotes == b'q' {
        temp = sh_single_quote(&temp);
    } else if want_quotes == b'x' {
        temp = quote_breaks(&temp);
    }

    *end_index = i;
    Ok((temp, print_only))
}

#[no_mangle]
pub unsafe extern "C" fn history_expand(hstring: *const c_char, output: *mut *mut c_char) -> c_int {
    if output.is_null() {
        return 0;
    }
    let input = c_str_bytes(hstring);
    let expansion_character = history_expansion_character();
    if expansion_character == 0 {
        *output = duplicate_bytes(input);
        return 0;
    }

    let quick = (quoting_state() != b'\'' || !quotes_inhibit_expansion())
        && input.first() == Some(&history_subst_character());
    let string = if quick {
        let mut prefixed = Vec::with_capacity(input.len() + 4);
        prefixed.push(expansion_character);
        prefixed.push(expansion_character);
        prefixed.push(b':');
        prefixed.push(b's');
        prefixed.extend_from_slice(input);
        prefixed
    } else {
        input.to_vec()
    };
    let length = string.len();

    if !quick {
        let mut dquote = quoting_state() == b'"';
        let squote = quoting_state() == b'\'';
        let no_expand = no_expand_characters();
        let word_delims = word_delimiters();
        let mut i = 0_usize;

        if squote && quotes_inhibit_expansion() {
            hist_string_extract_single_quoted(&string, &mut i, false);
            if at(&string, i) != 0 {
                i += 1;
            }
            if i >= length {
                i = length;
            }
        }

        while i < string.len() {
            let character = string[i];
            let next = at(&string, i + 1);

            if history_comment() != 0
                && character == history_comment()
                && !dquote
                && (i == 0 || member(string[i - 1], &word_delims))
            {
                i = string.len();
                break;
            } else if character == expansion_character {
                if next == 0 || member(next, &no_expand) || (dquote && next == b'"') {
                    i += 1;
                    continue;
                } else if let Some(callback) = inhibit_expansion_function() {
                    let mut buffer = string.clone();
                    buffer.push(0);
                    if callback(buffer.as_mut_ptr() as *mut c_char, i as c_int) != 0 {
                        i += 1;
                        continue;
                    }
                }
                break;
            } else if dquote && character == b'\\' && next == b'"' {
                i += 1;
            } else if quotes_inhibit_expansion() && character == b'"' {
                dquote = !dquote;
            } else if !dquote && quotes_inhibit_expansion() && character == b'\'' {
                let flag = i > 0 && string[i - 1] == b'$';
                i += 1;
                hist_string_extract_single_quoted(&string, &mut i, flag);
                if i >= length {
                    i = length;
                    break;
                }
            } else if quotes_inhibit_expansion()
                && character == b'\\'
                && (next == b'\'' || next == expansion_character)
            {
                i += 1;
            }

            i += 1;
        }

        if at(&string, i) != expansion_character {
            *output = duplicate_bytes(&string);
            return 0;
        }
    }

    let mut dquote = quoting_state() == b'"';
    let mut squote = quoting_state() == b'\'';
    let no_expand = no_expand_characters();
    let word_delims = word_delimiters();
    let mut result: Vec<u8> = Vec::new();
    let mut i = 0_usize;
    let mut passc = false;
    let mut modified = 0_i32;
    let mut only_printing = false;

    if squote && quotes_inhibit_expansion() {
        hist_string_extract_single_quoted(&string, &mut i, false);
        squote = false;
        result.extend_from_slice(&string[..i]);
        if at(&string, i) != 0 {
            result.push(string[i]);
            i += 1;
        }
    }

    enum HistoryCharacter {
        Expansion,
        Comment,
        Literal(u8),
    }

    while i < length {
        let character = string[i];

        if passc {
            passc = false;
            result.push(character);
            i += 1;
            continue;
        }

        let token = if character == expansion_character {
            HistoryCharacter::Expansion
        } else if history_comment() != 0 && character == history_comment() {
            HistoryCharacter::Comment
        } else {
            HistoryCharacter::Literal(character)
        };

        match token {
            HistoryCharacter::Expansion => {
                let next = at(&string, i + 1);
                if next == 0 || member(next, &no_expand) || (dquote && next == b'"') {
                    result.push(expansion_character);
                    i += 1;
                    continue;
                }

                if let Some(callback) = inhibit_expansion_function() {
                    let save_j = result.len();
                    result.push(expansion_character);
                    result.push(next);
                    result.push(0);
                    let inhibit =
                        callback(result.as_mut_ptr() as *mut c_char, save_j as c_int) != 0;
                    if inhibit {
                        result.truncate(save_j + 1);
                        i += 1;
                        continue;
                    }
                    result.truncate(save_j);
                }

                let qc = if squote {
                    c_int::from(b'\'')
                } else if dquote {
                    c_int::from(b'"')
                } else {
                    0
                };
                let mut end_index = i;
                match history_expand_internal(&string, i, qc, &mut end_index, &result) {
                    Err(message) => {
                        *output = duplicate_bytes(&message);
                        return -1;
                    }
                    Ok((replacement, print)) => {
                        modified += 1;
                        if !replacement.is_empty() {
                            result.extend_from_slice(&replacement);
                        }
                        only_printing |= print;
                        i = end_index;
                    }
                }
            }
            HistoryCharacter::Comment => {
                if (!dquote || !quotes_inhibit_expansion())
                    && (i == 0 || member(string[i - 1], &word_delims))
                {
                    result.extend_from_slice(&string[i..]);
                    i = length;
                } else {
                    result.push(string[i]);
                }
            }
            HistoryCharacter::Literal(b'\\') => {
                passc = true;
                result.push(b'\\');
            }
            HistoryCharacter::Literal(b'"') => {
                dquote = !dquote;
                result.push(b'"');
            }
            HistoryCharacter::Literal(b'\'') => {
                if squote {
                    squote = false;
                    result.push(b'\'');
                } else if !dquote && quotes_inhibit_expansion() {
                    let flag = i > 0 && string[i - 1] == b'$';
                    let quote = i;
                    i += 1;
                    hist_string_extract_single_quoted(&string, &mut i, flag);
                    let mut end = i + 1;
                    if end > string.len() {
                        end = string.len();
                    }
                    result.extend_from_slice(&string[quote..end]);
                } else if !dquote && !quotes_inhibit_expansion() {
                    squote = true;
                    result.push(b'\'');
                } else {
                    result.push(b'\'');
                }
            }
            HistoryCharacter::Literal(byte) => result.push(byte),
        }
        i += 1;
    }

    *output = duplicate_bytes(&result);
    if only_printing {
        return 2;
    }
    c_int::from(modified != 0)
}

fn read_history_impl(filename: *const c_char, from: c_int, to: c_int) -> c_int {
    let Some(path) = history_path(filename) else {
        return 0;
    };
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) => return errno_from(&error),
    };

    let mut state = history();
    let mut pending_timestamp: Option<Vec<u8>> = None;
    let mut line_number = 0_i32;
    let mut added = 0_i32;

    for raw_line in data.split(|byte| *byte == b'\n') {
        let line = strip_line_ending(raw_line);
        if line.is_empty() {
            continue;
        }

        let in_range =
            (from <= 0 || line_number >= from) && (to < 0 || to < from || line_number <= to);
        line_number += 1;

        if is_timestamp_line(line) {
            if history_comment() == 0 {
                set_history_comment(b'#');
            }
            if in_range {
                pending_timestamp = Some(line.to_vec());
            }
            continue;
        }

        if !in_range {
            continue;
        }

        let timestamp = pending_timestamp.take();
        if push_entry(&mut state, line, timestamp.as_deref(), ptr::null_mut()) {
            added += 1;
        }
    }

    unsafe {
        ptr::addr_of_mut!(history_lines_read_from_file).write(added);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn read_history(filename: *const c_char) -> c_int {
    read_history_impl(filename, 0, -1)
}

#[no_mangle]
pub unsafe extern "C" fn read_history_range(
    filename: *const c_char,
    from: c_int,
    to: c_int,
) -> c_int {
    read_history_impl(filename, from, to)
}

fn write_entries(
    file: &mut File,
    state: &HistoryState,
    start: usize,
    timestamps: bool,
) -> io::Result<c_int> {
    for entry in &state.entries[start..] {
        write_entry(file, &entry.handle, timestamps)?;
    }
    Ok((state.entries.len() - start) as c_int)
}

#[no_mangle]
pub unsafe extern "C" fn write_history(filename: *const c_char) -> c_int {
    let Some(path) = history_path(filename) else {
        return 0;
    };
    let state = history();
    let timestamps = ptr::addr_of!(history_write_timestamps).read() != 0;

    let mut file = match File::create(path) {
        Ok(file) => file,
        Err(error) => return errno_from(&error),
    };

    let result = write_entries(&mut file, &state, 0, timestamps);
    match result {
        Ok(lines) => {
            ptr::addr_of_mut!(history_lines_written_to_file).write(lines);
            0
        }
        Err(error) => errno_from(&error),
    }
}

#[no_mangle]
pub unsafe extern "C" fn append_history(nelements: c_int, filename: *const c_char) -> c_int {
    let Some(path) = history_path(filename) else {
        return 0;
    };
    let state = history();
    if nelements <= 0 {
        ptr::addr_of_mut!(history_lines_written_to_file).write(0);
        return 0;
    }

    let start = state.entries.len().saturating_sub(nelements as usize);
    let timestamps = ptr::addr_of!(history_write_timestamps).read() != 0;

    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => file,
        Err(error) => return errno_from(&error),
    };

    match write_entries(&mut file, &state, start, timestamps) {
        Ok(lines) => {
            ptr::addr_of_mut!(history_lines_written_to_file).write(lines);
            0
        }
        Err(error) => errno_from(&error),
    }
}

#[no_mangle]
pub unsafe extern "C" fn history_truncate_file(filename: *const c_char, nlines: c_int) -> c_int {
    let Some(path) = history_path(filename) else {
        return 0;
    };
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(error) => return errno_from(&error),
    };

    let keep = nlines.max(0) as usize;
    let mut lines: Vec<&[u8]> = data
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() > keep {
        lines.drain(..lines.len() - keep);
    }

    let mut output = Vec::new();
    for line in lines {
        output.extend_from_slice(strip_line_ending(line));
        output.push(b'\n');
    }

    match fs::write(path, output) {
        Ok(()) => 0,
        Err(error) => errno_from(&error),
    }
}

// --- Internal data-pointer maintenance (undo integration) ----------------
//
// GNU stores an `UNDO_LIST *` in `HIST_ENTRY.data`.  The undo module has to
// rewrite those pointers when it frees or advances a list, so expose the
// equivalent of histlib.c `_hs_replace_history_data` to the crate.

/// Replace every history entry whose `data` pointer equals `old` with `new`.
pub(crate) unsafe fn replace_history_data(old: *mut c_void, new: *mut c_void) {
    let mut state = history();
    for entry in state.entries.iter_mut() {
        if entry.handle.data == old {
            entry.handle.data = new;
        }
    }
}

/// Clear the `data` pointer of the entry at the current history offset.
pub(crate) unsafe fn clear_current_history_data() {
    let mut state = history();
    let offset = history_offset_value();
    if offset >= 0 && (offset as usize) < state.entries.len() {
        state.entries[offset as usize].handle.data = ptr::null_mut();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::MutexGuard;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn c_string(value: &str) -> CString {
        CString::new(value).unwrap()
    }

    fn take_string(value: *mut c_char) -> Option<String> {
        if value.is_null() {
            None
        } else {
            Some(
                unsafe { CStr::from_ptr(value) }
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    fn take_and_free(value: *mut c_char) -> Option<String> {
        let text = take_string(value);
        unsafe { libc::free(value as *mut c_void) };
        text
    }

    fn tokenized(value: *mut *mut c_char) -> Vec<String> {
        if value.is_null() {
            return Vec::new();
        }
        let mut words = Vec::new();
        unsafe {
            let mut index = 0;
            while !(*value.add(index)).is_null() {
                words.push(take_string(*value.add(index)).unwrap());
                libc::free(*value.add(index) as *mut c_void);
                index += 1;
            }
            libc::free(value as *mut c_void);
        }
        words
    }

    #[test]
    fn timestamp_lines_are_recognised() {
        assert!(is_timestamp_line(b"#12345"));
        assert!(!is_timestamp_line(b"#comment"));
        assert!(!is_timestamp_line(b"12345"));
        assert!(!is_timestamp_line(b""));
    }

    #[test]
    fn trailing_carriage_return_is_stripped() {
        assert_eq!(strip_line_ending(b"echo hi\r"), b"echo hi");
        assert_eq!(strip_line_ending(b"echo hi"), b"echo hi");
    }

    #[test]
    fn list_state_remove_and_replace_follow_gnu_ownership() {
        let _guard = guard();
        unsafe {
            clear_history();

            for line in ["one", "two", "three"] {
                let line = c_string(line);
                add_history(line.as_ptr());
            }

            let list = history_list();
            assert!(!list.is_null());
            assert!(!(*list).is_null());
            assert_eq!(take_string((**list).line).as_deref(), Some("one"));
            assert!((*list.add(3)).is_null());
            assert_eq!(history_length_value(), 3);

            let snapshot = history_get_history_state();
            assert!(!snapshot.is_null());
            assert_eq!((*snapshot).length, 3);
            assert_eq!((*snapshot).offset, 0);
            assert_eq!((*snapshot).entries, list);
            assert_eq!((*snapshot).flags, 0);
            (*snapshot).offset = 1;
            history_set_history_state(snapshot);
            assert_eq!(where_history(), 1);
            libc::free(snapshot as *mut c_void);

            let removed = remove_history(1);
            assert!(!removed.is_null());
            assert_eq!(take_string((*removed).line).as_deref(), Some("two"));
            assert_eq!(history_length_value(), 2);
            assert_eq!(free_history_entry(removed), ptr::null_mut());
            assert_eq!(
                take_string((*(*history_list().add(1))).line).as_deref(),
                Some("three")
            );

            let old = replace_history_entry(1, c_string("THREE").as_ptr(), 0x1234 as *mut c_void);
            assert!(!old.is_null());
            assert_eq!(take_string((*old).line).as_deref(), Some("three"));
            let replaced = history_get(history_base + 1);
            assert_eq!(take_string((*replaced).line).as_deref(), Some("THREE"));
            assert_eq!((*replaced).data as usize, 0x1234);
            free_history_entry(old);
            assert!(replace_history_entry(9, c_string("x").as_ptr(), ptr::null_mut()).is_null());

            let allocated = alloc_history_entry(
                c_string("line").as_ptr() as *mut c_char,
                duplicate_bytes(b"#999"),
            );
            assert!(!allocated.is_null());
            assert_eq!(take_string((*allocated).line).as_deref(), Some("line"));
            let copy = copy_history_entry(allocated);
            assert!(!copy.is_null());
            assert_ne!((*copy).line, (*allocated).line);
            assert_eq!(take_string((*copy).timestamp).as_deref(), Some("#999"));
            free_history_entry(allocated);
            free_history_entry(copy);

            let range = remove_history_range(0, 0);
            assert!(!range.is_null());
            assert_eq!(take_string((**range).line).as_deref(), Some("one"));
            assert!((*range.add(1)).is_null());
            free_history_entry(*range);
            libc::free(range as *mut c_void);
            assert_eq!(history_length_value(), 1);
            clear_history();
            // GNU clear_history frees the array and leaves the_history NULL.
            assert!(history_list().is_null());
        }
    }

    #[test]
    fn search_direction_prefix_and_position_match_gnu() {
        let _guard = guard();
        unsafe {
            clear_history();
            for line in ["alpha one", "beta two", "alphabet", "gamma alpha"] {
                let line = c_string(line);
                add_history(line.as_ptr());
            }
            using_history();

            assert_eq!(history_search(c_string("alpha").as_ptr(), -1), 6);
            assert_eq!(where_history(), 3);
            history_set_pos(2);
            assert_eq!(history_search(c_string("alpha").as_ptr(), -1), 0);
            assert_eq!(where_history(), 2);

            history_set_pos(0);
            assert_eq!(history_search(c_string("alpha").as_ptr(), 1), 0);
            assert_eq!(where_history(), 0);
            history_set_pos(1);
            assert_eq!(history_search(c_string("alpha").as_ptr(), 1), 0);
            assert_eq!(where_history(), 2);

            history_set_pos(4);
            assert_eq!(history_search_prefix(c_string("alpha").as_ptr(), -1), 0);
            assert_eq!(where_history(), 2);
            history_set_pos(0);
            assert_eq!(history_search_prefix(c_string("alpha").as_ptr(), 1), 0);
            assert_eq!(where_history(), 0);
            history_set_pos(4);
            assert_eq!(history_search_prefix(c_string("zzz").as_ptr(), -1), -1);
            history_set_pos(2);

            assert_eq!(history_search_pos(c_string("alpha").as_ptr(), -1, 2), 2);
            assert_eq!(where_history(), 2);
            assert_eq!(history_search_pos(c_string("alpha").as_ptr(), 1, 0), 0);
            assert_eq!(history_search_pos(c_string("zzz").as_ptr(), -1, 2), -1);
            assert_eq!(history_search_pos(c_string("alpha").as_ptr(), -1, 9), 2);
            assert_eq!(where_history(), 2);
            clear_history();
        }
    }

    #[test]
    fn expansion_arg_extract_and_tokenize_match_gnu() {
        let _guard = guard();
        unsafe {
            clear_history();
            for line in ["first", "second arg", "third thing here"] {
                let line = c_string(line);
                add_history(line.as_ptr());
            }
            using_history();

            let expand = |input: &str| {
                let mut output: *mut c_char = ptr::null_mut();
                let rc = history_expand(c_string(input).as_ptr(), &mut output);
                (rc, take_and_free(output))
            };

            assert_eq!(expand("echo !!"), (1, Some("echo third thing here".into())));
            assert_eq!(expand("echo !2"), (1, Some("echo second arg".into())));
            assert_eq!(expand("echo !sec"), (1, Some("echo second arg".into())));
            assert_eq!(
                expand("echo !nosuch"),
                (-1, Some("!nosuch: event not found".into()))
            );
            assert_eq!(expand("echo !!:$"), (1, Some("echo here".into())));
            assert_eq!(expand("echo !!:*"), (1, Some("echo thing here".into())));
            assert_eq!(
                expand("echo !!:q"),
                (1, Some("echo 'third thing here'".into()))
            );
            assert_eq!(
                expand("echo !!:p"),
                (2, Some("echo third thing here".into()))
            );
            assert_eq!(
                expand("echo !!:s/third/THIRD/"),
                (1, Some("echo THIRD thing here".into()))
            );
            assert_eq!(
                expand("echo '!!' \"!!\""),
                (
                    1,
                    Some("echo 'third thing here' \"third thing here\"".into())
                )
            );
            assert_eq!(expand("echo \\!!"), (0, Some("echo \\!!".into())));

            assert_eq!(
                take_and_free(history_arg_extract(
                    1,
                    2,
                    c_string("cmd one two three").as_ptr()
                ))
                .as_deref(),
                Some("one two")
            );
            assert_eq!(
                take_and_free(history_arg_extract(0, 0, c_string("cmd one two").as_ptr()))
                    .as_deref(),
                Some("cmd")
            );
            assert!(history_arg_extract(9, 9, c_string("cmd one").as_ptr()).is_null());

            assert_eq!(
                tokenized(history_tokenize(c_string("echo 'a b' \"c d\"").as_ptr())),
                vec!["echo", "'a b'", "\"c d\""]
            );
            assert_eq!(
                tokenized(history_tokenize(c_string("foo|bar;baz").as_ptr())),
                vec!["foo", "|", "bar", ";", "baz"]
            );
            assert_eq!(
                tokenized(history_tokenize(c_string("a\\ b c").as_ptr())),
                vec!["a\\ b", "c"]
            );
            assert!(history_tokenize(c_string("").as_ptr()).is_null());

            clear_history();
        }
    }

    #[test]
    fn truncate_file_keeps_the_requested_tail() {
        let _guard = guard();
        let path = std::env::temp_dir().join(format!("ruline-history-{}", std::process::id()));
        std::fs::write(&path, b"one\ntwo\nthree\nfour\n").unwrap();
        let filename = CString::new(path.to_str().unwrap()).unwrap();
        unsafe {
            assert_eq!(history_truncate_file(filename.as_ptr(), 2), 0);
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"three\nfour\n");
        let _ = std::fs::remove_file(&path);
    }
}
