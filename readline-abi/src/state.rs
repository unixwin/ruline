//! Exported readline state globals and their safe accessors.
//!
//! The C ABI requires real data symbols at fixed addresses, so the globals are
//! `static mut`.  Rust code never creates references to them; access goes
//! through `addr_of!`/`addr_of_mut!` and these small helper functions.  That
//! keeps the exported layout intact while avoiding `static_mut_refs` aliasing.

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
