//! Callback-mode readline and the byte-driven input loop.
//!
//! GNU `callback.c` is the oracle.  `rl_callback_handler_install` prepares the
//! terminal, prints the prompt and stores a line handler; each
//! `rl_callback_read_char` consumes one available key and dispatches it;
//! accepting a line (or EOF/Ctrl-C) tears the terminal down, invokes the
//! handler, restores the terminal and prints the next prompt unless the
//! handler removed itself.
//!
//! # Input sources
//!
//! A key is taken, in GNU's order, from the pushed-input queue (macro text),
//! from `rl_pending_input`, from an application `rl_getc_function`, or from
//! stdin.  When `rl_event_hook` is installed the library polls
//! `rl_input_available_hook` (or stdin) and calls the event hook until a byte
//! is available; this mirrors `rl_read_key`'s event loop.
//!
//! # Dispatch
//!
//! Keys go through the current keymap, including one `ISKMAP` level (C-x,
//! ESC) across callbacks and `ISMACR` macro expansion.  The editing command
//! bodies in `keymap_defaults.rs` are still being filled in by a parallel work
//! item, so this module calls the binding when it exists and
//! falls back to a small, byte-level safe subset (`self-insert`, `accept-line`,
//! `backward-delete-char`, `delete-char`, character motion, `abort`) only when
//! the call left the line state unchanged.  A command that did do something
//! is never applied twice, so replacing the placeholders with real bodies
//! needs no change here.

use std::collections::VecDeque;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::completion::{RlGetcFunc, RlHookFunc};
use crate::keymap::{self, Keymap, ISFUNC, ISKMAP, ISMACR};
use crate::line;
use crate::state;
use crate::terminal;

/// `rl_vcpfunc_t` — the application's completed-line callback.
pub type RlVcpFunc = Option<unsafe extern "C" fn(*mut c_char)>;

static LINE_FUNC: Mutex<RlVcpFunc> = Mutex::new(None);
static IN_HANDLER: AtomicBool = AtomicBool::new(false);
static PUSHED: Mutex<VecDeque<u8>> = Mutex::new(VecDeque::new());
static MULTIKEY: AtomicUsize = AtomicUsize::new(0);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static ABORT_LINE: AtomicBool = AtomicBool::new(false);

// --- Exported hook storage access ----------------------------------------

fn event_hook() -> RlHookFunc {
    // SAFETY: the global is a plain function pointer written by the C caller.
    unsafe { ptr::addr_of!(crate::completion::rl_event_hook).read() }
}

fn input_available_hook() -> RlHookFunc {
    // SAFETY: as above.
    unsafe { ptr::addr_of!(crate::completion::rl_input_available_hook).read() }
}

fn getc_function() -> RlGetcFunc {
    // SAFETY: as above.
    unsafe { ptr::addr_of!(crate::completion::rl_getc_function).read() }
}

/// A `FILE *` for standard input, created once.  GNU passes `rl_instream` to
/// `rl_getc_function`; this adapter's input stream is stdin.  The descriptor
/// stays owned by the process (the FILE is never closed).
#[cfg(unix)]
fn stdin_file() -> *mut libc::FILE {
    static STDIN_FILE: Mutex<usize> = Mutex::new(0);
    let mut cached = STDIN_FILE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *cached == 0 {
        // SAFETY: fd 0 is open for the lifetime of the process; `fdopen` wraps
        // it in a stream without closing the descriptor.
        let file = unsafe { libc::fdopen(0, c"r".as_ptr()) };
        *cached = file as usize;
    }
    *cached as *mut libc::FILE
}

/// Non-Unix fallback: there is no `fdopen`/stdin stream to lend the callback.
/// A host that installs `rl_getc_function` should ignore the argument.
#[cfg(not(unix))]
fn stdin_file() -> *mut libc::FILE {
    ptr::null_mut()
}

fn current_keymap() -> Keymap {
    // SAFETY: returns the keymap currently selected through the ABI.
    let map = unsafe { keymap::rl_get_keymap() };
    if map.is_null() {
        ptr::addr_of_mut!(keymap::emacs_standard_keymap).cast::<keymap::KeymapEntry>()
    } else {
        map
    }
}

// --- Key input ------------------------------------------------------------

fn pop_pushed() -> Option<u8> {
    PUSHED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .pop_front()
}

fn pushed_available() -> bool {
    !PUSHED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty()
}

#[cfg(unix)]
fn poll_stdin(timeout_microseconds: c_int) -> bool {
    let timeout = timeout_microseconds.max(0) as i64;
    let mut readfds: libc::fd_set = unsafe { std::mem::zeroed() };
    // SAFETY: the set is stack-owned and initialised for fd 0 only.
    unsafe {
        libc::FD_ZERO(&mut readfds);
        libc::FD_SET(0, &mut readfds);
    }
    let mut timeval = libc::timeval {
        tv_sec: (timeout / 1_000_000) as libc::time_t,
        tv_usec: (timeout % 1_000_000) as libc::suseconds_t,
    };
    // SAFETY: one descriptor, no write/exception sets.
    unsafe {
        libc::select(
            1,
            &mut readfds,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut timeval,
        ) > 0
    }
}

#[cfg(not(unix))]
fn poll_stdin(_timeout_microseconds: c_int) -> bool {
    // Windows consoles report availability through the console API.  Without
    // it, an installed rl_input_available_hook is the supported way to tell
    // this loop that a key is ready.
    false
}

/// Is a key available without blocking?  `rl_input_available_hook` has
/// priority, matching GNU's `_rl_input_available`.
fn input_available() -> bool {
    if let Some(hook) = input_available_hook() {
        // SAFETY: the application owns the callback and promised this ABI.
        return unsafe { hook() } != 0;
    }
    if state::pending_input() != 0 || pushed_available() {
        return true;
    }
    if getc_function().is_some() {
        // An opaque getc callback defines its own readiness; it returns EOF
        // when the stream is exhausted.
        return true;
    }
    poll_stdin(state::keyboard_timeout())
}

fn read_byte() -> c_int {
    if let Some(byte) = pop_pushed() {
        return c_int::from(byte);
    }

    let pending = state::pending_input();
    if pending != 0 {
        // SAFETY: clears the exported pending-input state.
        unsafe { terminal::rl_clear_pending_input() };
        return pending;
    }

    if let Some(getc) = getc_function() {
        // SAFETY: `stdin_file` is a valid FILE* and the callback follows the
        // documented ABI.  GNU passes `rl_instream`, which is stdin by default.
        return unsafe { getc(stdin_file()) };
    }

    #[cfg(unix)]
    {
        let mut byte: u8 = 0;
        loop {
            // SAFETY: one byte is read into a stack value from the standard
            // input descriptor.
            let read = unsafe { libc::read(0, &mut byte as *mut u8 as *mut c_void, 1) };
            if read == 1 {
                return c_int::from(byte);
            }
            if read == 0 {
                return libc::EOF;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return libc::EOF;
        }
    }

    // Non-Unix: reading the console needs a host input loop.  Report EOF so
    // callers do not spin; install `rl_getc_function` or
    // `rl_input_available_hook` plus an event hook to feed bytes.
    #[cfg(not(unix))]
    libc::EOF
}

/// Read one key, running `rl_event_hook` while no input is available.
///
/// `None` means a signal interrupted the read; the caller aborts the line.
/// Display commands (`rl_skip_csi_sequence`, `rl_arrow_keys`) share this path.
pub(crate) fn read_key() -> Option<c_int> {
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return None;
        }

        let Some(event) = event_hook() else {
            return Some(read_byte());
        };

        if input_available() {
            return Some(read_byte());
        }

        terminal::check_signals();
        if INTERRUPTED.load(Ordering::SeqCst) {
            return None;
        }
        if state::done() != 0 {
            // GNU returns a newline so the caller completes the line.
            return Some(c_int::from(b'\n'));
        }

        // SAFETY: the application owns the callback and promised this ABI.
        unsafe { event() };
    }
}

// --- Keymap dispatch ------------------------------------------------------

fn function_from_pointer(
    pointer: *mut c_void,
) -> Option<unsafe extern "C" fn(c_int, c_int) -> c_int> {
    if pointer.is_null() {
        None
    } else {
        // SAFETY: keymap entries store a `rl_command_func_t *` for ISFUNC.
        Some(unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(c_int, c_int) -> c_int>(pointer)
        })
    }
}

/// Recover the funmap name of a keymap binding so the fallback can tell which
/// command a key is bound to.  Only the generated default table is consulted;
/// application-defined commands keep their own (non-fallback) behaviour.
fn default_command_name(pointer: *mut c_void) -> Option<&'static str> {
    let address = pointer as usize;
    for (name, function) in crate::keymap_defaults::DEFAULT_FUNMAP {
        if let Some(function) = function {
            if *function as *const () as usize == address {
                return Some(name);
            }
        }
    }
    None
}

fn is_safe_command(name: &str) -> bool {
    matches!(
        name,
        "self-insert"
            | "accept-line"
            | "backward-delete-char"
            | "delete-char"
            | "backward-char"
            | "forward-char"
            | "beginning-of-line"
            | "end-of-line"
            | "abort"
    )
}

#[derive(PartialEq)]
struct Snapshot {
    line: Vec<u8>,
    point: c_int,
    end: c_int,
    done: c_int,
}

fn snapshot() -> Snapshot {
    let end = state::end().max(0) as usize;
    let buffer = state::line_buffer();
    let line = if buffer.is_null() {
        Vec::new()
    } else {
        // SAFETY: rl_line_buffer holds at least rl_end bytes.
        unsafe { std::slice::from_raw_parts(buffer as *const u8, end) }.to_vec()
    };
    Snapshot {
        line,
        point: state::point(),
        end: state::end(),
        done: state::done(),
    }
}

fn insert_byte(byte: u8) {
    let text = [byte as c_char, 0];
    // SAFETY: `text` is a NUL-terminated one-byte string.
    unsafe { line::rl_insert_text(text.as_ptr()) };
}

fn backward_delete_char() {
    let point = state::clamp_position(state::point());
    if point > 0 {
        // SAFETY: the range is inside the line buffer.
        unsafe { line::rl_delete_text(point - 1, point) };
        state::set_point(point - 1);
    }
}

fn delete_char_at_point() {
    let point = state::clamp_position(state::point());
    if point < state::end() {
        // SAFETY: the range is inside the line buffer.
        unsafe { line::rl_delete_text(point, point + 1) };
        state::set_point(point);
    }
}

fn accept_line() {
    state::set_done(1);
    terminal::add_state(terminal::RL_STATE_DONE);
}

/// Abort the current line without marking EOF.  This is the byte-stream
/// counterpart of the `Ctrl-C`/SIGINT path in `readline()`'s Reedline loop.
fn abort_line() {
    state::reset_line_state();
    state::set_done(1);
    terminal::add_state(terminal::RL_STATE_DONE);
    ABORT_LINE.store(true, Ordering::SeqCst);
}

fn abort_command() {
    // GNU's rl_abort rings, drops pending input, and cancels multi-key state
    // without ending the line.
    // SAFETY: default bell output only.
    unsafe { line::rl_ding() };
    // SAFETY: clears the exported pending-input state.
    unsafe { terminal::rl_clear_pending_input() };
    MULTIKEY.store(0, Ordering::SeqCst);
    terminal::remove_state(terminal::RL_STATE_MULTIKEY);
    state::set_last_func(None);
}

fn fallback_command(name: &str, byte: u8) {
    match name {
        "self-insert" => insert_byte(byte),
        "accept-line" => accept_line(),
        "backward-delete-char" => backward_delete_char(),
        "delete-char" => delete_char_at_point(),
        "backward-char" => state::set_point(state::clamp_position(state::point() - 1)),
        "forward-char" => state::set_point(state::clamp_position(state::point() + 1)),
        "beginning-of-line" => state::set_point(0),
        "end-of-line" => state::set_point(state::end()),
        "abort" => abort_command(),
        _ => {}
    }
}

/// Bytes that are not bound in the active keymap.  GNU binds every printable
/// character in its default maps, so this is mainly the control characters
/// (Ctrl-C/Ctrl-D) and a safety net for sparse application keymaps.
fn unbound_fallback(byte: u8) {
    match byte {
        0x03 => abort_line(),
        0x04 => delete_char_at_point(),
        0x08 | 0x7f => backward_delete_char(),
        b'\n' | b'\r' => accept_line(),
        0x20..=0x7e | 0x80..=0xff => insert_byte(byte),
        _ => {}
    }
}

fn run_command(pointer: *mut c_void, byte: u8) {
    let Some(function) = function_from_pointer(pointer) else {
        unbound_fallback(byte);
        return;
    };

    let name = default_command_name(pointer);
    let before = snapshot();
    state::set_last_func(Some(function));

    terminal::add_state(terminal::RL_STATE_DISPATCHING);
    state::set_dispatching(1);
    // SAFETY: the binding is an ABI command function; a default count of one
    // and the dispatching key match GNU `_rl_dispatch`.
    let _ = unsafe { function(1, c_int::from(byte)) };
    state::set_dispatching(0);
    terminal::remove_state(terminal::RL_STATE_DISPATCHING);

    if let Some(name) = name {
        if is_safe_command(name) && snapshot() == before {
            fallback_command(name, byte);
        }
    }
}

fn entry_at(map: Keymap, byte: u8) -> Option<keymap::KeymapEntry> {
    if map.is_null() {
        return None;
    }
    // SAFETY: KEYMAP_SIZE is 257, one slot per byte plus ANYOTHERKEY, so every
    // u8 index is in range.
    Some(unsafe { *map.add(byte as usize) })
}

/// Dispatch one key in `map`.  Multi-key maps and macros are consumed by
/// later `rl_callback_read_char` calls, matching GNU's callback state machine.
fn process_key(key: c_int, map: Keymap) {
    let byte = (key & 0xff) as u8;

    let Some(entry) = entry_at(map, byte) else {
        unbound_fallback(byte);
        return;
    };

    match entry.r#type {
        ISKMAP => {
            let child = entry.function as Keymap;
            if child.is_null() {
                unbound_fallback(byte);
            } else {
                MULTIKEY.store(child as usize, Ordering::SeqCst);
                terminal::add_state(terminal::RL_STATE_MULTIKEY);
            }
        }
        ISMACR => {
            if !entry.function.is_null() {
                // SAFETY: ISMACR entries hold a NUL-terminated macro string.
                let text = unsafe { CStr::from_ptr(entry.function as *const c_char) };
                let mut pushed = PUSHED
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for byte in text.to_bytes().iter().rev() {
                    pushed.push_front(*byte);
                }
            }
        }
        ISFUNC => {
            if entry.function.is_null() {
                unbound_fallback(byte);
            } else {
                run_command(entry.function, byte);
            }
        }
        _ => unbound_fallback(byte),
    }
}

// --- Callback lifecycle ---------------------------------------------------

fn duplicate_line() -> *mut c_char {
    let end = state::end().max(0) as usize;
    let buffer = state::line_buffer();
    // SAFETY: malloc-compatible allocation handed to the application.
    let copy = unsafe { libc::malloc(end + 1) } as *mut u8;
    if copy.is_null() {
        return ptr::null_mut();
    }
    if end > 0 && !buffer.is_null() {
        // SAFETY: copying rl_end bytes plus the terminator below.
        unsafe { ptr::copy_nonoverlapping(buffer as *const u8, copy, end) };
    }
    // SAFETY: the extra byte is the NUL reserved above.
    unsafe { *copy.add(end) = 0 };
    copy as *mut c_char
}

fn free_line(line: *mut c_char) {
    if !line.is_null() {
        // SAFETY: allocated by duplicate_line with malloc.
        unsafe { libc::free(line as *mut c_void) };
    }
}

/// GNU `_rl_callback_newline`: ready a fresh line and repaint the prompt.
fn callback_newline() {
    // SAFETY: resets the exported line state and deferred initialization.
    unsafe { line::rl_initialize() };
    terminal::remove_state(
        terminal::RL_STATE_DONE | terminal::RL_STATE_EOF | terminal::RL_STATE_TIMEOUT,
    );

    if !IN_HANDLER.swap(true, Ordering::SeqCst) {
        terminal::prep_for_input();
    }

    crate::completion::run_startup_hook();
    crate::completion::redisplay_with_hook();
    crate::completion::run_pre_input_hook();
    terminal::check_signals();
}

#[derive(Clone, Copy)]
enum LineEnd {
    Accepted,
    Eof,
    /// An interrupt: the line is dropped and the handler sees NULL.
    Aborted,
}

fn finish_line(kind: LineEnd) {
    let line_ptr = match kind {
        LineEnd::Accepted => duplicate_line(),
        LineEnd::Eof | LineEnd::Aborted => ptr::null_mut(),
    };

    state::set_done(1);
    match kind {
        LineEnd::Accepted => terminal::add_state(terminal::RL_STATE_DONE),
        LineEnd::Eof => {
            state::set_eof_found(1);
            terminal::add_state(terminal::RL_STATE_DONE | terminal::RL_STATE_EOF);
        }
        LineEnd::Aborted => terminal::add_state(terminal::RL_STATE_DONE),
    }

    if matches!(kind, LineEnd::Accepted)
        && !(crate::display::erase_empty_line() != 0 && state::end() == 0)
    {
        // GNU's final redisplay ends the accepted line before the handler runs.
        // SAFETY: writes one newline to the ABI output stream.
        unsafe { line::rl_crlf() };
    }

    terminal::deprep_for_input();
    // SAFETY: restores the application's signal dispositions.
    unsafe { terminal::rl_clear_signals() };
    IN_HANDLER.store(false, Ordering::SeqCst);

    let handler = *LINE_FUNC
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(handler) = handler {
        // SAFETY: the application owns the callback; the line is malloc-compatible
        // and NULL only for EOF/abort.
        unsafe { handler(line_ptr) };
    } else {
        free_line(line_ptr);
    }

    let still_installed = LINE_FUNC
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_some();

    if still_installed {
        // `rl_callback_handler_install` during the handler already prepared the
        // next line; only reprompt when the handler left the callback idle.
        if !IN_HANDLER.load(Ordering::SeqCst) {
            callback_newline();
        }
    } else if state::end() > 0 {
        // GNU clears a leftover line when the handler removed itself.
        state::reset_line_state();
    }
}

/// Install a callback handler, prepare the terminal, and print the prompt.
/// Reinstalling replaces the handler and starts a new line.
#[no_mangle]
pub unsafe extern "C" fn rl_callback_handler_install(prompt: *const c_char, linefunc: RlVcpFunc) {
    crate::line::set_prompt(prompt);
    terminal::add_state(terminal::RL_STATE_CALLBACK);
    *LINE_FUNC
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = linefunc;

    PUSHED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    MULTIKEY.store(0, Ordering::SeqCst);
    terminal::remove_state(terminal::RL_STATE_MULTIKEY);
    INTERRUPTED.store(false, Ordering::SeqCst);
    ABORT_LINE.store(false, Ordering::SeqCst);

    callback_newline();
}

/// Remove the handler and restore the terminal and signal dispositions.
#[no_mangle]
pub unsafe extern "C" fn rl_callback_handler_remove() {
    *LINE_FUNC
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    terminal::remove_state(terminal::RL_STATE_CALLBACK);
    terminal::check_signals();

    if IN_HANDLER.swap(false, Ordering::SeqCst) {
        terminal::deprep_for_input();
        // SAFETY: restores the application's signal dispositions.
        terminal::rl_clear_signals();
    }

    PUSHED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    MULTIKEY.store(0, Ordering::SeqCst);
    terminal::remove_state(terminal::RL_STATE_MULTIKEY);
    INTERRUPTED.store(false, Ordering::SeqCst);
    ABORT_LINE.store(false, Ordering::SeqCst);
}

/// Consume one available key and dispatch it.  Completing the line invokes the
/// installed handler and, unless the handler removed/reinstalled itself,
/// prints the next prompt.
#[no_mangle]
pub unsafe extern "C" fn rl_callback_read_char() {
    if LINE_FUNC
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_none()
    {
        // GNU aborts here rather than silently doing nothing.
        eprintln!("readline: rl_callback_read_char() called with no handler");
        libc::abort();
    }

    terminal::add_state(terminal::RL_STATE_CALLBACK);

    // Signal handlers are installed only while readline has control.
    // SAFETY: the saved dispositions are restored below.
    terminal::rl_set_signals();
    terminal::check_signals();

    let mut finished = None;

    if INTERRUPTED.swap(false, Ordering::SeqCst) {
        finished = Some(LineEnd::Aborted);
    } else if state::done() != 0 {
        finished = Some(if terminal::state_is_set(terminal::RL_STATE_EOF) {
            LineEnd::Eof
        } else {
            LineEnd::Accepted
        });
    } else {
        let saved_multikey = MULTIKEY.swap(0, Ordering::SeqCst);
        let map = if saved_multikey != 0 {
            terminal::remove_state(terminal::RL_STATE_MULTIKEY);
            saved_multikey as Keymap
        } else {
            current_keymap()
        };

        terminal::add_state(terminal::RL_STATE_READCMD);
        let key = read_key();
        terminal::remove_state(terminal::RL_STATE_READCMD);

        match key {
            None => finished = Some(LineEnd::Aborted),
            Some(key) => {
                let is_eof = key == libc::EOF;
                let is_ctrl_d = !is_eof && (key & 0xff) == 0x04;
                if (is_eof || is_ctrl_d) && state::end() == 0 {
                    finished = Some(LineEnd::Eof);
                } else if is_eof {
                    // GNU turns EOF on a non-empty line into the EOF
                    // character, which is normally delete-char.
                    process_key(0x04, map);
                } else {
                    process_key(key, map);
                }

                if finished.is_none()
                    && crate::display::num_chars_to_read() > 0
                    && state::end() >= crate::display::num_chars_to_read()
                {
                    // GNU stops collecting once rl_num_chars_to_read characters
                    // have been inserted.
                    state::set_done(1);
                    terminal::add_state(terminal::RL_STATE_DONE);
                }

                if finished.is_none() && state::done() != 0 {
                    finished = Some(if ABORT_LINE.swap(false, Ordering::SeqCst) {
                        LineEnd::Aborted
                    } else if terminal::state_is_set(terminal::RL_STATE_EOF) {
                        LineEnd::Eof
                    } else {
                        LineEnd::Accepted
                    });
                }
            }
        }
    }

    if let Some(kind) = finished {
        finish_line(kind);
    } else {
        crate::completion::redisplay_with_hook();
    }

    // SAFETY: returns the application's handlers after this call, matching
    // GNU's non-persistent callback signal policy.
    terminal::rl_clear_signals();
}

/// Drop transient callback state after a signal.  GNU clears search,
/// numeric-argument, vi-motion and multi-key contexts; only the multi-key
/// context exists in this slice.
#[no_mangle]
pub unsafe extern "C" fn rl_callback_sigcleanup() {
    if !terminal::state_is_set(terminal::RL_STATE_CALLBACK) {
        return;
    }
    MULTIKEY.store(0, Ordering::SeqCst);
    terminal::remove_state(terminal::RL_STATE_MULTIKEY);
}

/// Signal bridge used by `terminal::check_signals`: a caught interrupt aborts
/// the current line on the next callback step.
pub(crate) fn note_interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_subset_names_are_known_commands() {
        for (name, _) in crate::keymap_defaults::DEFAULT_FUNMAP {
            if is_safe_command(name) {
                assert!(crate::keymap_defaults::default_command(name).is_some());
            }
        }
    }

    #[test]
    fn default_command_name_recovers_the_funmap_spelling() {
        let pointer = crate::keymap_defaults::default_command("self-insert")
            .map(|function| function as *const () as *mut c_void)
            .unwrap();
        assert_eq!(default_command_name(pointer), Some("self-insert"));
    }
}
