//! Completion globals, hooks, and the application-callable completion API.
//!
//! This module is a clean-room implementation of the completion half of the
//! GNU readline 8.x C ABI.  The vendored GNU `complete.c` is the behavioural
//! oracle: word-break/quote scanning, the generator protocol, lowest-common-
//! denominator computation, append/quote rules, and the match-list layout all
//! follow it.  Nothing here is routed through the live ruline editor; a host
//! that owns a `Reedline` instance integrates these entry points separately.
//!
//! Exported variables are real `static mut` data symbols.  Rust code never
//! forms references to them; access goes through `addr_of!`/`addr_of_mut!` so
//! the ABI layout is stable without creating aliasing references.

use std::ffi::{CStr, OsString};
use std::io::Write;
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::line;
use crate::state;

// --- Public callback types ------------------------------------------------

/// `char *(*)(const char *, int)` — completion generator.
pub type RlCompEntryFunc = Option<unsafe extern "C" fn(*const c_char, c_int) -> *mut c_char>;
/// `char **(*)(const char *, int, int)` — attempted completion function.
pub type RlCompletionFunc =
    Option<unsafe extern "C" fn(*const c_char, c_int, c_int) -> *mut *mut c_char>;
/// `char *(*)(char *, int, char *)` — filename quoting function.
pub type RlQuoteFunc = Option<unsafe extern "C" fn(*mut c_char, c_int, *mut c_char) -> *mut c_char>;
/// `char *(*)(char *, int)` — filename dequoting/rewrite function.
pub type RlDequoteFunc = Option<unsafe extern "C" fn(*mut c_char, c_int) -> *mut c_char>;
/// `void (*)(char **, int, int)` — match display hook.
pub type RlCompDispFunc = Option<unsafe extern "C" fn(*mut *mut c_char, c_int, c_int)>;
/// `int (*)(char **)` — directory rewrite/stat hook.
pub type RlIcppFunc = Option<unsafe extern "C" fn(*mut *mut c_char) -> c_int>;
/// `char *(*)(void)` — word-break hook.
pub type RlCpvFunc = Option<unsafe extern "C" fn() -> *mut c_char>;
/// `int (*)(char *, int)` — "is this character quoted?" predicate.
pub type RlLinebufFunc = Option<unsafe extern "C" fn(*mut c_char, c_int) -> c_int>;
/// `int (*)(void)` — startup/pre-input/event hook.
pub type RlHookFunc = Option<unsafe extern "C" fn() -> c_int>;
/// `int (*)(FILE *)` — character input function.
pub type RlGetcFunc = Option<unsafe extern "C" fn(*mut libc::FILE) -> c_int>;
/// `void (*)(void)` — redisplay / terminal-deprep function.
pub type RlVoidFunc = Option<unsafe extern "C" fn()>;
/// `void (*)(int)` — terminal-prep function.
pub type RlVintFunc = Option<unsafe extern "C" fn(c_int)>;

// --- Defaults -------------------------------------------------------------

// `\t` and `\n` are escaped here; the backtick is literal.
static BASIC_WORD_BREAK_CHARS: [u8; 18] = *b" \t\n\"\\'`@$><=;|&{(\0";
static BASIC_QUOTE_CHARS: [u8; 3] = *b"\"'\0";

const RL_QF_SINGLE_QUOTE: c_int = 0x01;
const RL_QF_DOUBLE_QUOTE: c_int = 0x02;
const RL_QF_BACKSLASH: c_int = 0x04;
const RL_QF_OTHER_QUOTE: c_int = 0x08;

const NO_MATCH: c_int = 0;
const SINGLE_MATCH: c_int = 1;
const MULT_MATCH: c_int = 2;

const TAB: c_int = b'\t' as c_int;
const EXCLAMATION: c_int = b'!' as c_int;
const AT: c_int = b'@' as c_int;
const STAR: c_int = b'*' as c_int;
const QUESTION: c_int = b'?' as c_int;
const PERCENT: c_int = b'%' as c_int;
const PIPE: c_int = b'|' as c_int;
const DEFAULT_COMPLETION_COLUMNS: c_int = 80;

// --- Exported completion state -------------------------------------------

#[no_mangle]
pub static mut rl_completion_entry_function: RlCompEntryFunc = None;
#[no_mangle]
pub static mut rl_menu_completion_entry_function: RlCompEntryFunc = None;
#[no_mangle]
pub static mut rl_attempted_completion_function: RlCompletionFunc = None;
#[no_mangle]
pub static mut rl_completion_display_matches_hook: RlCompDispFunc = None;
#[no_mangle]
pub static mut rl_completion_word_break_hook: RlCpvFunc = None;

#[no_mangle]
pub static mut rl_filename_rewrite_hook: RlDequoteFunc = None;
#[no_mangle]
pub static mut rl_filename_stat_hook: RlIcppFunc = None;
#[no_mangle]
pub static mut rl_directory_rewrite_hook: RlIcppFunc = None;
#[no_mangle]
pub static mut rl_filename_dequoting_function: RlDequoteFunc = None;
/// GNU's default filename quoting function: prefix the opening quote
/// character from `rl_completer_quote_characters` and report it back.
unsafe extern "C" fn default_quote_filename(
    text: *mut c_char,
    _match_type: c_int,
    quote_character: *mut c_char,
) -> *mut c_char {
    let set = completer_quote_characters();
    if set.is_null() {
        return bytes_to_c(c_bytes(text));
    }
    let opening = first_byte(set);
    if opening == 0 {
        return bytes_to_c(c_bytes(text));
    }
    if !quote_character.is_null() {
        *quote_character = opening as c_char;
    }
    let mut quoted = Vec::with_capacity(c_bytes(text).len() + 1);
    quoted.push(opening);
    quoted.extend_from_slice(c_bytes(text));
    bytes_to_c(&quoted)
}

#[no_mangle]
pub static mut rl_filename_quoting_function: RlQuoteFunc = Some(default_quote_filename);
#[no_mangle]
pub static mut rl_char_is_quoted_p: RlLinebufFunc = None;

#[no_mangle]
pub static mut rl_completer_word_break_characters: *mut c_char =
    BASIC_WORD_BREAK_CHARS.as_ptr() as *mut c_char;
#[no_mangle]
pub static mut rl_completer_quote_characters: *mut c_char = ptr::null_mut();
#[no_mangle]
pub static mut rl_filename_quote_characters: *mut c_char = ptr::null_mut();
#[no_mangle]
pub static mut rl_basic_word_break_characters: *mut c_char =
    BASIC_WORD_BREAK_CHARS.as_ptr() as *mut c_char;
#[no_mangle]
pub static mut rl_basic_quote_characters: *mut c_char = BASIC_QUOTE_CHARS.as_ptr() as *mut c_char;
#[no_mangle]
pub static mut rl_special_prefixes: *mut c_char = ptr::null_mut();

#[no_mangle]
pub static mut rl_attempted_completion_over: c_int = 0;
#[no_mangle]
pub static mut rl_completion_type: c_int = 0;
#[no_mangle]
pub static mut rl_completion_invoking_key: c_int = 0;
#[no_mangle]
pub static mut rl_completion_suppress_append: c_int = 0;
#[no_mangle]
pub static mut rl_completion_suppress_quote: c_int = 0;
#[no_mangle]
pub static mut rl_completion_append_character: c_int = b' ' as c_int;
#[no_mangle]
pub static mut rl_completion_quote_character: c_int = 0;
#[no_mangle]
pub static mut rl_completion_found_quote: c_int = 0;
#[no_mangle]
pub static mut rl_completion_mark_symlink_dirs: c_int = 0;
#[no_mangle]
pub static mut rl_completion_query_items: c_int = 100;

#[no_mangle]
pub static mut rl_filename_completion_desired: c_int = 0;
#[no_mangle]
pub static mut rl_filename_quoting_desired: c_int = 1;
#[no_mangle]
pub static mut rl_full_quoting_desired: c_int = 0;
#[no_mangle]
pub static mut rl_ignore_completion_duplicates: c_int = 1;
#[no_mangle]
pub static mut rl_sort_completion_matches: c_int = 1;
#[no_mangle]
pub static mut rl_inhibit_completion: c_int = 0;

// GNU keeps this in rlprivate.h rather than the public header.  It stays
// private here too so the exported surface matches the published ABI.
static mut COMPLETE_WITH_TILDE_EXPANSION: c_int = 0;

// --- Exported readline hooks ---------------------------------------------

#[no_mangle]
pub static mut rl_startup_hook: RlHookFunc = None;
#[no_mangle]
pub static mut rl_pre_input_hook: RlHookFunc = None;
#[no_mangle]
pub static mut rl_event_hook: RlHookFunc = None;
#[no_mangle]
pub static mut rl_input_available_hook: RlHookFunc = None;
#[no_mangle]
pub static mut rl_getc_function: RlGetcFunc = None;
#[no_mangle]
pub static mut rl_redisplay_function: RlVoidFunc = Some(line::rl_redisplay);
#[no_mangle]
pub static mut rl_prep_term_function: RlVintFunc = None;
#[no_mangle]
pub static mut rl_deprep_term_function: RlVoidFunc = None;

// --- Accessors ------------------------------------------------------------

macro_rules! global_accessors {
    ($get:ident, $set:ident, $value:ident, $ty:ty) => {
        #[allow(dead_code)]
        fn $get() -> $ty {
            unsafe { ptr::addr_of!($value).read() }
        }

        #[allow(dead_code)]
        fn $set(value: $ty) {
            unsafe { ptr::addr_of_mut!($value).write(value) }
        }
    };
}

global_accessors!(
    completion_entry_function,
    set_completion_entry_function,
    rl_completion_entry_function,
    RlCompEntryFunc
);
global_accessors!(
    menu_completion_entry_function,
    set_menu_completion_entry_function,
    rl_menu_completion_entry_function,
    RlCompEntryFunc
);
global_accessors!(
    attempted_completion_function,
    set_attempted_completion_function,
    rl_attempted_completion_function,
    RlCompletionFunc
);
global_accessors!(
    display_matches_hook,
    set_display_matches_hook,
    rl_completion_display_matches_hook,
    RlCompDispFunc
);
global_accessors!(
    word_break_hook,
    set_word_break_hook,
    rl_completion_word_break_hook,
    RlCpvFunc
);
global_accessors!(
    filename_rewrite_hook,
    set_filename_rewrite_hook,
    rl_filename_rewrite_hook,
    RlDequoteFunc
);
global_accessors!(
    filename_stat_hook,
    set_filename_stat_hook,
    rl_filename_stat_hook,
    RlIcppFunc
);
global_accessors!(
    directory_rewrite_hook,
    set_directory_rewrite_hook,
    rl_directory_rewrite_hook,
    RlIcppFunc
);
global_accessors!(
    filename_dequoting_function,
    set_filename_dequoting_function,
    rl_filename_dequoting_function,
    RlDequoteFunc
);
global_accessors!(
    filename_quoting_function,
    set_filename_quoting_function,
    rl_filename_quoting_function,
    RlQuoteFunc
);
global_accessors!(
    char_is_quoted_p,
    set_char_is_quoted_p,
    rl_char_is_quoted_p,
    RlLinebufFunc
);

global_accessors!(
    completer_word_break_characters,
    set_completer_word_break_characters,
    rl_completer_word_break_characters,
    *mut c_char
);
global_accessors!(
    completer_quote_characters,
    set_completer_quote_characters,
    rl_completer_quote_characters,
    *mut c_char
);
global_accessors!(
    filename_quote_characters,
    set_filename_quote_characters,
    rl_filename_quote_characters,
    *mut c_char
);
global_accessors!(
    basic_word_break_characters,
    set_basic_word_break_characters,
    rl_basic_word_break_characters,
    *mut c_char
);
global_accessors!(
    basic_quote_characters,
    set_basic_quote_characters,
    rl_basic_quote_characters,
    *mut c_char
);
global_accessors!(
    special_prefixes,
    set_special_prefixes,
    rl_special_prefixes,
    *mut c_char
);

global_accessors!(
    attempted_completion_over,
    set_attempted_completion_over,
    rl_attempted_completion_over,
    c_int
);
global_accessors!(
    completion_type,
    set_completion_type,
    rl_completion_type,
    c_int
);
global_accessors!(
    completion_invoking_key,
    set_completion_invoking_key,
    rl_completion_invoking_key,
    c_int
);
global_accessors!(
    completion_suppress_append,
    set_completion_suppress_append,
    rl_completion_suppress_append,
    c_int
);
global_accessors!(
    completion_suppress_quote,
    set_completion_suppress_quote,
    rl_completion_suppress_quote,
    c_int
);
global_accessors!(
    completion_append_character,
    set_completion_append_character,
    rl_completion_append_character,
    c_int
);
global_accessors!(
    completion_quote_character,
    set_completion_quote_character,
    rl_completion_quote_character,
    c_int
);
global_accessors!(
    completion_found_quote,
    set_completion_found_quote,
    rl_completion_found_quote,
    c_int
);
global_accessors!(
    completion_mark_symlink_dirs,
    set_completion_mark_symlink_dirs,
    rl_completion_mark_symlink_dirs,
    c_int
);
global_accessors!(
    completion_query_items,
    set_completion_query_items,
    rl_completion_query_items,
    c_int
);
global_accessors!(
    filename_completion_desired,
    set_filename_completion_desired,
    rl_filename_completion_desired,
    c_int
);
global_accessors!(
    filename_quoting_desired,
    set_filename_quoting_desired,
    rl_filename_quoting_desired,
    c_int
);
global_accessors!(
    full_quoting_desired,
    set_full_quoting_desired,
    rl_full_quoting_desired,
    c_int
);
global_accessors!(
    ignore_completion_duplicates,
    set_ignore_completion_duplicates,
    rl_ignore_completion_duplicates,
    c_int
);
global_accessors!(
    sort_completion_matches,
    set_sort_completion_matches,
    rl_sort_completion_matches,
    c_int
);
global_accessors!(
    inhibit_completion,
    set_inhibit_completion,
    rl_inhibit_completion,
    c_int
);
global_accessors!(
    complete_with_tilde_expansion,
    set_complete_with_tilde_expansion,
    COMPLETE_WITH_TILDE_EXPANSION,
    c_int
);

global_accessors!(startup_hook, set_startup_hook, rl_startup_hook, RlHookFunc);
global_accessors!(
    pre_input_hook,
    set_pre_input_hook,
    rl_pre_input_hook,
    RlHookFunc
);
global_accessors!(event_hook, set_event_hook, rl_event_hook, RlHookFunc);
global_accessors!(
    input_available_hook,
    set_input_available_hook,
    rl_input_available_hook,
    RlHookFunc
);
global_accessors!(
    getc_function,
    set_getc_function,
    rl_getc_function,
    RlGetcFunc
);
global_accessors!(
    redisplay_function,
    set_redisplay_function,
    rl_redisplay_function,
    RlVoidFunc
);
global_accessors!(
    prep_term_function,
    set_prep_term_function,
    rl_prep_term_function,
    RlVintFunc
);
global_accessors!(
    deprep_term_function,
    set_deprep_term_function,
    rl_deprep_term_function,
    RlVoidFunc
);

// --- Internal state -------------------------------------------------------

static COMPLETION_CHANGED_BUFFER: AtomicBool = AtomicBool::new(false);
static LAST_COMPLETION_FAILED: AtomicBool = AtomicBool::new(false);
static DISPLAY_INTERRUPTED: AtomicBool = AtomicBool::new(false);

struct FilenameCompletionState {
    dirname: Vec<u8>,
    users_dirname: Vec<u8>,
    entries: Vec<Vec<u8>>,
    index: usize,
}

static FILENAME_COMPLETION: Mutex<Option<FilenameCompletionState>> = Mutex::new(None);

struct UsernameCompletionState {
    first_char: u8,
    names: Vec<Vec<u8>>,
    index: usize,
}

static USERNAME_COMPLETION: Mutex<Option<UsernameCompletionState>> = Mutex::new(None);

struct MenuState {
    orig_text: Vec<u8>,
    comparison_text: Vec<u8>,
    matches: Vec<Vec<u8>>,
    index: i32,
    size: i32,
    full_completion: bool,
    orig_start: c_int,
    quote_char: c_char,
    delimiter: c_int,
}

static MENU_COMPLETION: Mutex<Option<MenuState>> = Mutex::new(None);

// --- Small C helpers ------------------------------------------------------

fn c_bytes<'a>(value: *const c_char) -> &'a [u8] {
    if value.is_null() {
        &[]
    } else {
        // SAFETY: ABI callers pass NUL-terminated strings valid for the call.
        unsafe { CStr::from_ptr(value).to_bytes() }
    }
}

fn bytes_to_c(bytes: &[u8]) -> *mut c_char {
    let allocation = unsafe { libc::malloc(bytes.len() + 1) as *mut c_char };
    if allocation.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            state::char_ptr_as_u8(allocation),
            bytes.len(),
        );
        *allocation.add(bytes.len()) = 0;
    }
    allocation
}

fn free_c(value: *mut c_char) {
    if !value.is_null() {
        // SAFETY: pointers freed here were allocated with libc::malloc.
        unsafe { libc::free(value as *mut libc::c_void) };
    }
}

fn contains(set: *const c_char, byte: u8) -> bool {
    !set.is_null() && byte != 0 && c_bytes(set).contains(&byte)
}

/// `c_char` is signed on some targets and unsigned on others; this is the
/// single conversion point for reading a C character as an unsigned byte.
#[allow(clippy::unnecessary_cast)]
fn c_byte(value: c_char) -> u8 {
    value as u8
}

fn first_byte(value: *const c_char) -> u8 {
    if value.is_null() {
        0
    } else {
        // SAFETY: value points at a readable C character.
        c_byte(unsafe { *value })
    }
}

fn c_strings(matches: *mut *mut c_char) -> Vec<Vec<u8>> {
    let mut result = Vec::new();
    if matches.is_null() {
        return result;
    }
    let mut index = 0_isize;
    loop {
        // SAFETY: matches is a NULL-terminated array of C strings.
        let item = unsafe { *matches.offset(index) };
        if item.is_null() {
            break;
        }
        result.push(c_bytes(item).to_vec());
        index += 1;
    }
    result
}

fn free_match_list(matches: *mut *mut c_char) {
    if matches.is_null() {
        return;
    }
    let mut index = 0_isize;
    loop {
        // SAFETY: matches is a NULL-terminated array allocated by this module
        // or handed to us by an ABI completion function.
        let item = unsafe { *matches.offset(index) };
        if item.is_null() {
            break;
        }
        free_c(item);
        index += 1;
    }
    unsafe { libc::free(matches as *mut libc::c_void) };
}

fn match_list_len(matches: *mut *mut c_char) -> c_int {
    if matches.is_null() {
        return 0;
    }
    let mut index = 0_isize;
    loop {
        // SAFETY: NULL-terminated array.
        if unsafe { *matches.offset(index) }.is_null() {
            return index.min(c_int::MAX as isize) as c_int;
        }
        index += 1;
    }
}

// --- Line-buffer helpers --------------------------------------------------

fn line_bytes() -> Vec<u8> {
    let buffer = state::line_buffer();
    let end = state::end().max(0) as usize;
    if buffer.is_null() || end == 0 {
        Vec::new()
    } else {
        // SAFETY: rl_line_buffer has rl_end valid bytes and a trailing NUL.
        unsafe { std::slice::from_raw_parts(buffer as *const u8, end).to_vec() }
    }
}

fn line_byte_at(index: c_int) -> u8 {
    if index < 0 {
        return 0;
    }
    let buffer = state::line_buffer();
    if buffer.is_null() || index >= state::end() {
        return 0;
    }
    c_byte(unsafe { *buffer.add(index as usize) })
}

fn copy_text(start: c_int, end: c_int) -> Vec<u8> {
    let mut from = start.min(end).max(0);
    let mut to = start.max(end).max(0);
    let limit = state::end().max(0);
    from = from.min(limit);
    to = to.min(limit);
    if from >= to {
        return Vec::new();
    }
    let buffer = state::line_buffer();
    if buffer.is_null() {
        return Vec::new();
    }
    // SAFETY: the range is within rl_end.
    unsafe {
        std::slice::from_raw_parts(buffer.add(from as usize) as *const u8, (to - from) as usize)
            .to_vec()
    }
}

/// Replace the inclusive range `[start, end]` (GNU `_rl_replace_text`).
fn replace_text(start: c_int, end: c_int, replacement: &[u8]) {
    // SAFETY: the exported line primitives only touch the exported buffer.
    unsafe {
        if start <= end {
            crate::line::rl_delete_text(start, end + 1);
        }
        state::set_point(start.max(0).min(state::end()));
        if !replacement.is_empty() {
            let text = bytes_to_c(replacement);
            if !text.is_null() {
                crate::line::rl_insert_text(text);
                free_c(text);
            }
        }
    }
}

fn insert_text_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let text = bytes_to_c(bytes);
    if text.is_null() {
        return;
    }
    // SAFETY: NUL-terminated allocation owned for the call.
    unsafe { crate::line::rl_insert_text(text) };
    free_c(text);
}

// --- Hook invocation ------------------------------------------------------

/// Initialise completion globals that GNU sets during `rl_initialize`.
///
/// A custom word-break set installed before initialization is preserved; only
/// the NULL default is replaced, matching readline.c.
pub(crate) fn initialize_defaults() {
    if completer_word_break_characters().is_null() {
        set_completer_word_break_characters(basic_word_break_characters());
    }
}

pub(crate) fn run_startup_hook() {
    if let Some(hook) = startup_hook() {
        // SAFETY: the application owns this callback and promised this ABI.
        unsafe { hook() };
    }
}

pub(crate) fn run_pre_input_hook() {
    if let Some(hook) = pre_input_hook() {
        // SAFETY: the application owns this callback and promised this ABI.
        unsafe { hook() };
    }
}

/// Repaint through the application's redisplay hook when one is installed,
/// otherwise through our default renderer.  This mirrors GNU's dispatcher:
/// `rl_redisplay_function` is the indirection, `rl_redisplay` is the default.
pub(crate) fn redisplay_with_hook() {
    if let Some(hook) = redisplay_function() {
        // SAFETY: the application owns this callback and promised this ABI.
        unsafe { hook() };
    } else {
        // SAFETY: default renderer only touches exported state.
        unsafe { line::rl_redisplay() };
    }
}

fn display_matches_interrupted() -> bool {
    DISPLAY_INTERRUPTED.swap(false, Ordering::Relaxed)
}

// --- Paths and passwd -----------------------------------------------------

fn os_bytes(value: OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        value.into_vec()
    }
    #[cfg(not(unix))]
    {
        value.to_string_lossy().into_owned().into_bytes()
    }
}

fn bytes_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Parse `/etc/passwd`-style records.  Returns `(name, home)` pairs.
///
/// Platforms without `/etc/passwd` (notably Windows) get an empty list, so
/// username completion and `~user` expansion report no matches instead of
/// failing.  This is the documented platform fallback for the standalone ABI.
fn passwd_entries() -> Vec<(Vec<u8>, Vec<u8>)> {
    let Ok(data) = std::fs::read("/etc/passwd") else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for line in data.split(|byte| *byte == b'\n') {
        let fields: Vec<&[u8]> = line.split(|byte| *byte == b':').collect();
        if fields.len() >= 6 {
            entries.push((fields[0].to_vec(), fields[5].to_vec()));
        }
    }
    entries
}

fn home_for_user(user: &[u8]) -> Option<Vec<u8>> {
    passwd_entries()
        .into_iter()
        .find(|(name, _)| name == user)
        .map(|(_, home)| home)
}

fn tilde_expand(bytes: &[u8]) -> Vec<u8> {
    if !bytes.starts_with(b"~") {
        return bytes.to_vec();
    }
    let rest = &bytes[1..];
    let (user, tail) = match rest.iter().position(|byte| *byte == b'/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, &b""[..]),
    };

    let home = if user.is_empty() {
        std::env::var_os("HOME")
            .map(os_bytes)
            .filter(|home| !home.is_empty())
    } else {
        home_for_user(user)
    };

    match home {
        Some(mut home) => {
            home.extend_from_slice(tail);
            home
        }
        None => bytes.to_vec(),
    }
}

fn apply_cpp_hook(hook: RlIcppFunc, bytes: &[u8]) -> Vec<u8> {
    let Some(hook) = hook else {
        return bytes.to_vec();
    };
    let allocation = bytes_to_c(bytes);
    if allocation.is_null() {
        return bytes.to_vec();
    }
    let mut pointer = allocation;
    // SAFETY: hook receives the address of a mutable C string and may replace
    // it with another malloc-compatible allocation (the GNU contract).
    unsafe { hook(&mut pointer) };
    let result = if pointer.is_null() {
        bytes.to_vec()
    } else {
        c_bytes(pointer).to_vec()
    };
    if pointer != allocation {
        free_c(allocation);
    }
    free_c(pointer);
    result
}

fn apply_dequote(bytes: &[u8], quote: c_int) -> Vec<u8> {
    let Some(function) = filename_dequoting_function() else {
        return bytes.to_vec();
    };
    let allocation = bytes_to_c(bytes);
    if allocation.is_null() {
        return bytes.to_vec();
    }
    // SAFETY: dequoting function receives a mutable C string valid for the
    // call and returns a malloc-compatible string (or the same pointer).
    let result_ptr = unsafe { function(allocation, quote) };
    let result = if result_ptr.is_null() {
        bytes.to_vec()
    } else {
        c_bytes(result_ptr).to_vec()
    };
    if result_ptr != allocation {
        free_c(result_ptr);
    }
    free_c(allocation);
    result
}

fn apply_stat_hook(bytes: &[u8]) -> Vec<u8> {
    apply_cpp_hook(filename_stat_hook(), bytes)
}

// --- Word scanning --------------------------------------------------------

/// Find the start of the word at point.
///
/// Mirrors GNU `_rl_find_completion_word`: an unclosed quote character wins,
/// otherwise word-break characters delimit the word, with
/// `rl_char_is_quoted_p` able to veto a break.
fn find_completion_word(found_quote: &mut c_int, delimiter: &mut c_int) -> c_char {
    let end = state::point().max(0);
    let buffer = line_bytes();
    let mut quote: c_int = 0;
    let mut delim: c_int = 0;
    let mut quote_char: u8 = 0;
    let mut point = end;

    let hook_chars = word_break_hook().and_then(|hook| {
        // SAFETY: the hook returns a NUL-terminated string owned by the app.
        let value = unsafe { hook() };
        if value.is_null() {
            None
        } else {
            Some(c_bytes(value).to_vec())
        }
    });
    let break_chars = hook_chars.unwrap_or_else(|| {
        let value = completer_word_break_characters();
        if value.is_null() {
            c_bytes(basic_word_break_characters()).to_vec()
        } else {
            c_bytes(value).to_vec()
        }
    });
    let quote_chars = completer_quote_characters();
    let quote_set = if quote_chars.is_null() {
        Vec::new()
    } else {
        c_bytes(quote_chars).to_vec()
    };

    if !quote_set.is_empty() {
        let mut pass_next = false;
        let mut scan = 0_usize;
        while scan < end as usize {
            let byte = buffer.get(scan).copied().unwrap_or(0);
            if pass_next {
                pass_next = false;
                scan += 1;
                continue;
            }
            if quote_char != b'\'' && byte == b'\\' {
                pass_next = true;
                quote |= RL_QF_BACKSLASH;
                scan += 1;
                continue;
            }
            if quote_char != 0 {
                if byte == quote_char {
                    quote_char = 0;
                    point = end;
                }
            } else if quote_set.contains(&byte) {
                quote_char = byte;
                point = scan as c_int + 1;
                quote |= match quote_char {
                    b'\'' => RL_QF_SINGLE_QUOTE,
                    b'"' => RL_QF_DOUBLE_QUOTE,
                    _ => RL_QF_OTHER_QUOTE,
                };
            }
            scan += 1;
        }
    }

    if point == end && quote_char == 0 {
        while point > 0 {
            point -= 1;
            let byte = buffer.get(point as usize).copied().unwrap_or(0);
            if !break_chars.contains(&byte) {
                continue;
            }
            if let Some(function) = char_is_quoted_p() {
                if quote != 0 {
                    // SAFETY: the line buffer is valid for the call and point
                    // is a valid index.
                    let quoted = unsafe { function(state::line_buffer(), point) };
                    if quoted != 0 {
                        continue;
                    }
                }
            }
            break;
        }
    }

    let scan = buffer.get(point as usize).copied().unwrap_or(0);
    if scan != 0 {
        let quoted = match char_is_quoted_p() {
            Some(function) if quote != 0 => {
                // SAFETY: as above.
                unsafe { function(state::line_buffer(), point) != 0 }
            }
            _ => false,
        };
        let is_break = !quoted && break_chars.contains(&scan);
        if is_break {
            if quote != 0 && contains(basic_quote_characters(), scan) && (end - point) > 1 {
                delim = scan as c_int;
            }
            if !contains(special_prefixes(), scan) {
                point += 1;
            }
        }
    }

    state::set_point(point.clamp(0, end));
    *found_quote = quote;
    *delimiter = delim;
    quote_char as c_char
}

// --- Quoting and insertion ------------------------------------------------

fn quoting_desired() -> bool {
    full_quoting_desired() != 0
        || (filename_completion_desired() != 0 && filename_quoting_desired() != 0)
}

/// Compute the replacement for MATCH, applying quoting rules and the
/// application quoting hook.  Returns owned bytes plus an updated quote char.
fn make_quoted_replacement(match_bytes: &[u8], match_type: c_int, quote: &mut c_char) -> Vec<u8> {
    let mut should_quote =
        !match_bytes.is_empty() && !completer_quote_characters().is_null() && quoting_desired();

    if should_quote {
        should_quote = if *quote == 0 {
            true
        } else {
            contains(completer_quote_characters(), c_byte(*quote))
        };
    }

    if should_quote {
        let quote_chars = filename_quote_characters();
        should_quote = if quote_chars.is_null() {
            false
        } else {
            match_bytes.iter().any(|byte| contains(quote_chars, *byte))
        };
        if !should_quote
            && *quote == 0
            && completion_found_quote() != 0
            && match_type != NO_MATCH
            && filename_quoting_function().is_some()
        {
            should_quote = true;
        }
    }

    if !should_quote {
        return match_bytes.to_vec();
    }

    let Some(function) = filename_quoting_function() else {
        // GNU leaves the match unquoted when the application clears the hook.
        return match_bytes.to_vec();
    };
    let input = bytes_to_c(match_bytes);
    if input.is_null() {
        return match_bytes.to_vec();
    }
    // SAFETY: the quoting hook receives a mutable match string, the match
    // type, and the address of the quoting character.
    let result = unsafe { function(input, match_type, quote) };
    let bytes = if result.is_null() {
        match_bytes.to_vec()
    } else {
        c_bytes(result).to_vec()
    };
    if result != input {
        free_c(result);
    }
    free_c(input);
    bytes
}

fn insert_match(match_bytes: &[u8], start: c_int, match_type: c_int, quote: &mut c_char) {
    let original_quote = *quote;
    let replacement = make_quoted_replacement(match_bytes, match_type, quote);
    if replacement.is_empty() {
        return;
    }

    let mut start = start;
    let mut end = state::point().max(0) - 1;
    let current = line_bytes();

    let remove_opening_quote = (*quote != 0
        && start > 0
        && current.get((start - 1) as usize).copied() == Some(c_byte(*quote))
        && replacement.first().copied() == Some(c_byte(*quote)))
        || (*quote != original_quote
            && start > 0
            && current.get((start - 1) as usize).copied() == Some(c_byte(original_quote))
            && replacement.first().copied() != Some(c_byte(original_quote)));
    if remove_opening_quote {
        start -= 1;
    }

    if *quote != 0
        && end > 0
        && current.get(state::point().max(0) as usize).copied() == Some(c_byte(*quote))
        && replacement.last().copied() == Some(c_byte(*quote))
    {
        end += 1;
    }

    if end < start {
        end = start;
    }
    replace_text(start, end, &replacement);
}

fn append_to_match(text: &[u8], delimiter: c_int, quote_char: c_char, nontrivial: bool) -> c_int {
    let mut suffix = Vec::new();
    if quote_char != 0
        && state::point() > 0
        && completion_suppress_quote() == 0
        && line_byte_at(state::point() - 1) != c_byte(quote_char)
    {
        suffix.push(c_byte(quote_char));
    }

    if delimiter != 0 {
        suffix.push(delimiter as u8);
    } else if completion_suppress_append() == 0 {
        let append = completion_append_character();
        if append != 0 {
            suffix.push(append as u8);
        }
    }

    if filename_completion_desired() != 0 {
        let filename = apply_stat_hook(&tilde_expand(text));
        let path = bytes_path(&filename);
        let metadata = if nontrivial && completion_mark_symlink_dirs() == 0 {
            std::fs::symlink_metadata(&path)
        } else {
            std::fs::metadata(&path)
        };

        match metadata {
            Ok(info) if info.is_dir() => {
                let point = state::point().max(0);
                if point > 0 && line_byte_at(point) == 0 && line_byte_at(point - 1) == b'/' {
                    // Already marked.
                } else if line_byte_at(point) != b'/' {
                    insert_text_bytes(b"/");
                }
            }
            Ok(info) if info.file_type().is_symlink() => {
                if !std::fs::metadata(&path)
                    .map(|target| target.is_dir())
                    .unwrap_or(false)
                    && state::point() == state::end()
                {
                    insert_text_bytes(&suffix);
                }
            }
            _ => {
                if state::point() == state::end() {
                    insert_text_bytes(&suffix);
                }
            }
        }
    } else if state::point() == state::end() {
        insert_text_bytes(&suffix);
    }

    suffix.len() as c_int
}

fn insert_all_matches(matches: &[Vec<u8>], point: c_int, quote: &mut c_char) {
    let mut start = point;
    if *quote != 0 && start > 0 && line_byte_at(start - 1) == c_byte(*quote) {
        start -= 1;
    }
    // SAFETY: line primitives only touch exported state.
    unsafe {
        crate::line::rl_delete_text(start, state::point());
    }
    state::set_point(start);

    if matches.len() > 1 {
        for entry in &matches[1..] {
            let replacement = make_quoted_replacement(entry, SINGLE_MATCH, quote);
            insert_text_bytes(&replacement);
            insert_text_bytes(b" ");
        }
    } else if let Some(entry) = matches.first() {
        let replacement = make_quoted_replacement(entry, SINGLE_MATCH, quote);
        insert_text_bytes(&replacement);
        insert_text_bytes(b" ");
    }
}

// --- Match generation -----------------------------------------------------

fn compute_lcd(matches: &[Vec<u8>]) -> Vec<u8> {
    if matches.is_empty() {
        return Vec::new();
    }
    if matches.len() == 1 {
        return matches[0].clone();
    }
    let first = &matches[0];
    let mut length = first.len();
    for other in &matches[1..] {
        let mut shared = 0;
        while shared < length && shared < other.len() && first[shared] == other[shared] {
            shared += 1;
        }
        length = shared;
        if length == 0 {
            break;
        }
    }
    first[..length].to_vec()
}

fn build_match_array(entries: Vec<Vec<u8>>) -> *mut *mut c_char {
    if entries.is_empty() {
        return ptr::null_mut();
    }

    // One match: the array is [match, NULL] and slot 0 is that match.
    // Multiple matches: slot 0 is a fresh lowest-common-denominator string.
    let single = entries.len() == 1;
    let total = if single { 2 } else { entries.len() + 2 };
    let array =
        unsafe { libc::malloc(total * std::mem::size_of::<*mut c_char>()) } as *mut *mut c_char;
    if array.is_null() {
        return ptr::null_mut();
    }

    let lcd = if single {
        entries[0].clone()
    } else {
        compute_lcd(&entries)
    };
    let lcd_ptr = bytes_to_c(&lcd);
    if lcd_ptr.is_null() {
        unsafe { libc::free(array as *mut libc::c_void) };
        return ptr::null_mut();
    }

    unsafe {
        *array = lcd_ptr;
        if single {
            *array.add(1) = ptr::null_mut();
            return array;
        }

        let mut index = 1_isize;
        for entry in &entries {
            let pointer = bytes_to_c(entry);
            if pointer.is_null() {
                // Error path: free everything placed so far, including the LCD.
                for slot in 0..index {
                    free_c(*array.offset(slot));
                }
                libc::free(array as *mut libc::c_void);
                return ptr::null_mut();
            }
            *array.offset(index) = pointer;
            index += 1;
        }
        *array.offset(index) = ptr::null_mut();
    }

    array
}

fn generator_matches(text: *const c_char, entry_function: RlCompEntryFunc) -> *mut *mut c_char {
    let Some(entry_function) = entry_function else {
        return ptr::null_mut();
    };

    // GNU keeps generator ownership in the returned array.  We collect the
    // byte content and rebuild fresh allocations so the error path in
    // build_match_array can free everything it has produced.
    let mut entries: Vec<Vec<u8>> = Vec::new();
    let mut generator_state: c_int = 0;
    loop {
        // SAFETY: the generator follows the state protocol and returns a
        // malloc-compatible string or NULL.
        let item = unsafe { entry_function(text, generator_state) };
        if item.is_null() {
            break;
        }
        entries.push(c_bytes(item).to_vec());
        free_c(item);
        generator_state += 1;
    }

    build_match_array(entries)
}

fn gen_completion_matches(
    text: &[u8],
    start: c_int,
    end: c_int,
    entry_function: unsafe extern "C" fn(*const c_char, c_int) -> *mut c_char,
    found_quote: c_int,
    quote_char: c_char,
) -> *mut *mut c_char {
    set_completion_found_quote(found_quote);
    set_completion_quote_character(quote_char as c_int);

    let text_c = bytes_to_c(text);
    if text_c.is_null() {
        return ptr::null_mut();
    }

    if let Some(attempted) = attempted_completion_function() {
        // SAFETY: ABI callback; text is NUL-terminated for the call.
        let matches = unsafe { attempted(text_c, start, end) };
        if !matches.is_null() || attempted_completion_over() != 0 {
            free_c(text_c);
            set_attempted_completion_over(0);
            return matches;
        }
    }

    let matches = generator_matches(text_c, Some(entry_function));
    free_c(text_c);
    matches
}

// --- Match post-processing ------------------------------------------------

fn compare_bytes(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    left.cmp(right)
}

/// Remove duplicates in place (GNU `remove_duplicate_matches`).
///
/// Slot 0 (the LCD) is preserved.  The array is sorted, adjacent duplicates
/// are freed, and the result is a fresh malloc-compatible array.
fn remove_duplicate_matches(matches: *mut *mut c_char) -> *mut *mut c_char {
    if matches.is_null() {
        return matches;
    }
    let len = match_list_len(matches) as usize;
    if len <= 1 {
        return matches;
    }

    // `entries` represents the array's slots 1..LEN; slot 0 is the LCD.
    let mut entries: Vec<Vec<u8>> = Vec::with_capacity(len);
    for index in 1..=len as isize {
        // SAFETY: NULL-terminated array with LEN entries after slot 0.
        let item = unsafe { *matches.offset(index) };
        if item.is_null() {
            break;
        }
        entries.push(c_bytes(item).to_vec());
    }
    if sort_completion_matches() != 0 {
        entries.sort_by(|left, right| compare_bytes(left, right));
    }
    entries.dedup();

    let total = entries.len() + 2;
    let array =
        unsafe { libc::malloc(total * std::mem::size_of::<*mut c_char>()) } as *mut *mut c_char;
    if array.is_null() {
        return matches;
    }

    // Keep the original LCD pointer (it is not one of ENTRIES).
    let lcd = unsafe { *matches };
    if entries.len() == 1 && c_bytes(lcd) == entries[0].as_slice() {
        // The single remaining match is the LCD; GNU drops the duplicate.
        // Free the old match strings and array, but keep the LCD itself.
        for index in 1..=len as isize {
            free_c(unsafe { *matches.offset(index) });
        }
        unsafe { libc::free(matches as *mut libc::c_void) };
        unsafe {
            *array = lcd;
            *array.add(1) = ptr::null_mut();
        }
        return array;
    }

    let lcd_copy = bytes_to_c(c_bytes(lcd));
    if lcd_copy.is_null() {
        unsafe { libc::free(array as *mut libc::c_void) };
        return matches;
    }
    unsafe {
        *array = lcd_copy;
        for (index, entry) in entries.iter().enumerate() {
            let pointer = bytes_to_c(entry);
            if pointer.is_null() {
                for slot in 0..=index {
                    free_c(*array.add(slot));
                }
                libc::free(array as *mut libc::c_void);
                return matches;
            }
            *array.add(index + 1) = pointer;
        }
        *array.add(entries.len() + 1) = ptr::null_mut();
    }

    // The old array and its generator strings are no longer referenced.
    free_match_list(matches);
    array
}

fn postprocess_matches(matches: *mut *mut c_char, _matching_filenames: c_int) -> *mut *mut c_char {
    if matches.is_null() {
        return matches;
    }
    if ignore_completion_duplicates() != 0 {
        remove_duplicate_matches(matches)
    } else {
        matches
    }
}

// --- Display --------------------------------------------------------------

fn display_width(bytes: &[u8]) -> c_int {
    let text = String::from_utf8_lossy(bytes);
    let mut width = 0_i32;
    for character in text.chars() {
        let code = character as u32;
        if code < 0x20 || code == 0x7f {
            width += 2;
        } else {
            width += 1;
        }
    }
    width
}

fn printable_part(pathname: &[u8]) -> &[u8] {
    if filename_completion_desired() == 0 {
        return pathname;
    }
    let Some(last) = pathname.iter().rposition(|byte| *byte == b'/') else {
        return pathname;
    };
    if last + 1 >= pathname.len() {
        // Trailing slash: use the component before it when there is one.
        if last == 0 {
            return pathname;
        }
        match pathname[..last].iter().rposition(|byte| *byte == b'/') {
            Some(previous) => &pathname[previous + 1..],
            None => pathname,
        }
    } else if last == 0 && pathname.len() == 1 {
        pathname
    } else {
        &pathname[last + 1..]
    }
}

fn print_filename(to_print: &[u8], full_pathname: &[u8], prefix_bytes: c_int) -> c_int {
    let mut printed = if prefix_bytes > 0 && prefix_bytes as usize <= to_print.len() {
        display_width(&to_print[prefix_bytes as usize..]) + prefix_bytes
    } else {
        display_width(to_print)
    };
    write_stdout(to_print);

    if filename_completion_desired() != 0 {
        let expanded = apply_stat_hook(&tilde_expand(full_pathname));
        if bytes_path(&expanded).is_dir() {
            write_stdout(b"/");
            printed += 1;
        }
    }
    printed
}

fn write_stdout(bytes: &[u8]) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(bytes);
    let _ = lock.flush();
}

fn screen_width() -> c_int {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.trim().parse::<c_int>().ok())
        .filter(|columns| *columns > 0)
        .unwrap_or(DEFAULT_COMPLETION_COLUMNS)
}

fn sort_match_entries(matches: *mut *mut c_char, len: c_int) {
    // GNU only sorts here when duplicates were not already sorted out by
    // remove_duplicate_matches().
    if ignore_completion_duplicates() != 0
        || sort_completion_matches() == 0
        || matches.is_null()
        || len <= 1
    {
        return;
    }
    let mut entries: Vec<*mut c_char> = Vec::new();
    for index in 1..=len as isize {
        // SAFETY: array is NULL-terminated with at least LEN entries.
        let item = unsafe { *matches.offset(index) };
        if item.is_null() {
            break;
        }
        entries.push(item);
    }
    entries.sort_by(|left, right| c_bytes(*left).cmp(c_bytes(*right)));
    for (offset, item) in entries.into_iter().enumerate() {
        // SAFETY: rewriting the same slots.
        unsafe { *matches.offset(offset as isize + 1) = item };
    }
}

/// Print MATCHES in columns, the way GNU `rl_display_match_list` does.
#[no_mangle]
pub unsafe extern "C" fn rl_display_match_list(matches: *mut *mut c_char, len: c_int, max: c_int) {
    if matches.is_null() {
        return;
    }
    let len = len.max(0);
    sort_match_entries(matches, len);

    let columns = screen_width();
    let cell = max.max(0) + 2;
    let mut limit = if cell > 0 { columns / cell } else { 1 };
    if limit != 1 && limit * cell == columns {
        limit -= 1;
    }
    if limit <= 0 {
        limit = 1;
    }
    let rows = (len + limit - 1) / limit;

    write_stdout(b"\n");
    let mut row = 1;
    while row <= rows {
        let mut column = 0;
        let mut index = row;
        while column < limit {
            if index > len {
                break;
            }
            // SAFETY: index is within the NULL-terminated array.
            let item = *matches.offset(index as isize);
            if item.is_null() {
                break;
            }
            let full = c_bytes(item);
            let printable = printable_part(full);
            let printed = print_filename(printable, full, 0);
            if column + 1 < limit {
                if cell <= printed {
                    write_stdout(b" ");
                } else {
                    for _ in 0..(cell - printed) {
                        write_stdout(b" ");
                    }
                }
            }
            index += rows;
            column += 1;
        }
        write_stdout(b"\n");
        row += 1;
    }
}

fn get_y_or_n() -> bool {
    let mut byte = 0_u8;
    // SAFETY: reading one byte from the process's standard input.
    let read = unsafe { libc::read(0, &mut byte as *mut u8 as *mut libc::c_void, 1) };
    if read <= 0 {
        return false;
    }
    matches!(byte, b'y' | b'Y' | b' ')
}

fn display_matches(matches: *mut *mut c_char) {
    if matches.is_null() {
        return;
    }
    // SAFETY: NULL-terminated array.
    let single = unsafe { *matches.add(1) }.is_null();
    if single {
        let full = c_bytes(unsafe { *matches });
        let printable = printable_part(full);
        write_stdout(b"\n");
        print_filename(printable, full, 0);
        write_stdout(b"\n");
        // SAFETY: line rendering only touches exported state; uses the hook.
        redisplay_with_hook();
        return;
    }

    let mut max = 0;
    let mut len = 0;
    let mut index = 1_isize;
    loop {
        // SAFETY: NULL-terminated array.
        let item = unsafe { *matches.offset(index) };
        if item.is_null() {
            break;
        }
        let full = c_bytes(item);
        let width = display_width(printable_part(full));
        if width > max {
            max = width;
        }
        len += 1;
        index += 1;
    }

    if let Some(hook) = display_matches_hook() {
        // SAFETY: the hook receives the match array, its length, and the
        // longest printable width, matching GNU's callback contract.
        unsafe { hook(matches, len, max) };
        return;
    }

    let query_items = completion_query_items();
    if query_items > 0 && len >= query_items {
        write_stdout(b"\n");
        write_stdout(format!("Display all {len} possibilities? (y or n)").as_bytes());
        if !get_y_or_n() {
            write_stdout(b"\n");
            redisplay_with_hook();
            return;
        }
    }

    if !display_matches_interrupted() {
        // SAFETY: the public function validates the array and length.
        unsafe { rl_display_match_list(matches, len, max) };
    }
    redisplay_with_hook();
}

// --- Completion entry points ---------------------------------------------

/// Compare two readline command pointers without relying on function-pointer
/// equality (which is not stable across codegen units).
fn same_command(left: state::RlCommandFunc, right: state::RlCommandFunc) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => std::ptr::fn_addr_eq(left, right),
        _ => false,
    }
}

fn set_completion_defaults(what_to_do: c_int) {
    set_filename_completion_desired(0);
    set_filename_quoting_desired(1);
    set_full_quoting_desired(0);
    set_completion_type(what_to_do);
    set_completion_suppress_append(0);
    set_completion_suppress_quote(0);
    set_completion_append_character(b' ' as c_int);
    set_completion_mark_symlink_dirs(0);
}

fn current_entry_function() -> RlCompEntryFunc {
    completion_entry_function()
}

fn dequoted_text(text: &[u8]) -> Vec<u8> {
    if quoting_desired() && completion_found_quote() != 0 {
        apply_dequote(text, completion_quote_character())
    } else {
        text.to_vec()
    }
}

/// Complete the word at point.
#[no_mangle]
pub unsafe extern "C" fn rl_complete_internal(what_to_do: c_int) -> c_int {
    let saved_state = state::readline_state();
    state::set_readline_state(saved_state | line::RL_STATE_COMPLETING);

    let saved_last_failure = LAST_COMPLETION_FAILED.load(Ordering::Relaxed);
    set_completion_defaults(what_to_do);
    let saved_line = line_bytes();

    let our_function = current_entry_function();
    let end = state::point().max(0);
    let mut found_quote = 0;
    let mut delimiter = 0;
    let mut quote_char = 0;

    if state::point() > 0 {
        quote_char = find_completion_word(&mut found_quote, &mut delimiter);
    }
    let start = state::point().max(0);
    state::set_point(end);

    let text = copy_text(start, end);
    let mut matches = gen_completion_matches(
        &text,
        start,
        end,
        our_function.unwrap_or(rl_filename_completion_function),
        found_quote,
        quote_char,
    );

    let comparison_text = dequoted_text(&text);
    let nontrivial_lcd = if matches.is_null() {
        false
    } else {
        // SAFETY: matches[0] exists for a non-NULL array.
        c_bytes(*matches) != comparison_text.as_slice()
    };
    let text_len = comparison_text.len() as c_int;

    if matches.is_null() {
        // SAFETY: exported utility; rings the terminal bell.
        crate::line::rl_ding();
        LAST_COMPLETION_FAILED.store(true, Ordering::Relaxed);
        COMPLETION_CHANGED_BUFFER.store(false, Ordering::Relaxed);
        state::set_readline_state(state::readline_state() & !line::RL_STATE_COMPLETING);
        reset_completion_state();
        return 0;
    }

    let matching_filenames = filename_completion_desired();
    matches = postprocess_matches(matches, matching_filenames);
    if matches.is_null() {
        crate::line::rl_ding();
        LAST_COMPLETION_FAILED.store(true, Ordering::Relaxed);
        COMPLETION_CHANGED_BUFFER.store(false, Ordering::Relaxed);
        state::set_readline_state(state::readline_state() & !line::RL_STATE_COMPLETING);
        reset_completion_state();
        return 0;
    }

    if !c_bytes(*matches).is_empty() {
        LAST_COMPLETION_FAILED.store(false, Ordering::Relaxed);
    }

    let entries = c_strings(matches);
    let match_count = entries.len();
    let mut do_display = false;

    match what_to_do {
        TAB | EXCLAMATION | AT => {
            if what_to_do == TAB {
                if !entries[0].is_empty() {
                    insert_match(
                        &entries[0],
                        start,
                        if match_count > 1 {
                            MULT_MATCH
                        } else {
                            SINGLE_MATCH
                        },
                        &mut quote_char,
                    );
                }
            } else if !entries[0].is_empty() && match_count == 1 {
                insert_match(&entries[0], start, SINGLE_MATCH, &mut quote_char);
            } else if !entries[0].is_empty() && entries[0].len() as c_int >= text_len {
                insert_match(
                    &entries[0],
                    start,
                    if match_count > 1 {
                        MULT_MATCH
                    } else {
                        SINGLE_MATCH
                    },
                    &mut quote_char,
                );
            }

            if match_count > 1 {
                if what_to_do == EXCLAMATION {
                    do_display = true;
                } else if what_to_do == AT {
                    do_display = !nontrivial_lcd;
                } else {
                    crate::line::rl_ding();
                }
            } else {
                append_to_match(&entries[0], delimiter, quote_char, nontrivial_lcd);
            }
        }
        STAR => insert_all_matches(&entries, start, &mut quote_char),
        QUESTION => {
            if saved_last_failure && match_count == 1 && !entries[0].is_empty() {
                insert_match(&entries[0], start, SINGLE_MATCH, &mut quote_char);
                append_to_match(&entries[0], delimiter, quote_char, nontrivial_lcd);
            } else {
                do_display = true;
            }
        }
        PERCENT | PIPE => do_display = true,
        _ => {
            crate::line::rl_ding();
            free_match_list(matches);
            state::set_readline_state(state::readline_state() & !line::RL_STATE_COMPLETING);
            reset_completion_state();
            return 1;
        }
    }

    if do_display {
        state::set_readline_state(state::readline_state() | line::RL_STATE_COMPLETING);
        display_matches(matches);
        DISPLAY_INTERRUPTED.store(false, Ordering::Relaxed);
    }

    free_match_list(matches);

    let changed = line_bytes() != saved_line;
    COMPLETION_CHANGED_BUFFER.store(changed, Ordering::Relaxed);

    state::set_readline_state(state::readline_state() & !line::RL_STATE_COMPLETING);
    reset_completion_state();
    0
}

fn reset_completion_state() {
    set_completion_found_quote(0);
    set_completion_quote_character(0);
}

/// Complete the word at or before point.
#[no_mangle]
pub unsafe extern "C" fn rl_complete(_ignore: c_int, invoking_key: c_int) -> c_int {
    set_completion_invoking_key(invoking_key);
    if inhibit_completion() != 0 {
        if (0..=255).contains(&invoking_key) {
            let byte = [invoking_key as u8, 0];
            crate::line::rl_insert_text(byte.as_ptr() as *const c_char);
        }
        return 0;
    }
    let last = ptr::addr_of!(state::rl_last_func).read();
    if same_command(last, Some(rl_complete)) && !COMPLETION_CHANGED_BUFFER.load(Ordering::Relaxed) {
        rl_complete_internal(b'?' as c_int)
    } else {
        rl_complete_internal(TAB)
    }
}

/// List possible completions.
#[no_mangle]
pub unsafe extern "C" fn rl_possible_completions(_ignore: c_int, invoking_key: c_int) -> c_int {
    LAST_COMPLETION_FAILED.store(false, Ordering::Relaxed);
    set_completion_invoking_key(invoking_key);
    rl_complete_internal(b'?' as c_int)
}

/// Insert all possible completions.
#[no_mangle]
pub unsafe extern "C" fn rl_insert_completions(_ignore: c_int, invoking_key: c_int) -> c_int {
    set_completion_invoking_key(invoking_key);
    rl_complete_internal(b'*' as c_int)
}

/// Return the value `rl_complete_internal` should receive for CFUNC.
#[no_mangle]
pub unsafe extern "C" fn rl_completion_mode(cfunc: state::RlCommandFunc) -> c_int {
    let last = ptr::addr_of!(state::rl_last_func).read();
    if same_command(last, cfunc) && !COMPLETION_CHANGED_BUFFER.load(Ordering::Relaxed) {
        b'?' as c_int
    } else {
        TAB
    }
}

// --- Menu completion ------------------------------------------------------

fn menu_active(last: state::RlCommandFunc) -> bool {
    same_command(last, Some(rl_menu_complete))
        || same_command(last, Some(rl_backward_menu_complete))
}

fn menu_lock() -> std::sync::MutexGuard<'static, Option<MenuState>> {
    MENU_COMPLETION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Generate the menu list and insert the lowest common denominator.
///
/// Returns `None` when no matches exist.  A single-match state has
/// `full_completion` set so the next call regenerates (GNU does the same).
fn run_menu_generation() -> Option<MenuState> {
    set_completion_defaults(b'%' as c_int);

    let our_function = menu_completion_entry_function()
        .or_else(current_entry_function)
        .unwrap_or(rl_filename_completion_function);

    let orig_end = state::point().max(0);
    let mut found_quote = 0;
    let mut delimiter = 0;
    let mut quote_char = 0;
    if state::point() > 0 {
        quote_char = find_completion_word(&mut found_quote, &mut delimiter);
    }
    let orig_start = state::point().max(0);
    state::set_point(orig_end);

    let orig_text = copy_text(orig_start, orig_end);
    let mut matches = gen_completion_matches(
        &orig_text,
        orig_start,
        orig_end,
        our_function,
        found_quote,
        quote_char,
    );
    if matches.is_null() {
        // SAFETY: ding only touches terminal output.
        unsafe { crate::line::rl_ding() };
        return None;
    }

    let comparison_text = dequoted_text(&orig_text);
    // SAFETY: matches is a non-NULL, NULL-terminated match array.
    let nontrivial_lcd =
        !matches.is_null() && c_bytes(unsafe { *matches }) != comparison_text.as_slice();
    let matching_filenames = filename_completion_desired();
    matches = postprocess_matches(matches, matching_filenames);
    if matches.is_null() {
        // SAFETY: ding only touches terminal output.
        unsafe { crate::line::rl_ding() };
        return None;
    }

    let entries = c_strings(matches);
    free_match_list(matches);
    let size = entries.len() as i32;

    // Insert the lowest common denominator first, as GNU does.
    let mut quote = quote_char;
    if let Some(first) = entries.first() {
        if !first.is_empty() {
            insert_match(
                first,
                orig_start,
                if size > 1 { MULT_MATCH } else { SINGLE_MATCH },
                &mut quote,
            );
        }
    }
    if let Some(first) = entries.first() {
        if !first.is_empty() {
            COMPLETION_CHANGED_BUFFER.store(
                comparison_text.as_slice() != first.as_slice(),
                Ordering::Relaxed,
            );
        }
    }

    if size <= 1 {
        if let Some(first) = entries.first() {
            append_to_match(first, delimiter, quote, nontrivial_lcd);
        }
        return Some(MenuState {
            orig_text,
            comparison_text,
            matches: entries,
            index: 0,
            size,
            full_completion: true,
            orig_start,
            quote_char: quote,
            delimiter,
        });
    }

    Some(MenuState {
        orig_text,
        comparison_text,
        matches: entries,
        index: 0,
        size,
        full_completion: false,
        orig_start,
        quote_char: quote,
        delimiter,
    })
}

fn menu_insert(state: &MenuState, zero: bool) {
    let mut quote = state.quote_char;
    if zero && state.size > 1 {
        // SAFETY: ding only touches terminal output.
        unsafe { crate::line::rl_ding() };
        if let Some(first) = state.matches.first() {
            insert_match(first, state.orig_start, MULT_MATCH, &mut quote);
        }
    } else if let Ok(index) = usize::try_from(state.index) {
        if let Some(entry) = state.matches.get(index) {
            insert_match(entry, state.orig_start, SINGLE_MATCH, &mut quote);
            let nontrivial = state.comparison_text.as_slice() != entry.as_slice();
            append_to_match(entry, state.delimiter, quote, nontrivial);
        }
    }
    COMPLETION_CHANGED_BUFFER.store(true, Ordering::Relaxed);
}

/// Menu-complete the word at point.
#[no_mangle]
pub unsafe extern "C" fn rl_menu_complete(count: c_int, _ignore: c_int) -> c_int {
    let last = ptr::addr_of!(state::rl_last_func).read();

    let restart = {
        let guard = menu_lock();
        match guard.as_ref() {
            Some(state) => !menu_active(last) || state.full_completion,
            None => true,
        }
    };

    if restart {
        {
            let mut guard = menu_lock();
            *guard = None;
        }
        let Some(state) = run_menu_generation() else {
            return 0;
        };
        let single = state.size <= 1;
        {
            let mut guard = menu_lock();
            *guard = Some(state);
        }
        if single {
            return 0;
        }
    }

    let snapshot = {
        let mut guard = menu_lock();
        let Some(state) = guard.as_mut() else {
            return 0;
        };
        if state.size > 0 {
            state.index += count;
            state.index %= state.size;
            if state.index < 0 {
                state.index += state.size;
            }
        }
        MenuState {
            orig_text: state.orig_text.clone(),
            comparison_text: state.comparison_text.clone(),
            matches: state.matches.clone(),
            index: state.index,
            size: state.size,
            full_completion: state.full_completion,
            orig_start: state.orig_start,
            quote_char: state.quote_char,
            delimiter: state.delimiter,
        }
    };

    let zero = snapshot.index == 0 && snapshot.size > 1;
    menu_insert(&snapshot, zero);
    0
}

/// Menu-complete in the reverse direction.
#[no_mangle]
pub unsafe extern "C" fn rl_backward_menu_complete(count: c_int, key: c_int) -> c_int {
    rl_menu_complete(-count, key)
}

// --- Application-callable generators -------------------------------------

/// Build a match array by calling ENTRY_FUNCTION with state 0, 1, ...
///
/// The returned array is `malloc`-compatible and NULL-terminated.  For one
/// match, `matches[0]` is that match and `matches[1]` is NULL.  For several
/// matches, `matches[0]` is a freshly allocated lowest common denominator.
#[no_mangle]
pub unsafe extern "C" fn rl_completion_matches(
    text: *const c_char,
    entry_function: RlCompEntryFunc,
) -> *mut *mut c_char {
    generator_matches(text, entry_function)
}

/// Filename completion generator.
///
/// State 0 splits TEXT into directory and basename components, expands
/// `~user`, applies the dequoting/directory-rewrite hooks, and snapshots the
/// directory.  Later states return one match per call until NULL.  Matches are
/// `malloc`-compatible strings joined with the directory the user typed.
#[no_mangle]
pub unsafe extern "C" fn rl_filename_completion_function(
    text: *const c_char,
    state_value: c_int,
) -> *mut c_char {
    let mut guard = FILENAME_COMPLETION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if state_value == 0 {
        *guard = None;
        let typed = c_bytes(text).to_vec();
        let mut prefix = typed.clone();
        let mut dirname = if typed.is_empty() {
            b".".to_vec()
        } else {
            typed.clone()
        };

        if let Some(index) = dirname.iter().rposition(|byte| *byte == b'/') {
            prefix = dirname[index + 1..].to_vec();
            dirname.truncate(index + 1);
        } else {
            dirname = b".".to_vec();
        }

        let found_quote = completion_found_quote() != 0;
        let quote = completion_quote_character();
        let users_dirname = if found_quote {
            apply_dequote(&dirname, quote)
        } else {
            dirname.clone()
        };

        let expanded = tilde_expand(&dirname);
        let expanded = if directory_rewrite_hook().is_some() {
            apply_cpp_hook(directory_rewrite_hook(), &expanded)
        } else if found_quote {
            // GNU reconciles the already-dequoted users_dirname with the
            // expanded dirname; tilde expansion has already happened.
            if dirname.starts_with(b"~") && !users_dirname.starts_with(b"~") {
                users_dirname.clone()
            } else {
                tilde_expand(&users_dirname)
            }
        } else {
            expanded
        };

        if found_quote && !prefix.is_empty() {
            prefix = apply_dequote(&prefix, quote);
        }

        let mut entries = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(bytes_path(&expanded)) {
            entries.push(b".".to_vec());
            entries.push(b"..".to_vec());
            for entry in read_dir.flatten() {
                let mut name = os_bytes(entry.file_name());
                if let Some(hook) = filename_rewrite_hook() {
                    let allocation = bytes_to_c(&name);
                    if !allocation.is_null() {
                        // SAFETY: hook receives the entry name and its length;
                        // it returns either that pointer or a new allocation.
                        let converted = hook(allocation, name.len() as c_int);
                        if !converted.is_null() && converted != allocation {
                            name = c_bytes(converted).to_vec();
                            free_c(converted);
                        }
                        free_c(allocation);
                    }
                }
                entries.push(name);
            }
        }

        let prefix_len = prefix.len();
        entries.retain(|name| {
            if prefix_len == 0 {
                name.as_slice() != b"." && name.as_slice() != b".."
            } else {
                name.len() >= prefix_len && name[..prefix_len] == prefix[..]
            }
        });

        set_filename_completion_desired(1);
        *guard = Some(FilenameCompletionState {
            dirname: expanded,
            users_dirname,
            entries,
            index: 0,
        });
    }

    let Some(state) = guard.as_mut() else {
        return ptr::null_mut();
    };
    if state.index >= state.entries.len() {
        *guard = None;
        return ptr::null_mut();
    }

    let name = state.entries[state.index].clone();
    state.index += 1;

    let base = if state.dirname.as_slice() == b"."
        || (state.dirname.len() == 1 && state.dirname[0] == b'.')
    {
        None
    } else if complete_with_tilde_expansion() != 0 && state.users_dirname.starts_with(b"~") {
        Some(state.dirname.as_slice())
    } else {
        Some(state.users_dirname.as_slice())
    };

    let output = match base {
        Some(directory) => {
            let mut path = directory.to_vec();
            if !path.ends_with(b"/") {
                path.push(b'/');
            }
            path.extend_from_slice(&name);
            path
        }
        None => name,
    };

    bytes_to_c(&output)
}

/// Username completion generator.
///
/// State 0 snapshots the matching records from `/etc/passwd`; later states
/// return one name per call.  TEXT may be `user`, `~user`, or empty.  On
/// platforms without `/etc/passwd` this is a documented no-match fallback.
#[no_mangle]
pub unsafe extern "C" fn rl_username_completion_function(
    text: *const c_char,
    state_value: c_int,
) -> *mut c_char {
    let mut guard = USERNAME_COMPLETION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if state_value == 0 {
        let typed = c_bytes(text);
        let first_char = typed.first().copied().unwrap_or(0);
        let first_char_loc = usize::from(first_char == b'~');
        let prefix = typed.get(first_char_loc..).unwrap_or(&[]).to_vec();
        let names = passwd_entries()
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| {
                prefix.is_empty()
                    || (name.len() >= prefix.len() && name[..prefix.len()] == prefix[..])
            })
            .collect();
        *guard = Some(UsernameCompletionState {
            first_char,
            names,
            index: 0,
        });
    }

    let Some(state) = guard.as_mut() else {
        return ptr::null_mut();
    };
    if state.index >= state.names.len() {
        *guard = None;
        return ptr::null_mut();
    }
    let name = state.names[state.index].clone();
    state.index += 1;

    let mut value = Vec::with_capacity(name.len() + 1);
    if state.first_char == b'~' {
        value.push(b'~');
        set_filename_completion_desired(1);
    }
    value.extend_from_slice(&name);
    bytes_to_c(&value)
}

/// Refresh the cached screen dimensions and repaint.
#[no_mangle]
pub unsafe extern "C" fn rl_resize_terminal() {
    // The width is read from COLUMNS on every display, so there is no cached
    // size to invalidate.  Repaint through the application's hook, matching
    // GNU's SIGWINCH path.
    let readline_state = state::readline_state();
    if readline_state & line::RL_STATE_INITIALIZED != 0 {
        redisplay_with_hook();
    }
}

// --- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset_globals() {
        set_completion_entry_function(None);
        set_menu_completion_entry_function(None);
        set_attempted_completion_function(None);
        set_display_matches_hook(None);
        set_word_break_hook(None);
        set_filename_rewrite_hook(None);
        set_filename_stat_hook(None);
        set_directory_rewrite_hook(None);
        set_filename_dequoting_function(None);
        set_filename_quoting_function(Some(default_quote_filename));
        set_char_is_quoted_p(None);
        // SAFETY: tests are serialized by TEST_LOCK.
        set_completer_word_break_characters(unsafe { rl_basic_word_break_characters });
        set_completer_quote_characters(ptr::null_mut());
        set_filename_quote_characters(ptr::null_mut());
        set_special_prefixes(ptr::null_mut());
        set_attempted_completion_over(0);
        set_filename_completion_desired(0);
        set_filename_quoting_desired(1);
        set_full_quoting_desired(0);
        set_completion_found_quote(0);
        set_completion_quote_character(0);
        set_completion_suppress_append(0);
        set_completion_query_items(100);
        set_ignore_completion_duplicates(1);
        set_sort_completion_matches(1);
        set_inhibit_completion(0);
        set_redisplay_function(None);
        set_startup_hook(None);
        set_pre_input_hook(None);
        *FILENAME_COMPLETION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        *USERNAME_COMPLETION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        *MENU_COMPLETION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        LAST_COMPLETION_FAILED.store(false, Ordering::Relaxed);
        COMPLETION_CHANGED_BUFFER.store(false, Ordering::Relaxed);
        // SAFETY: unit tests are serialized by TEST_LOCK.
        unsafe {
            crate::line::rl_initialize();
        }
    }

    unsafe extern "C" fn counting_generator(
        _text: *const c_char,
        state_value: c_int,
    ) -> *mut c_char {
        const NAMES: [&[u8]; 2] = [b"alpha", b"alpine"];
        NAMES
            .get(state_value as usize)
            .map(|name| bytes_to_c(name))
            .unwrap_or(ptr::null_mut())
    }

    unsafe extern "C" fn single_generator(_text: *const c_char, state_value: c_int) -> *mut c_char {
        if state_value == 0 {
            bytes_to_c(b"solo")
        } else {
            ptr::null_mut()
        }
    }

    #[test]
    fn completion_matches_follows_the_generator_protocol() {
        let _guard = test_guard();
        reset_globals();
        // SAFETY: test serialized; NUL-terminated static text.
        let matches = unsafe { rl_completion_matches(c"al".as_ptr(), Some(counting_generator)) };
        assert!(!matches.is_null());
        // SAFETY: NULL-terminated array.
        unsafe {
            assert_eq!(c_bytes(*matches), b"alp");
            assert_eq!(c_bytes(*matches.add(1)), b"alpha");
            assert_eq!(c_bytes(*matches.add(2)), b"alpine");
            assert!((*matches.add(3)).is_null());
            free_match_list(matches);
        }
    }

    #[test]
    fn completion_matches_returns_null_without_matches_and_for_null_generator() {
        let _guard = test_guard();
        reset_globals();
        // SAFETY: static text and NULL generator.
        let matches = unsafe { rl_completion_matches(c"x".as_ptr(), None) };
        assert!(matches.is_null());
    }

    #[test]
    fn completion_matches_single_result_uses_slot_zero() {
        let _guard = test_guard();
        reset_globals();
        // SAFETY: see above.
        let matches = unsafe { rl_completion_matches(c"s".as_ptr(), Some(single_generator)) };
        assert!(!matches.is_null());
        unsafe {
            assert_eq!(c_bytes(*matches), b"solo");
            assert!((*matches.add(1)).is_null());
            free_match_list(matches);
        }
    }

    #[test]
    fn find_completion_word_uses_break_characters() {
        let _guard = test_guard();
        reset_globals();
        // SAFETY: line primitives touch only exported state.
        unsafe {
            crate::line::rl_replace_line(c"echo foobar".as_ptr(), 0);
            state::set_point(11);
        }
        let mut found_quote = 0;
        let mut delimiter = 0;
        let quote = find_completion_word(&mut found_quote, &mut delimiter);
        assert_eq!(quote, 0);
        assert_eq!(state::point(), 5);
        assert_eq!(found_quote, 0);
        assert_eq!(delimiter, 0);
    }

    #[test]
    fn find_completion_word_detects_an_unclosed_quote() {
        let _guard = test_guard();
        reset_globals();
        set_completer_quote_characters(c"\"'".as_ptr() as *mut c_char);
        unsafe {
            crate::line::rl_replace_line(c"echo 'he".as_ptr(), 0);
            state::set_point(8);
        }
        let mut found_quote = 0;
        let mut delimiter = 0;
        let quote = find_completion_word(&mut found_quote, &mut delimiter);
        assert_eq!(quote, b'\'' as c_char);
        assert_eq!(state::point(), 6);
        assert_ne!(found_quote & RL_QF_SINGLE_QUOTE, 0);
    }

    #[test]
    fn filename_completion_lists_directory_entries() {
        let _guard = test_guard();
        reset_globals();
        let root = std::env::temp_dir().join(format!(
            "ruline-filename-completion-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("space name"), b"x").unwrap();
        let text = format!("{}/", root.display());
        let text = bytes_to_c(text.as_bytes());

        let mut names = Vec::new();
        let mut state_value = 0;
        loop {
            // SAFETY: text is NUL-terminated for the loop; state is the ABI
            // generator protocol.
            let item = unsafe { rl_filename_completion_function(text, state_value) };
            if item.is_null() {
                break;
            }
            // SAFETY: item is a NUL-terminated allocation.
            names.push(
                String::from_utf8_lossy(unsafe { CStr::from_ptr(item) }.to_bytes()).into_owned(),
            );
            free_c(item);
            state_value += 1;
        }
        free_c(text);
        std::fs::remove_dir_all(&root).unwrap();

        assert!(names.iter().any(|name| name.ends_with("/sub")));
        assert!(names.iter().any(|name| name.ends_with("/space name")));
    }

    #[test]
    fn append_to_match_marks_directories_with_a_slash() {
        let _guard = test_guard();
        reset_globals();
        let root = std::env::temp_dir().join(format!(
            "ruline-append-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let directory = format!("{}/sub", root.display());
        unsafe {
            crate::line::rl_replace_line(bytes_to_c(directory.as_bytes()), 0);
            set_filename_completion_desired(1);
            set_completion_append_character(b' ' as c_int);
        }
        state::set_point(state::end());
        let appended = append_to_match(directory.as_bytes(), 0, 0, true);
        assert_eq!(appended, 1);
        assert_eq!(line_byte_at(state::point() - 1), b'/');
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn username_completion_handles_the_tilde_prefix() {
        let _guard = test_guard();
        reset_globals();
        let entries = passwd_entries();
        if entries.is_empty() {
            // Documented platform fallback: no /etc/passwd, no matches.
            // SAFETY: static text.
            let value =
                unsafe { rl_username_completion_function(c"~__ruline_missing_user".as_ptr(), 0) };
            assert!(value.is_null());
            return;
        }
        let name = entries[0].0.clone();
        let text = format!("~{}", String::from_utf8_lossy(&name));
        let value = unsafe { rl_username_completion_function(bytes_to_c(text.as_bytes()), 0) };
        assert!(!value.is_null());
        assert_eq!(c_bytes(value), text.as_bytes());
        free_c(value);
    }

    unsafe extern "C" fn comma_word_break_hook() -> *mut c_char {
        c",".as_ptr() as *mut c_char
    }

    #[test]
    fn completion_word_break_hook_overrides_the_default_set() {
        let _guard = test_guard();
        reset_globals();
        // SAFETY: line primitives touch only exported state.
        unsafe {
            crate::line::rl_replace_line(c"alpha,beta".as_ptr(), 0);
            state::set_point(10);
        }
        set_word_break_hook(Some(comma_word_break_hook));
        let mut found_quote = 0;
        let mut delimiter = 0;
        let _ = find_completion_word(&mut found_quote, &mut delimiter);
        assert_eq!(state::point(), 6);
    }

    unsafe extern "C" fn strip_leading_quote(text: *mut c_char, _quote: c_int) -> *mut c_char {
        let bytes = c_bytes(text);
        let stripped = bytes.strip_prefix(b"'").unwrap_or(bytes);
        bytes_to_c(stripped)
    }

    #[test]
    fn filename_completion_applies_the_dequoting_hook() {
        let _guard = test_guard();
        reset_globals();
        let root = std::env::temp_dir().join(format!(
            "ruline-dequote-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("alpha"), b"x").unwrap();

        set_filename_dequoting_function(Some(strip_leading_quote));
        set_completion_found_quote(1);
        set_completion_quote_character(b'\'' as c_int);
        set_completer_quote_characters(c"'".as_ptr() as *mut c_char);

        let text = format!("'{}/al", root.display());
        let text = bytes_to_c(text.as_bytes());
        // SAFETY: generator protocol with a NUL-terminated text.
        let item = unsafe { rl_filename_completion_function(text, 0) };
        assert!(!item.is_null());
        let matched = c_bytes(item).to_vec();
        free_c(item);
        free_c(text);
        std::fs::remove_dir_all(&root).unwrap();

        let expected = format!("{}/alpha", root.display());
        assert_eq!(matched, expected.as_bytes());
    }

    #[test]
    fn quoting_replacement_uses_quote_and_filename_quote_characters() {
        let _guard = test_guard();
        reset_globals();
        set_completer_quote_characters(c"\"'".as_ptr() as *mut c_char);
        set_filename_quote_characters(c" ".as_ptr() as *mut c_char);
        set_filename_completion_desired(1);
        set_filename_quoting_desired(1);
        let mut quote = 0;
        let replacement = make_quoted_replacement(b"space name", SINGLE_MATCH, &mut quote);
        assert_eq!(replacement, b"\"space name");
        assert_eq!(quote, b'"' as c_char);
    }

    #[test]
    fn completion_mode_uses_the_last_command_and_buffer_change() {
        let _guard = test_guard();
        reset_globals();
        // SAFETY: tests are serialized by TEST_LOCK.
        unsafe {
            ptr::addr_of_mut!(state::rl_last_func).write(Some(rl_complete));
        }
        COMPLETION_CHANGED_BUFFER.store(false, Ordering::Relaxed);
        // SAFETY: comparison only reads exported state.
        assert_eq!(
            unsafe { rl_completion_mode(Some(rl_complete)) },
            b'?' as c_int
        );
        COMPLETION_CHANGED_BUFFER.store(true, Ordering::Relaxed);
        assert_eq!(unsafe { rl_completion_mode(Some(rl_complete)) }, TAB);
        // SAFETY: restore the exported dispatch state.
        unsafe {
            ptr::addr_of_mut!(state::rl_last_func).write(None);
        }
    }

    #[test]
    fn display_width_counts_control_characters_as_two_columns() {
        assert_eq!(display_width(b"abcd"), 4);
        assert_eq!(display_width(b"a\tb"), 4);
        assert_eq!(display_width("é".as_bytes()), 1);
    }

    #[test]
    fn printable_part_uses_the_basename_for_filenames() {
        let _guard = test_guard();
        reset_globals();
        set_filename_completion_desired(0);
        assert_eq!(printable_part(b"/usr/lib"), b"/usr/lib");
        set_filename_completion_desired(1);
        assert_eq!(printable_part(b"/usr/lib"), b"lib");
        assert_eq!(printable_part(b"/usr/lib/"), b"lib/");
        assert_eq!(printable_part(b"/"), b"/");
        assert_eq!(printable_part(b"plain"), b"plain");
    }

    #[test]
    fn redisplay_hook_is_invoked_when_installed() {
        let _guard = test_guard();
        reset_globals();
        static CALLED: AtomicBool = AtomicBool::new(false);
        unsafe extern "C" fn hook() {
            CALLED.store(true, Ordering::Relaxed);
        }
        set_redisplay_function(Some(hook));
        redisplay_with_hook();
        assert!(CALLED.load(Ordering::Relaxed));
    }

    #[test]
    fn completion_defaults_match_gnu() {
        let _guard = test_guard();
        reset_globals();
        set_completion_defaults(b'?' as c_int);
        assert_eq!(completion_type(), b'?' as c_int);
        assert_eq!(filename_completion_desired(), 0);
        assert_eq!(filename_quoting_desired(), 1);
        assert_eq!(completion_append_character(), b' ' as c_int);
        assert_eq!(completion_suppress_append(), 0);
        assert_eq!(completion_suppress_quote(), 0);
    }
}
