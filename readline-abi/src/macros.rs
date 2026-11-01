//! Keyboard macros, macro input replay, and the "read one key" seam.
//!
//! GNU records every dispatched key while `RL_STATE_MACRODEF` is set, keeps
//! the bytes in a growable buffer, and replays them by pushing the string onto
//! `rl_executing_macro`.  Replay is driven by the dispatcher: each iteration
//! asks `_rl_next_macro_key` for the next byte before falling back to the
//! terminal.
//!
//! This standalone library has no terminal loop, so it implements the state
//! machine and the replay core and exposes [`read_key`] as the dispatcher
//! seam.  `read_key` follows GNU's priority order: `rl_pending_input` first,
//! then the executing macro, then "no input" (-1).  Commands that need the
//! next key (`rl_char_search`, `rl_execute_named_command`, bracketed paste)
//! therefore work with queued input and macro input, and never fabricate a
//! terminal read.
//!
//! Key *capture* is the other half of the seam: the live dispatcher calls
//! [`record_dispatched_key`] for each key it dispatches.  The callback driver
//! and any future readline loop can do that without changing the command
//! bodies.

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use crate::state;

const RL_STATE_MACROINPUT: std::os::raw::c_ulong = 0x0000_0800;
const RL_STATE_MACRODEF: std::os::raw::c_ulong = 0x0000_1000;
const RL_STATE_DISPATCHING: std::os::raw::c_ulong = 0x0000_0020;
const RL_STATE_INPUTPENDING: std::os::raw::c_ulong = 0x0002_0000;

/// GNU's `MAX_MACRO_LEVEL`.
const MAX_MACRO_LEVEL: i32 = 16;

/// Bytes of the closing bracketed-paste sequence (`ESC [ 2 0 1 ~`).
const BRACK_PASTE_SUFFIX: &[u8] = b"\x1b[201~";

/// The macro currently being replayed.  GNU exports this pointer; it is a
/// malloc string owned by the library.
#[no_mangle]
pub static mut rl_executing_macro: *mut c_char = ptr::null_mut();

static EXECUTING_INDEX: AtomicI32 = AtomicI32::new(0);
static MACRO_LEVEL: AtomicI32 = AtomicI32::new(0);
static CURRENT_INDEX: AtomicI32 = AtomicI32::new(0);

/// The macro being defined.  `None` means GNU's `current_macro == NULL`.
static CURRENT_MACRO: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// Saved executing macros for nested `_rl_with_macro_input` calls.
static MACRO_STACK: Mutex<Vec<(usize, c_int)>> = Mutex::new(Vec::new());

fn current_macro() -> std::sync::MutexGuard<'static, Option<Vec<u8>>> {
    CURRENT_MACRO
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn macro_stack() -> std::sync::MutexGuard<'static, Vec<(usize, c_int)>> {
    MACRO_STACK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn executing_macro() -> *mut c_char {
    unsafe { ptr::addr_of!(rl_executing_macro).read() }
}

fn set_executing_macro(value: *mut c_char) {
    unsafe { ptr::addr_of_mut!(rl_executing_macro).write(value) };
}

fn state_set(bits: std::os::raw::c_ulong) {
    state::set_readline_state(state::readline_state() | bits);
}

fn state_clear(bits: std::os::raw::c_ulong) {
    state::set_readline_state(state::readline_state() & !bits);
}

fn copy_to_c_string(bytes: &[u8]) -> *mut c_char {
    let mut terminated = Vec::with_capacity(bytes.len() + 1);
    terminated.extend_from_slice(bytes);
    terminated.push(0);
    // SAFETY: the allocation is freed by the library when the macro is popped.
    let pointer = unsafe { libc::malloc(terminated.len()) as *mut c_char };
    if pointer.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(
            terminated.as_ptr(),
            state::char_ptr_as_u8(pointer),
            terminated.len(),
        );
    }
    pointer
}

/// Bytes of the current macro up to its first NUL, matching C string access.
fn macro_bytes() -> Option<Vec<u8>> {
    let current = current_macro();
    current.as_ref().map(|bytes| {
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        bytes[..end].to_vec()
    })
}

// --- Executing-macro stack ------------------------------------------------

fn push_executing_macro() {
    let mut stack = macro_stack();
    stack.push((
        executing_macro() as usize,
        EXECUTING_INDEX.load(Ordering::Relaxed),
    ));
    MACRO_LEVEL.fetch_add(1, Ordering::Relaxed);
}

/// Pop the saved macro, freeing the one that just finished.
pub(crate) fn pop_executing_macro() {
    let current = executing_macro();
    if !current.is_null() {
        // SAFETY: every pointer stored here came from malloc or a caller that
        // transferred ownership, matching GNU's contract.
        unsafe { libc::free(current as *mut c_void) };
    }
    set_executing_macro(ptr::null_mut());
    EXECUTING_INDEX.store(0, Ordering::Relaxed);

    let saved = macro_stack().pop();
    if let Some((pointer, index)) = saved {
        set_executing_macro(pointer as *mut c_char);
        EXECUTING_INDEX.store(index, Ordering::Relaxed);
    }
    MACRO_LEVEL.fetch_sub(1, Ordering::Relaxed);

    if executing_macro().is_null() {
        state_clear(RL_STATE_MACROINPUT);
    }
}

fn with_macro_input_owned(string: *mut c_char) {
    if MACRO_LEVEL.load(Ordering::Relaxed) > MAX_MACRO_LEVEL {
        if !string.is_null() {
            // SAFETY: ownership was transferred to us.
            unsafe { libc::free(string as *mut c_void) };
        }
        // SAFETY: the bell writer only touches the output stream.
        unsafe { crate::line::rl_ding() };
        return;
    }

    push_executing_macro();
    set_executing_macro(string);
    EXECUTING_INDEX.store(0, Ordering::Relaxed);
    state_set(RL_STATE_MACROINPUT);
}

fn with_macro_input_bytes(bytes: &[u8]) {
    let pointer = copy_to_c_string(bytes);
    if pointer.is_null() {
        return;
    }
    with_macro_input_owned(pointer);
}

// --- Dispatcher seam ------------------------------------------------------

/// Return the next byte of the executing macro, or 0 when none is available.
pub(crate) fn next_macro_key() -> c_int {
    let pointer = executing_macro();
    if pointer.is_null() {
        return 0;
    }

    let index = EXECUTING_INDEX.load(Ordering::Relaxed).max(0) as usize;
    // SAFETY: the executing macro is a NUL-terminated string and `index`
    // never advances past the terminator.
    let byte = unsafe { *state::char_ptr_as_u8(pointer).add(index) };
    if byte == 0 {
        pop_executing_macro();
        return next_macro_key();
    }
    EXECUTING_INDEX.store(index as i32 + 1, Ordering::Relaxed);
    c_int::from(byte)
}

/// Read one key for a command that needs lookahead.
///
/// Order matches GNU `rl_read_key`: queued pending input, then macro input,
/// then -1 because this library has no terminal loop.  Callers must treat -1
/// as "no key available" and document the dispatcher seam.
pub(crate) fn read_key() -> c_int {
    let pending = state::pending_input();
    if pending != 0 {
        state::set_pending_input(0);
        state_clear(RL_STATE_INPUTPENDING);
        return pending;
    }

    // `next_macro_key` uses 0 for "the macro is exhausted"; `rl_read_key`
    // reports a negative value when there is no terminal to read from.
    match next_macro_key() {
        0 => crate::terminal::pop_stuffed().unwrap_or(-1),
        key => key,
    }
}

/// Record a key while a macro is being defined.
///
/// The live dispatcher calls this for every key it dispatches, before invoking
/// the bound command (readline.c `_rl_dispatch_subseq`).
#[allow(dead_code)]
pub(crate) fn record_dispatched_key(key: c_int) {
    if key > 0 && state::readline_state() & RL_STATE_MACRODEF != 0 {
        add_macro_char(key);
    }
}

/// Append a byte to the macro under construction (`_rl_add_macro_char`).
pub(crate) fn add_macro_char(character: c_int) {
    let mut current = current_macro();
    let bytes = current.get_or_insert_with(Vec::new);
    let index = CURRENT_INDEX.load(Ordering::Relaxed).max(0) as usize;
    if bytes.len() <= index {
        bytes.resize(index + 1, 0);
    }
    bytes[index] = character as u8;
    bytes.truncate(index + 1);
    CURRENT_INDEX.store(index as i32 + 1, Ordering::Relaxed);
}

/// Discard the macro being defined and any executing macro.
pub(crate) fn kill_kbd_macro() {
    // Drain the saved-macro stack first, mirroring `_rl_abort_internal`'s
    // `while (rl_executing_macro) _rl_pop_executing_macro ()`.
    while !executing_macro().is_null() {
        pop_executing_macro();
    }
    let mut stack = macro_stack();
    for (pointer, _) in stack.drain(..) {
        if pointer != 0 {
            // SAFETY: saved entries are library-owned macro strings.
            unsafe { libc::free(pointer as *mut c_void) };
        }
    }
    drop(stack);
    MACRO_LEVEL.store(0, Ordering::Relaxed);

    *current_macro() = None;
    CURRENT_INDEX.store(0, Ordering::Relaxed);
    state_clear(RL_STATE_MACRODEF);
}

fn abort_internal() -> c_int {
    // SAFETY: the bell writer only touches the output stream.
    unsafe { crate::line::rl_ding() };
    state::set_pending_input(0);
    state_clear(RL_STATE_INPUTPENDING);
    state::reset_argument();
    kill_kbd_macro();
    1
}

// --- Macro command ABI ----------------------------------------------------

/// Begin defining a keyboard macro.
#[no_mangle]
pub unsafe extern "C" fn rl_start_kbd_macro(_count: c_int, _key: c_int) -> c_int {
    if state::readline_state() & RL_STATE_MACRODEF != 0 {
        return abort_internal();
    }

    if state::explicit_arg() != 0 {
        if let Some(bytes) = macro_bytes() {
            with_macro_input_bytes(&bytes);
        }
    } else {
        CURRENT_INDEX.store(0, Ordering::Relaxed);
    }

    state_set(RL_STATE_MACRODEF);
    0
}

/// Stop defining a keyboard macro and optionally replay it.
#[no_mangle]
pub unsafe extern "C" fn rl_end_kbd_macro(count: c_int, _key: c_int) -> c_int {
    if state::readline_state() & RL_STATE_MACRODEF == 0 {
        return abort_internal();
    }

    {
        let mut current = current_macro();
        if let Some(bytes) = current.as_mut() {
            let index = CURRENT_INDEX
                .load(Ordering::Relaxed)
                .saturating_sub(state::key_sequence_length())
                .max(0) as usize;
            if index < bytes.len() {
                bytes.truncate(index);
            }
            CURRENT_INDEX.store(index as i32, Ordering::Relaxed);
        }
    }

    state_clear(RL_STATE_MACRODEF);
    rl_call_last_kbd_macro(count - 1, 0)
}

/// Replay the most recently defined macro `count` times.
#[no_mangle]
pub unsafe extern "C" fn rl_call_last_kbd_macro(count: c_int, _key: c_int) -> c_int {
    let bytes = match macro_bytes() {
        Some(bytes) => bytes,
        None => return abort_internal(),
    };

    if state::readline_state() & RL_STATE_MACRODEF != 0 {
        crate::line::rl_ding(); // no recursive macros
        let mut current = current_macro();
        if let Some(bytes) = current.as_mut() {
            let index = CURRENT_INDEX.load(Ordering::Relaxed).max(0) as usize;
            if index > 0 {
                bytes.truncate(index - 1);
                CURRENT_INDEX.store(index as i32 - 1, Ordering::Relaxed);
            }
        }
        return 0;
    }

    for _ in 0..count.max(0) {
        with_macro_input_bytes(&bytes);
    }
    0
}

/// Print the current macro with backslash escapes, like GNU's
/// `rl_print_last_kbd_macro`.
#[no_mangle]
pub unsafe extern "C" fn rl_print_last_kbd_macro(_count: c_int, _key: c_int) -> c_int {
    let bytes = match macro_bytes() {
        Some(bytes) => bytes,
        None => {
            crate::line::rl_ding();
            return 0;
        }
    };

    let printable = untranslate_macro_value(&bytes, true);
    crate::line::rl_crlf();
    {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(&printable);
        let _ = stdout.flush();
    }
    crate::line::rl_crlf();
    crate::completion::redisplay_with_hook();
    0
}

/// Make `macro` the input source.  Ownership of the malloc string transfers
/// to the library, exactly as in GNU's `rl_push_macro_input`.
#[no_mangle]
pub unsafe extern "C" fn rl_push_macro_input(macro_string: *mut c_char) {
    with_macro_input_owned(macro_string);
}

// --- Read-a-command-name and bracketed paste ------------------------------

/// Read a command name from the input source.
///
/// This is the portable half of GNU's `_rl_read_command_name`: printable bytes
/// accumulate, backspace/`C-h` erase, `C-u` clears the word, `C-w` erases the
/// previous word, and newline/return terminates.  The terminal editor that GNU
/// runs around this loop (completion, prompt, redisplay) is the dispatcher's
/// job; when no input is available the function returns `None`.
fn read_command_name() -> Option<Vec<u8>> {
    let mut name: Vec<u8> = Vec::new();
    loop {
        let key = read_key();
        if key < 0 {
            return None;
        }
        match key as u8 {
            0 => break,
            b'\r' | b'\n' => break,
            0x7f | 0x08 => {
                name.pop();
            }
            0x15 => name.clear(),
            0x17 => {
                while let Some(byte) = name.pop() {
                    if !byte.is_ascii_alphanumeric() {
                        name.push(byte);
                        break;
                    }
                }
            }
            byte => name.push(byte),
        }
        if name.len() > 1024 {
            break;
        }
    }
    Some(name)
}

/// Read a command name and invoke it, mirroring GNU's
/// `rl_execute_named_command`.
///
/// The name comes from pending or macro input; a terminal read is left to the
/// dispatcher.  With no input the command reports failure without consuming a
/// fake key.
#[no_mangle]
pub unsafe extern "C" fn rl_execute_named_command(count: c_int, key: c_int) -> c_int {
    let Some(mut name) = read_command_name() else {
        return 1;
    };
    name.push(0);
    if name.len() == 1 {
        return 1;
    }

    let function = crate::keymap::rl_named_function(name.as_ptr() as *const c_char);
    match function {
        Some(function) => {
            let previous_state = state::readline_state();
            state::set_readline_state(previous_state | RL_STATE_DISPATCHING);
            let result = function(count, key);
            if previous_state & RL_STATE_DISPATCHING == 0 {
                state::set_readline_state(state::readline_state() & !RL_STATE_DISPATCHING);
            }
            result
        }
        None => {
            crate::line::rl_ding();
            1
        }
    }
}

/// Read bracketed-paste text until the closing sequence.
///
/// Returns the pasted bytes.  Input comes from `read_key`; when the input
/// source is exhausted the paste ends with whatever was read so far, matching
/// GNU's loop termination on a negative key.
fn bracketed_text() -> Vec<u8> {
    let mut buffer = Vec::new();
    loop {
        let key = read_key();
        if key < 0 {
            break;
        }
        if state::readline_state() & RL_STATE_MACRODEF != 0 {
            add_macro_char(key);
        }
        let byte = if key == b'\r' as c_int {
            b'\n'
        } else {
            key as u8
        };
        buffer.push(byte);
        if buffer.len() >= BRACK_PASTE_SUFFIX.len() && buffer.ends_with(BRACK_PASTE_SUFFIX) {
            buffer.truncate(buffer.len() - BRACK_PASTE_SUFFIX.len());
            break;
        }
    }
    buffer
}

/// Insert the bracketed-paste payload as a single unit.
#[no_mangle]
pub unsafe extern "C" fn rl_bracketed_paste_begin(_count: c_int, _key: c_int) -> c_int {
    let text = bracketed_text();
    state::set_mark(state::point());

    let mut terminated = text.clone();
    terminated.push(0);
    let inserted = crate::line::rl_insert_text(terminated.as_ptr() as *const c_char);
    let retval = c_int::from(inserted != text.len() as c_int);

    if crate::keymap::variable_boolean("enable-active-region") {
        crate::editing::rl_activate_mark();
    }
    retval
}

/// GNU implements this only for `_WIN32`; the POSIX library has no clipboard
/// primitive, so the command is an explicit no-op rather than a terminal read.
#[no_mangle]
pub unsafe extern "C" fn rl_paste_from_clipboard(_count: c_int, _key: c_int) -> c_int {
    0
}

// --- Printing helper ------------------------------------------------------

fn escape_character(character: u8) -> Option<u8> {
    match character {
        0x07 => Some(b'a'),
        0x08 => Some(b'b'),
        0x0c => Some(b'f'),
        b'\n' => Some(b'n'),
        b'\r' => Some(b'r'),
        b'\t' => Some(b't'),
        0x0b => Some(b'v'),
        _ => None,
    }
}

/// Port of bind.c `_rl_untranslate_macro_value`.
fn untranslate_macro_value(sequence: &[u8], use_escapes: bool) -> Vec<u8> {
    let mut output = Vec::with_capacity(sequence.len() * 8 + 1);
    for &raw in sequence {
        let mut character = raw;
        if character > 0x7f {
            output.extend_from_slice(b"\\M-");
            character &= 0x7f;
        }

        if character == 0x1b {
            output.push(b'\\');
            character = b'e';
        } else if character < 0x20 {
            output.push(b'\\');
            if use_escapes {
                if let Some(escape) = escape_character(character) {
                    character = escape;
                } else {
                    output.extend_from_slice(b"C-");
                    character = (character | 0x40).to_ascii_uppercase().to_ascii_lowercase();
                }
            } else {
                output.extend_from_slice(b"C-");
                character = (character | 0x40).to_ascii_uppercase().to_ascii_lowercase();
            }
        } else if character == 0x7f {
            output.extend_from_slice(b"\\C-");
            character = b'?';
        }

        if character == b'\\' || character == b'"' {
            output.push(b'\\');
        }
        output.push(character);
    }
    output
}

// --- Test hooks -----------------------------------------------------------

/// C string of the current macro, for in-crate tests.  Not exported.
#[cfg(test)]
pub(crate) fn current_macro_bytes() -> Option<Vec<u8>> {
    macro_bytes()
}

/// Read the next replay byte, for in-crate tests.  Not exported.
#[cfg(test)]
pub(crate) fn test_read_key() -> c_int {
    read_key()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn reset() {
        kill_kbd_macro();
        *macro_stack() = Vec::new();
        MACRO_LEVEL.store(0, Ordering::Relaxed);
    }

    #[test]
    fn macro_definition_replay_and_print_escape() {
        reset();
        // SAFETY: the test lock is not shared with other ABI tests here, and
        // these are the same globals the C ABI uses.
        unsafe {
            state::set_readline_state(0);
            assert_eq!(rl_start_kbd_macro(1, 0), 0);
            assert_ne!(state::readline_state() & RL_STATE_MACRODEF, 0);
            add_macro_char(b'a' as c_int);
            add_macro_char(b'\n' as c_int);
            assert_eq!(current_macro_bytes(), Some(b"a\n".to_vec()));
            assert_eq!(rl_end_kbd_macro(1, 0), 0);
            assert_eq!(state::readline_state() & RL_STATE_MACRODEF, 0);

            assert_eq!(rl_call_last_kbd_macro(1, 0), 0);
            assert_eq!(test_read_key(), b'a' as c_int);
            assert_eq!(test_read_key(), b'\n' as c_int);
            assert_eq!(test_read_key(), -1);
        }
        assert_eq!(untranslate_macro_value(b"a\n", true), b"a\\n".to_vec());
        assert_eq!(untranslate_macro_value(b"\x01", false), b"\\C-a".to_vec());
        assert_eq!(untranslate_macro_value(b"\x1b", true), b"\\e".to_vec());
        assert_eq!(
            untranslate_macro_value(b"\xff", true),
            b"\\M-\\C-?".to_vec()
        );
        reset();
    }

    fn owned_macro(text: &str) -> *mut c_char {
        let text = CString::new(text).unwrap();
        // SAFETY: strdup returns libc storage, which is what the macro stack
        // frees.
        unsafe { libc::strdup(text.as_ptr()) }
    }

    #[test]
    fn push_macro_input_reads_in_order_and_restores_previous() {
        reset();
        let first = owned_macro("one");
        // SAFETY: the pointer is malloc storage and ownership transfers.
        unsafe { rl_push_macro_input(first) };
        assert_eq!(test_read_key(), b'o' as c_int);
        let second = owned_macro("two");
        unsafe { rl_push_macro_input(second) };
        assert_eq!(test_read_key(), b't' as c_int);
        assert_eq!(test_read_key(), b'w' as c_int);
        assert_eq!(test_read_key(), b'o' as c_int);
        // Exhausting "two" pops back to "one" at its saved index.
        assert_eq!(test_read_key(), b'n' as c_int);
        assert_eq!(test_read_key(), b'e' as c_int);
        assert_eq!(test_read_key(), -1);
        reset();
    }

    #[test]
    fn command_name_reader_handles_backspace() {
        reset();
        let name = owned_macro("forward-chax\x7fr\n");
        // SAFETY: ownership transfers to the macro input stack.
        unsafe { rl_push_macro_input(name) };
        assert_eq!(read_command_name(), Some(b"forward-char".to_vec()));
        reset();
    }

    #[test]
    fn bracketed_text_strips_closing_sequence() {
        reset();
        let pasted = owned_macro("hello\x1b[201~tail");
        // SAFETY: ownership transfers to the macro input stack.
        unsafe { rl_push_macro_input(pasted) };
        assert_eq!(bracketed_text(), b"hello".to_vec());
        reset();
    }
}
