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
}

static HISTORY: Mutex<HistoryState> = Mutex::new(HistoryState {
    entries: Vec::new(),
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

// --- History file I/O -----------------------------------------------------

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
    let mut lines = 0_i32;
    for entry in &state.entries[start..] {
        lines += write_entry(file, &entry.handle, timestamps)? as c_int;
    }
    Ok(lines)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
