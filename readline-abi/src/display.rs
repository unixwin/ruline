//! Redisplay helpers from the GNU readline ABI.
//!
//! The adapter keeps no visible/invisible screen-line structures: the line is
//! repainted from `rl_display_prompt` and `rl_line_buffer` on every
//! `rl_redisplay`.  The functions here implement the state and formatting
//! part of GNU's display slice on top of that model:
//!
//! * `rl_message` formats into an owned echo-area buffer, points
//!   `rl_display_prompt` at it, and repaints; `rl_clear_message` restores
//!   `rl_prompt`.
//! * `rl_show_char`/`rl_character_len` reproduce GNU's `M-`/`C-` rendering and
//!   column widths, including the `DISPLAY_TABS` build that reads tabs as
//!   eight-column stops.
//! * `rl_clear_screen`/`rl_clear_display` emit ANSI clear sequences because
//!   this adapter does not link termcap/terminfo.  `rl_refresh_line` uses the
//!   same clear-to-end-of-line sequence as `rl_clear_visible_line`.
//! * Screen-line movement delegates to `rl_backward_char`/`rl_forward_char`
//!   with the cached terminal width.  Autowrap is assumed, as it is for every
//!   terminal this adapter prepares.
//!
//! Dispatch-only pieces are explicit rather than hidden: `rl_arrow_keys` and
//! `rl_skip_csi_sequence` read through the callback input path
//! (`rl_pending_input`, `rl_getc_function`, or the standard input stream), and
//! history movement in `rl_arrow_keys` uses the implemented history list.  The
//! library does not maintain GNU's undo list or keyboard macros, so
//! `rl_undo_list`/`rl_executing_macro` are state storage here.
//! `rl_already_prompted` is honoured by the Reedline `readline()` path by
//! suppressing prompt output, since the application has already painted it.

use std::ffi::CStr;
use std::os::raw::c_void;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::callback;
use crate::completion;
use crate::editing;
use crate::history;
use crate::line;
use crate::state;
use crate::terminal;

/// GNU `DISPLAY_TABS`: a tab occupies up to the next eight-column stop.
const TAB_WIDTH: c_int = 8;
const RUBOUT: c_int = 0x7f;
const META_BIT: c_int = 0x80;
const CONTROL_LIMIT: c_int = 0x20;

// --- Display variables ----------------------------------------------------

#[no_mangle]
pub static mut rl_erase_empty_line: c_int = 0;
#[no_mangle]
pub static mut rl_already_prompted: c_int = 0;
#[no_mangle]
pub static mut rl_num_chars_to_read: c_int = 0;

/// `rl_erase_empty_line` is a plain C variable; the callback loop reads it
/// through this helper instead of taking a reference to the static.
pub(crate) fn erase_empty_line() -> c_int {
    unsafe { ptr::addr_of!(rl_erase_empty_line).read() }
}

/// `rl_num_chars_to_read` accessor for the callback input loop.
pub(crate) fn num_chars_to_read() -> c_int {
    unsafe { ptr::addr_of!(rl_num_chars_to_read).read() }
}

/// `rl_already_prompted` accessor for the `readline()` entry point.
pub(crate) fn already_prompted() -> c_int {
    unsafe { ptr::addr_of!(rl_already_prompted).read() }
}

/// GNU's `_rl_output_meta_chars`.  readline switches to direct eight-bit
/// output during initialization (nls.c); before that, meta bytes are shown as
/// `M-` sequences.
static OUTPUT_META_CHARS: AtomicBool = AtomicBool::new(false);

pub(crate) fn initialize_display_state() {
    OUTPUT_META_CHARS.store(true, Ordering::Relaxed);
}

fn output_meta_chars() -> bool {
    OUTPUT_META_CHARS.load(Ordering::Relaxed)
}

// --- Output ---------------------------------------------------------------

/// Write bytes to `rl_outstream` (stdout before initialization) and flush,
/// matching GNU's character-output functions.
pub(crate) fn write_output(bytes: &[u8]) {
    let stream = state::effective_outstream();
    if stream.is_null() || bytes.is_empty() {
        return;
    }
    // SAFETY: `stream` is the caller-owned FILE* supplied through the ABI.
    unsafe {
        libc::fwrite(bytes.as_ptr() as *const c_void, 1, bytes.len(), stream);
        libc::fflush(stream);
    }
}

// --- Character rendering --------------------------------------------------

/// GNU `rl_show_char`'s byte expansion.  The returned length is also the
/// value GNU returns (each prefix byte and the final character count one).
fn rendered_char_with(c: c_int, output_meta: bool) -> Vec<u8> {
    let mut output = Vec::with_capacity(7);
    let mut value = c;

    if (c as u8) > RUBOUT as u8 && !output_meta {
        output.extend_from_slice(b"M-");
        value = c_int::from((c as u8) & !(META_BIT as u8));
    }

    let byte = value as u8;
    if byte == b'\t' {
        output.push(byte);
    } else if value == RUBOUT {
        output.extend_from_slice(b"C-?");
    } else if value < CONTROL_LIMIT && (value & META_BIT) == 0 {
        output.extend_from_slice(b"C-");
        output.push(byte.to_ascii_uppercase() | 0x40);
    } else {
        output.push(byte);
    }

    output
}

fn rendered_char(c: c_int) -> Vec<u8> {
    rendered_char_with(c, output_meta_chars())
}

/// GNU `rl_character_len` for the single-byte, `DISPLAY_TABS` build.
fn character_len_with(c: c_int, pos: c_int, output_meta: bool) -> c_int {
    let byte = c as u8;
    if byte > RUBOUT as u8 {
        return if output_meta { 1 } else { 4 };
    }
    if byte == b'\t' {
        return ((pos | (TAB_WIDTH - 1)) + 1) - pos;
    }
    if (c < CONTROL_LIMIT && (c & META_BIT) == 0) || c == RUBOUT {
        return 2;
    }
    if unsafe { libc::isprint(c_int::from(byte)) } != 0 {
        1
    } else {
        2
    }
}

fn character_len(c: c_int, pos: c_int) -> c_int {
    character_len_with(c, pos, output_meta_chars())
}

// --- Echo area ------------------------------------------------------------

static mut MESSAGE_BUFFER: *mut c_char = ptr::null_mut();
static mut MESSAGE_CAPACITY: usize = 0;

fn message_buffer() -> *mut c_char {
    // SAFETY: plain read of this module's message storage.
    unsafe { ptr::addr_of!(MESSAGE_BUFFER).read() }
}

fn ensure_message_capacity(required: usize) -> bool {
    let current = message_buffer();
    if !current.is_null() && required <= unsafe { ptr::addr_of!(MESSAGE_CAPACITY).read() } {
        return true;
    }

    let capacity = required.max(128);
    // SAFETY: `current` is either NULL or a buffer allocated below with the C
    // allocator; `realloc` preserves its contents.
    let resized = unsafe { libc::realloc(current as *mut c_void, capacity) as *mut c_char };
    if resized.is_null() {
        return false;
    }
    unsafe {
        ptr::addr_of_mut!(MESSAGE_BUFFER).write(resized);
        ptr::addr_of_mut!(MESSAGE_CAPACITY).write(capacity);
    }
    true
}

extern "C" {
    fn vsnprintf(
        buffer: *mut c_char,
        size: usize,
        format: *const c_char,
        arguments: std::ffi::VaList<'_>,
    ) -> c_int;
}

/// Format a message into the echo area and repaint.
///
/// GNU saves the local prompt before installing the message; this adapter's
/// display prompt is just a pointer, so `rl_clear_message` can restore it
/// without a second saved copy.
#[no_mangle]
pub unsafe extern "C" fn rl_message(format: *const c_char, arguments: ...) -> c_int {
    if format.is_null() {
        return -1;
    }

    let needed = vsnprintf(ptr::null_mut(), 0, format, arguments.clone());
    if needed < 0 {
        return -1;
    }
    if !ensure_message_capacity(needed as usize + 1) {
        return -1;
    }

    let buffer = message_buffer();
    let written = vsnprintf(
        buffer,
        unsafe { ptr::addr_of!(MESSAGE_CAPACITY).read() },
        format,
        arguments,
    );
    if written < 0 {
        return -1;
    }

    state::set_display_prompt(buffer);
    completion::redisplay_with_hook();
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_show_char(c: c_int) -> c_int {
    let rendered = rendered_char(c);
    write_output(&rendered);
    rendered.len() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn rl_character_len(c: c_int, pos: c_int) -> c_int {
    character_len(c, pos)
}

// --- Prompt and line repainting ------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rl_on_new_line_with_prompt() -> c_int {
    let prompt = state::prompt();
    state::set_display_prompt(if prompt.is_null() {
        state::empty_string()
    } else {
        prompt
    });
    0
}

/// Redraw only the final line of a multi-line display prompt.
#[no_mangle]
pub unsafe extern "C" fn rl_redraw_prompt_last_line() {
    let prompt = state::display_prompt();
    if prompt.is_null() {
        line::rl_forced_update_display();
        return;
    }

    let bytes = CStr::from_ptr(prompt).to_bytes();
    match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(index) => {
            let last_line = prompt.add(index + 1);
            state::set_display_prompt(last_line);
            line::rl_forced_update_display();
            state::set_display_prompt(prompt);
        }
        None => {
            line::rl_forced_update_display();
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rl_refresh_line(_count: c_int, _key: c_int) -> c_int {
    line::rl_clear_visible_line();
    rl_redraw_prompt_last_line();
    editing::rl_keep_mark_active();
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_clear_screen(count: c_int, key: c_int) -> c_int {
    if state::explicit_arg() != 0 {
        return rl_refresh_line(count, key);
    }

    // ANSI `clear`: home the cursor and erase the screen.
    write_output(b"\x1b[H\x1b[2J");
    editing::rl_keep_mark_active();
    line::rl_forced_update_display();
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_clear_display(_count: c_int, _key: c_int) -> c_int {
    // ANSI `clear` plus xterm's erase-saved-lines (`E3`) for the scrollback.
    write_output(b"\x1b[H\x1b[2J\x1b[3J");
    line::rl_forced_update_display();
    0
}

/// Legacy alias exported by GNU's `compat.c`.
#[no_mangle]
pub unsafe extern "C" fn crlf() -> c_int {
    line::rl_crlf()
}

// --- Screen-line commands -------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rl_previous_screen_line(_count: c_int, key: c_int) -> c_int {
    editing::backward_char(terminal::screen_width(), key)
}

#[no_mangle]
pub unsafe extern "C" fn rl_next_screen_line(_count: c_int, key: c_int) -> c_int {
    editing::forward_char(terminal::screen_width(), key)
}

// --- Escape-sequence and arrow dispatch ----------------------------------

#[no_mangle]
pub unsafe extern "C" fn rl_skip_csi_sequence(_count: c_int, _key: c_int) -> c_int {
    terminal::add_state(terminal::RL_STATE_MOREINPUT);
    let ch = loop {
        match callback::read_key() {
            Some(value) if (CONTROL_LIMIT..0x40).contains(&value) => continue,
            Some(value) => break value,
            None => break -1,
        }
    };
    terminal::remove_state(terminal::RL_STATE_MOREINPUT);

    c_int::from(ch < 0)
}

fn navigate_history(backward: bool) -> bool {
    // SAFETY: the history list owns the returned entry for the duration of
    // the call; the line is copied immediately.
    let entry = unsafe {
        if backward {
            history::previous_history()
        } else {
            history::next_history()
        }
    };
    if entry.is_null() {
        return false;
    }
    let line = unsafe { (*entry).line };
    if line.is_null() {
        return false;
    }
    let bytes = unsafe { CStr::from_ptr(line) }.to_bytes();
    line::set_line_bytes(bytes);
    true
}

#[no_mangle]
pub unsafe extern "C" fn rl_arrow_keys(count: c_int, key: c_int) -> c_int {
    terminal::add_state(terminal::RL_STATE_MOREINPUT);
    let ch = callback::read_key().unwrap_or(-1);
    terminal::remove_state(terminal::RL_STATE_MOREINPUT);

    if ch < 0 {
        return 1;
    }

    match (ch as u8).to_ascii_uppercase() {
        b'A' => {
            if !navigate_history(true) {
                line::rl_ding();
            }
        }
        b'B' => {
            if !navigate_history(false) {
                line::rl_ding();
            }
        }
        b'C' => {
            editing::forward_char(count, ch);
        }
        b'D' => {
            editing::backward_char(count, ch);
        }
        _ => {
            line::rl_ding();
            let _ = key;
        }
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_and_control_rendering_matches_gnu() {
        assert_eq!(rendered_char_with(c_int::from(b'a'), false), b"a");
        assert_eq!(rendered_char_with(0x01, false), b"C-A");
        assert_eq!(rendered_char_with(RUBOUT, false), b"C-?");
        assert_eq!(rendered_char_with(0x80, false), b"M-C-@");
        assert_eq!(rendered_char_with(0x89, false), b"M-\t");
        assert_eq!(rendered_char_with(0x80, true), [0x80]);
    }

    #[test]
    fn character_width_uses_tab_stops_and_control_prefixes() {
        assert_eq!(character_len_with(c_int::from(b'a'), 0, false), 1);
        assert_eq!(character_len_with(0x01, 0, false), 2);
        assert_eq!(character_len_with(RUBOUT, 0, false), 2);
        assert_eq!(character_len_with(0x80, 0, false), 4);
        assert_eq!(character_len_with(0x80, 0, true), 1);
        assert_eq!(character_len_with(b'\t' as c_int, 0, false), 8);
        assert_eq!(character_len_with(b'\t' as c_int, 3, false), 5);
        assert_eq!(character_len_with(b'\t' as c_int, 8, false), 8);
    }
}
