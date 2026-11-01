//! Exported readline state globals and their safe accessors.
//!
//! The C ABI requires real data symbols at fixed addresses, so the globals are
//! `static mut`.  Rust code never creates references to them; access goes
//! through `addr_of!`/`addr_of_mut!` and these small helper functions.  That
//! keeps the exported layout intact while avoiding `static_mut_refs` aliasing.

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

/// Signature of a readline bindable command.
pub type RlCommandFunc = Option<unsafe extern "C" fn(c_int, c_int) -> c_int>;

const DEFAULT_BUFFER_SIZE: usize = 1024;
const DEFAULT_KEYBOARD_TIMEOUT: c_int = 100_000;

static LINE_CAPACITY: AtomicUsize = AtomicUsize::new(0);
static KEYBOARD_TIMEOUT: AtomicUsize = AtomicUsize::new(DEFAULT_KEYBOARD_TIMEOUT as usize);
// GNU keeps the sign of the numeric argument in an internal `rl_arg_sign`.
static ARGUMENT_SIGN: AtomicI32 = AtomicI32::new(1);

static EMPTY_STRING: [u8; 1] = [0];
static DEFAULT_READLINE_NAME: [u8; 6] = *b"other\0";
static LIBRARY_VERSION: [u8; 4] = *b"8.3\0";

// --- Exported state -------------------------------------------------------

#[no_mangle]
pub static mut rl_line_buffer: *mut c_char = ptr::null_mut();
#[no_mangle]
pub static mut rl_point: c_int = 0;
#[no_mangle]
pub static mut rl_end: c_int = 0;
#[no_mangle]
pub static mut rl_mark: c_int = 0;

#[no_mangle]
pub static mut rl_readline_state: std::os::raw::c_ulong = 0;
#[no_mangle]
pub static mut rl_done: c_int = 0;
#[no_mangle]
pub static mut rl_pending_input: c_int = 0;

#[no_mangle]
pub static mut rl_numeric_arg: c_int = 1;
#[no_mangle]
pub static mut rl_explicit_arg: c_int = 0;

#[no_mangle]
pub static mut rl_prompt: *mut c_char = ptr::null_mut();
#[no_mangle]
pub static mut rl_display_prompt: *mut c_char = ptr::null_mut();

#[no_mangle]
pub static mut rl_readline_name: *const c_char = DEFAULT_READLINE_NAME.as_ptr() as *const c_char;
#[no_mangle]
pub static mut rl_echoing_p: c_int = 1;
#[no_mangle]
pub static mut rl_insert_mode: c_int = 1;
#[no_mangle]
pub static mut rl_last_func: RlCommandFunc = None;

#[no_mangle]
pub static mut rl_key_sequence_length: c_int = 0;
#[no_mangle]
pub static mut rl_executing_keyseq: *mut c_char = ptr::null_mut();

#[no_mangle]
pub static mut rl_library_version: *const c_char = LIBRARY_VERSION.as_ptr() as *const c_char;
#[no_mangle]
pub static mut rl_readline_version: c_int = 0x0803;

// GNU initializes these in readline_initialize_everything().  `rl_instream`
// and `rl_outstream` start NULL and are bound to the C standard streams by
// rl_initialize() unless the application assigned them first.
#[no_mangle]
pub static mut rl_gnu_readline_p: c_int = 1;
#[no_mangle]
pub static mut rl_editing_mode: c_int = 1; /* emacs_mode */
#[no_mangle]
pub static mut rl_eof_found: c_int = 0;
#[no_mangle]
pub static mut rl_dispatching: c_int = 0;
#[no_mangle]
pub static mut rl_terminal_name: *const c_char = ptr::null();
#[no_mangle]
pub static mut rl_instream: *mut libc::FILE = ptr::null_mut();
#[no_mangle]
pub static mut rl_outstream: *mut libc::FILE = ptr::null_mut();
#[no_mangle]
pub static mut rl_prefer_env_winsize: c_int = 0;

#[cfg(unix)]
extern "C" {
    static mut stdin: *mut libc::FILE;
    static mut stdout: *mut libc::FILE;
}

// Saved prompt state (rl_save_prompt/rl_restore_prompt).
pub(crate) static mut SAVED_PROMPT: *mut c_char = ptr::null_mut();
pub(crate) static mut SAVED_DISPLAY_PROMPT: *mut c_char = ptr::null_mut();
pub(crate) static mut PROMPT_IS_SAVED: bool = false;

// --- Scalar accessors -----------------------------------------------------

macro_rules! scalar_accessors {
    ($get:ident, $set:ident, $value:ident, $ty:ty) => {
        #[allow(dead_code)]
        pub(crate) fn $get() -> $ty {
            unsafe { ptr::addr_of!($value).read() }
        }

        #[allow(dead_code)]
        pub(crate) fn $set(value: $ty) {
            unsafe { ptr::addr_of_mut!($value).write(value) }
        }
    };
}

scalar_accessors!(point, set_point, rl_point, c_int);
scalar_accessors!(end, set_end, rl_end, c_int);
scalar_accessors!(mark, set_mark, rl_mark, c_int);
scalar_accessors!(done, set_done, rl_done, c_int);
scalar_accessors!(pending_input, set_pending_input, rl_pending_input, c_int);
scalar_accessors!(numeric_arg, set_numeric_arg, rl_numeric_arg, c_int);
scalar_accessors!(explicit_arg, set_explicit_arg, rl_explicit_arg, c_int);
scalar_accessors!(
    readline_state,
    set_readline_state,
    rl_readline_state,
    std::os::raw::c_ulong
);
scalar_accessors!(insert_mode, set_insert_mode, rl_insert_mode, c_int);
scalar_accessors!(echoing, set_echoing, rl_echoing_p, c_int);
scalar_accessors!(
    key_sequence_length,
    set_key_sequence_length,
    rl_key_sequence_length,
    c_int
);
scalar_accessors!(
    readline_version,
    set_readline_version,
    rl_readline_version,
    c_int
);
scalar_accessors!(gnu_readline_p, set_gnu_readline_p, rl_gnu_readline_p, c_int);
scalar_accessors!(editing_mode, set_editing_mode, rl_editing_mode, c_int);
scalar_accessors!(eof_found, set_eof_found, rl_eof_found, c_int);
scalar_accessors!(dispatching, set_dispatching, rl_dispatching, c_int);
scalar_accessors!(
    prefer_env_winsize,
    set_prefer_env_winsize,
    rl_prefer_env_winsize,
    c_int
);

// --- Pointer accessors ----------------------------------------------------

pub(crate) fn line_buffer() -> *mut c_char {
    unsafe { ptr::addr_of!(rl_line_buffer).read() }
}

pub(crate) fn set_line_buffer(value: *mut c_char) {
    unsafe { ptr::addr_of_mut!(rl_line_buffer).write(value) }
}

pub(crate) fn prompt() -> *mut c_char {
    unsafe { ptr::addr_of!(rl_prompt).read() }
}

pub(crate) fn set_prompt(value: *mut c_char) {
    unsafe { ptr::addr_of_mut!(rl_prompt).write(value) }
}

pub(crate) fn display_prompt() -> *mut c_char {
    unsafe { ptr::addr_of!(rl_display_prompt).read() }
}

pub(crate) fn set_display_prompt(value: *mut c_char) {
    unsafe { ptr::addr_of_mut!(rl_display_prompt).write(value) }
}

pub(crate) fn terminal_name() -> *const c_char {
    unsafe { ptr::addr_of!(rl_terminal_name).read() }
}

pub(crate) fn set_terminal_name(value: *const c_char) {
    unsafe { ptr::addr_of_mut!(rl_terminal_name).write(value) }
}

pub(crate) fn instream() -> *mut libc::FILE {
    unsafe { ptr::addr_of!(rl_instream).read() }
}

pub(crate) fn set_instream(value: *mut libc::FILE) {
    unsafe { ptr::addr_of_mut!(rl_instream).write(value) }
}

pub(crate) fn outstream() -> *mut libc::FILE {
    unsafe { ptr::addr_of!(rl_outstream).read() }
}

pub(crate) fn set_outstream(value: *mut libc::FILE) {
    unsafe { ptr::addr_of_mut!(rl_outstream).write(value) }
}

/// GNU's `readline_initialize_everything` binds the unset streams to the C
/// standard streams.  An application assignment made before `rl_initialize`
/// is preserved.
pub(crate) fn initialize_streams() {
    #[cfg(unix)]
    {
        if instream().is_null() {
            // SAFETY: `stdin` is the process's standard FILE stream.
            set_instream(unsafe { ptr::addr_of!(stdin).read() });
        }
        if outstream().is_null() {
            // SAFETY: `stdout` is the process's standard FILE stream.
            set_outstream(unsafe { ptr::addr_of!(stdout).read() });
        }
    }
}

/// GNU sets `rl_terminal_name` from `TERM` during the first initialization.
/// The allocated copy lives for the process lifetime, as GNU's borrowed
/// environment pointer effectively does.
pub(crate) fn initialize_terminal_name() {
    if !terminal_name().is_null() {
        return;
    }
    if let Ok(value) = std::env::var("TERM") {
        if let Ok(name) = CString::new(value) {
            set_terminal_name(name.into_raw() as *const c_char);
        }
    }
}

/// The application's output stream, or stdout before `rl_initialize` has
/// bound it.
pub(crate) fn effective_outstream() -> *mut libc::FILE {
    let stream = outstream();
    if stream.is_null() {
        #[cfg(unix)]
        {
            // SAFETY: as above; a plain read of the C standard stream.
            return unsafe { ptr::addr_of!(stdout).read() };
        }
        #[cfg(not(unix))]
        {
            return ptr::null_mut();
        }
    }
    stream
}

pub(crate) fn empty_string() -> *mut c_char {
    EMPTY_STRING.as_ptr() as *mut c_char
}

/// Reinterpret a C `char *` as a byte pointer.  `c_char` is signed on some
/// targets, so the cast is necessary for portability even where it is a no-op.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn char_ptr_as_u8(pointer: *mut c_char) -> *mut u8 {
    pointer as *mut u8
}

pub(crate) fn last_func() -> RlCommandFunc {
    unsafe { ptr::addr_of!(rl_last_func).read() }
}

pub(crate) fn set_last_func(value: RlCommandFunc) {
    unsafe { ptr::addr_of_mut!(rl_last_func).write(value) }
}

pub(crate) fn set_argument_sign(value: c_int) {
    ARGUMENT_SIGN.store(value, Ordering::Relaxed);
}

/// Restore the default numeric argument, as GNU does before reading the next
/// command.
pub(crate) fn reset_argument() {
    set_numeric_arg(1);
    set_explicit_arg(0);
    set_argument_sign(1);
}

// --- Line buffer allocation ----------------------------------------------

/// Current capacity of the exported line buffer, including the trailing NUL.
pub(crate) fn line_capacity() -> usize {
    LINE_CAPACITY.load(Ordering::Relaxed)
}

pub(crate) fn set_line_capacity(capacity: usize) {
    LINE_CAPACITY.store(capacity, Ordering::Relaxed);
}

/// Allocate or grow the exported line buffer so it can hold `required` bytes,
/// including the trailing NUL.  Uses libc allocation so the pointer remains
/// compatible with C callers and `realloc`-style growth.
pub(crate) fn ensure_line_capacity(required: usize) {
    let current_capacity = LINE_CAPACITY.load(Ordering::Relaxed);
    let current = line_buffer();

    if current.is_null() {
        let capacity = required.max(DEFAULT_BUFFER_SIZE);
        let new_buffer = unsafe { libc::malloc(capacity) as *mut c_char };
        if new_buffer.is_null() {
            return;
        }
        unsafe { *new_buffer = 0 };
        set_line_buffer(new_buffer);
        LINE_CAPACITY.store(capacity, Ordering::Relaxed);
        return;
    }

    if required <= current_capacity {
        return;
    }

    let mut capacity = current_capacity.max(DEFAULT_BUFFER_SIZE);
    while capacity < required {
        capacity = capacity.saturating_mul(2);
    }

    let new_buffer =
        unsafe { libc::realloc(current as *mut libc::c_void, capacity) as *mut c_char };
    if new_buffer.is_null() {
        return;
    }
    set_line_buffer(new_buffer);
    LINE_CAPACITY.store(capacity, Ordering::Relaxed);
}

/// Copy `bytes` into the exported line buffer and make it the current contents.
pub(crate) fn set_line_bytes(bytes: &[u8]) {
    ensure_line_capacity(bytes.len() + 1);
    let buffer = line_buffer();
    if buffer.is_null() {
        return;
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), char_ptr_as_u8(buffer), bytes.len());
        *buffer.add(bytes.len()) = 0;
    }
    let length = bytes.len() as c_int;
    set_end(length);
    set_point(length);
}

pub(crate) fn reset_line_state() {
    ensure_line_capacity(1);
    let buffer = line_buffer();
    if !buffer.is_null() {
        unsafe { *buffer = 0 };
    }
    set_end(0);
    set_point(0);
    set_mark(0);
}

pub(crate) fn clamp_position(value: c_int) -> c_int {
    let end = end();
    if value < 0 {
        0
    } else if value > end {
        end
    } else {
        value
    }
}

pub(crate) fn clamp_point() {
    let fixed = clamp_position(point());
    set_point(fixed);
}

pub(crate) fn clamp_mark() {
    let fixed = clamp_position(mark());
    set_mark(fixed);
}

// --- Keyboard timeout -----------------------------------------------------

pub(crate) fn keyboard_timeout() -> c_int {
    KEYBOARD_TIMEOUT.load(Ordering::Relaxed) as c_int
}

pub(crate) fn set_keyboard_timeout(value: c_int) {
    if value >= 0 {
        KEYBOARD_TIMEOUT.store(value as usize, Ordering::Relaxed);
    }
}

// --- Saved process state --------------------------------------------------

/// Clean-room mirror of GNU's public `struct readline_state`.  Field order,
/// types, and the trailing 64-byte reserve match readline 8.3 so a caller
/// compiled against the GNU header can pass the same object to
/// `rl_save_state`/`rl_restore_state`.
#[repr(C)]
pub struct ReadlineState {
    pub point: c_int,
    pub end: c_int,
    pub mark: c_int,
    pub buflen: c_int,
    pub buffer: *mut c_char,
    pub ul: *mut crate::undo::UNDO_LIST,
    pub prompt: *mut c_char,

    pub rlstate: c_int,
    pub done: c_int,
    pub kmap: *mut crate::keymap::KeymapEntry,

    pub lastfunc: RlCommandFunc,
    pub insmode: c_int,
    pub edmode: c_int,
    pub kseq: *mut c_char,
    pub kseqlen: c_int,

    pub pendingin: c_int,
    pub inf: *mut libc::FILE,
    pub outf: *mut libc::FILE,
    pub macro_: *mut c_char,

    pub catchsigs: c_int,
    pub catchsigwinch: c_int,

    pub entryfunc: crate::completion::RlCompEntryFunc,
    pub menuentryfunc: crate::completion::RlCompEntryFunc,
    pub ignorefunc: crate::completion::RlIcppFunc,
    pub attemptfunc: crate::completion::RlCompletionFunc,
    pub wordbreakchars: *const c_char,

    pub reserved: [c_char; 64],
}

/// GNU `rl_save_state`.  Returns -1 for a NULL destination.
///
/// The signal toggles (`rl_catch_signals`, `rl_catch_sigwinch`) and
/// `rl_ignore_some_completions_function` are not part of this ABI slice; they
/// are recorded as zero and left untouched by `rl_restore_state` rather than
/// guessed.
#[no_mangle]
pub unsafe extern "C" fn rl_save_state(state: *mut ReadlineState) -> c_int {
    if state.is_null() {
        return -1;
    }

    let destination = &mut *state;
    destination.point = point();
    destination.end = end();
    destination.mark = mark();
    destination.buflen = line_capacity() as c_int;
    destination.buffer = line_buffer();
    destination.ul = unsafe { ptr::addr_of!(crate::undo::rl_undo_list).read() };
    destination.prompt = prompt();

    destination.rlstate = readline_state() as c_int;
    destination.done = done();
    destination.kmap = crate::keymap::rl_get_keymap();

    destination.lastfunc = last_func();
    destination.insmode = insert_mode();
    destination.edmode = editing_mode();
    destination.kseq = unsafe { ptr::addr_of!(rl_executing_keyseq).read() };
    destination.kseqlen = key_sequence_length();

    destination.pendingin = pending_input();
    destination.inf = instream();
    destination.outf = outstream();
    destination.macro_ = unsafe { ptr::addr_of!(crate::macros::rl_executing_macro).read() };

    destination.catchsigs = 0;
    destination.catchsigwinch = 0;

    destination.entryfunc = crate::completion::rl_completion_entry_function;
    destination.menuentryfunc = crate::completion::rl_menu_completion_entry_function;
    destination.ignorefunc = None;
    destination.attemptfunc = crate::completion::rl_attempted_completion_function;
    destination.wordbreakchars =
        crate::completion::rl_completer_word_break_characters as *const c_char;

    0
}

/// GNU `rl_restore_state`.  Returns -1 for a NULL source.
#[no_mangle]
pub unsafe extern "C" fn rl_restore_state(state: *mut ReadlineState) -> c_int {
    if state.is_null() {
        return -1;
    }

    let source = &*state;
    set_point(source.point);
    set_end(source.end);
    set_mark(source.mark);
    set_line_buffer(source.buffer);
    set_line_capacity(source.buflen.max(0) as usize);
    unsafe { ptr::addr_of_mut!(crate::undo::rl_undo_list).write(source.ul) };
    set_prompt(source.prompt);

    set_readline_state(source.rlstate as std::os::raw::c_ulong);
    set_done(source.done);
    crate::keymap::rl_set_keymap(source.kmap);

    set_last_func(source.lastfunc);
    set_insert_mode(source.insmode);
    set_editing_mode(source.edmode);
    ptr::addr_of_mut!(rl_executing_keyseq).write(source.kseq);
    set_key_sequence_length(source.kseqlen);
    set_instream(source.inf);
    set_outstream(source.outf);
    set_pending_input(source.pendingin);
    unsafe { ptr::addr_of_mut!(crate::macros::rl_executing_macro).write(source.macro_) };

    crate::completion::rl_completion_entry_function = source.entryfunc;
    crate::completion::rl_menu_completion_entry_function = source.menuentryfunc;
    crate::completion::rl_attempted_completion_function = source.attemptfunc;
    crate::completion::rl_completer_word_break_characters = source.wordbreakchars as *mut c_char;

    // SAFETY: deactivating the mark only clears internal bookkeeping.
    crate::editing::rl_deactivate_mark();

    0
}
