//! Line-buffer, prompt, and display functions from the readline ABI.

use std::ffi::CStr;
use std::io::Write;
use std::os::raw::{c_char, c_int};
use std::ptr;

use crate::state;

pub(crate) const RL_STATE_INITIALIZING: std::os::raw::c_ulong = 0x0000_0001;
pub(crate) const RL_STATE_INITIALIZED: std::os::raw::c_ulong = 0x0000_0002;
pub(crate) const RL_STATE_COMPLETING: std::os::raw::c_ulong = 0x0000_4000;
pub(crate) const RL_STATE_DONE: std::os::raw::c_ulong = 0x0200_0000;
pub(crate) const RL_STATE_EOF: std::os::raw::c_ulong = 0x0800_0000;

fn c_bytes<'a>(value: *const c_char) -> &'a [u8] {
    if value.is_null() {
        &[]
    } else {
        // SAFETY: the ABI requires NUL-terminated strings for the lifetime of
        // the call.
        unsafe { CStr::from_ptr(value).to_bytes() }
    }
}

fn duplicate(value: *const c_char) -> *mut c_char {
    if value.is_null() {
        ptr::null_mut()
    } else {
        // SAFETY: value is a NUL-terminated string supplied by the caller.
        unsafe { libc::strdup(value) }
    }
}

fn free_c_string(value: *mut c_char) {
    if !value.is_null() {
        // SAFETY: prompt strings are allocated by this library with malloc.
        unsafe { libc::free(value as *mut libc::c_void) };
    }
}

/// Number of visible columns in the final prompt line, ignoring the
/// `RL_PROMPT_START_IGNORE`/`RL_PROMPT_END_IGNORE`-marked span.
fn visible_prompt_len(bytes: &[u8]) -> usize {
    let mut visible = 0;
    let mut ignore = false;
    for &byte in bytes {
        match byte {
            0x01 => ignore = true,
            0x02 => ignore = false,
            b'\n' => visible = 0,
            _ if !ignore => visible += 1,
            _ => {}
        }
    }
    visible
}

fn prompt_for_display(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .copied()
        .filter(|byte| *byte != 0x01 && *byte != 0x02)
        .collect()
}

fn write_stdout(bytes: &[u8]) {
    crate::display::write_output(bytes);
}

fn write_stderr(bytes: &[u8]) {
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = lock.write_all(bytes);
    let _ = lock.flush();
}

pub(crate) fn initialize_line_state() {
    state::initialize_streams();
    state::initialize_terminal_name();
    crate::display::initialize_display_state();
    state::reset_line_state();
    crate::vi::initialize_line();
    state::reset_argument();
    state::set_mark(0);
    state::set_done(0);
    state::set_eof_found(0);
    state::set_dispatching(0);
    state::set_insert_mode(1);
    state::set_last_func(None);
    state::set_key_sequence_length(0);
    state::set_readline_state(
        state::readline_state()
            & !(crate::editing::RL_STATE_OVERWRITE_BIT | crate::editing::RL_STATE_NUMERICARG_BIT),
    );
    if state::display_prompt().is_null() {
        state::set_display_prompt(match state::prompt() {
            prompt if !prompt.is_null() => prompt,
            _ => state::empty_string(),
        });
    }
}

pub(crate) fn set_prompt(prompt: *const c_char) {
    let old = state::prompt();
    free_c_string(old);

    let new_prompt = duplicate(prompt);
    state::set_prompt(new_prompt);
    state::set_display_prompt(if new_prompt.is_null() {
        state::empty_string()
    } else {
        new_prompt
    });
}

pub(crate) fn set_line_bytes(bytes: &[u8]) {
    state::set_line_bytes(bytes);
    state::set_mark(0);
}

pub(crate) fn initialize_state_bits() {
    let current = state::readline_state();
    state::set_readline_state((current | RL_STATE_INITIALIZED) & !RL_STATE_INITIALIZING);
}

#[no_mangle]
pub unsafe extern "C" fn rl_initialize() -> c_int {
    crate::keymap::initialize();
    let previous = state::readline_state();
    state::set_readline_state(previous | RL_STATE_INITIALIZING);

    initialize_line_state();
    initialize_state_bits();
    crate::completion::initialize_defaults();

    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_set_prompt(prompt: *const c_char) -> c_int {
    set_prompt(prompt);
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_expand_prompt(prompt: *mut c_char) -> c_int {
    visible_prompt_len(c_bytes(prompt)) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn rl_replace_line(text: *const c_char, clear_undo: c_int) {
    if clear_undo != 0 {
        crate::undo::rl_free_undo_list();
    }

    let bytes = c_bytes(text);
    state::ensure_line_capacity(bytes.len() + 1);
    let buffer = state::line_buffer();
    if buffer.is_null() {
        return;
    }

    ptr::copy_nonoverlapping(bytes.as_ptr(), state::char_ptr_as_u8(buffer), bytes.len());
    *buffer.add(bytes.len()) = 0;

    let len = bytes.len() as c_int;
    state::set_end(len);
    state::clamp_point();
    state::clamp_mark();
}

#[no_mangle]
pub unsafe extern "C" fn rl_insert_text(text: *const c_char) -> c_int {
    let bytes = c_bytes(text);
    if bytes.is_empty() {
        return 0;
    }

    let end = state::end().max(0);
    let point = state::clamp_position(state::point());
    state::ensure_line_capacity(end as usize + bytes.len() + 1);

    let buffer = state::line_buffer();
    if buffer.is_null() {
        return 0;
    }

    if !crate::vi::undo_in_progress() {
        crate::vi::record_insert(point, point + bytes.len() as c_int);
    }

    let tail_len = (end - point) as usize + 1; // include the trailing NUL
    ptr::copy(
        buffer.add(point as usize),
        buffer.add(point as usize + bytes.len()),
        tail_len,
    );
    ptr::copy_nonoverlapping(
        bytes.as_ptr(),
        state::char_ptr_as_u8(buffer.add(point as usize)),
        bytes.len(),
    );

    if !crate::undo::is_undoing() {
        crate::undo::record_insert(point, bytes.len() as c_int);
    }

    state::set_point(point + bytes.len() as c_int);
    state::set_end(end + bytes.len() as c_int);
    bytes.len() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn rl_delete_text(from: c_int, to: c_int) -> c_int {
    let mut start = from;
    let mut stop = to;
    if start > stop {
        std::mem::swap(&mut start, &mut stop);
    }

    let end = state::end().max(0);
    if stop > end {
        stop = end;
        if start > stop {
            start = stop;
        }
    }
    if start < 0 {
        start = 0;
    }

    let removed = stop - start;
    if removed <= 0 {
        return 0;
    }

    if !crate::undo::is_undoing() {
        crate::undo::record_delete(start, stop);
    }

    state::ensure_line_capacity(end as usize + 1);
    let buffer = state::line_buffer();
    if buffer.is_null() {
        return 0;
    }

    if !crate::vi::undo_in_progress() {
        // SAFETY: `start` and `stop` are inside the line buffer and the range
        // is non-empty after the boundary fixes above.
        let text = unsafe {
            std::slice::from_raw_parts(buffer.add(start as usize) as *const u8, removed as usize)
        }
        .to_vec();
        crate::vi::record_delete(start, stop, text);
    }

    let remaining = (end - stop) as usize;
    ptr::copy(
        buffer.add(stop as usize),
        buffer.add(start as usize),
        remaining,
    );
    let new_end = end - removed;
    *buffer.add(new_end as usize) = 0;
    state::set_end(new_end);
    state::clamp_mark();

    removed
}

#[no_mangle]
pub unsafe extern "C" fn rl_copy_text(from: c_int, to: c_int) -> *mut c_char {
    let mut start = from;
    let mut stop = to;
    if start > stop {
        std::mem::swap(&mut start, &mut stop);
    }

    let end = state::end().max(0);
    start = start.clamp(0, end);
    stop = stop.clamp(0, end);

    let length = (stop - start) as usize;
    let copy = libc::malloc(length + 1) as *mut c_char;
    if copy.is_null() {
        return ptr::null_mut();
    }

    let buffer = state::line_buffer();
    if length > 0 && !buffer.is_null() {
        ptr::copy_nonoverlapping(buffer.add(start as usize), copy, length);
    }
    *copy.add(length) = 0;
    copy
}

#[no_mangle]
pub unsafe extern "C" fn rl_kill_text(from: c_int, to: c_int) -> c_int {
    // GNU saves the killed text on the kill ring and reports success, not the
    // number of removed bytes (see kill.c:rl_kill_text).
    crate::editing::kill_text(from, to);
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_extend_line_buffer(len: c_int) {
    state::ensure_line_capacity(len.max(0) as usize + 1);
}

#[no_mangle]
pub unsafe extern "C" fn rl_ding() -> c_int {
    // GNU writes the audible bell to stderr and reports -1 when echoing is
    // disabled (terminal.c:rl_ding).
    if state::echoing() == 0 {
        return -1;
    }
    write_stderr(b"\x07");
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_crlf() -> c_int {
    write_stdout(b"\n");
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_on_new_line() -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_redisplay() {
    let prompt_ptr = state::display_prompt();
    let prompt = if prompt_ptr.is_null() {
        c_bytes(state::prompt())
    } else {
        c_bytes(prompt_ptr)
    };
    let rendered_prompt = prompt_for_display(prompt);

    let buffer = state::line_buffer();
    let end = state::end().max(0) as usize;
    let line = if buffer.is_null() {
        &[][..]
    } else {
        std::slice::from_raw_parts(buffer as *const u8, end)
    };

    let mut output = Vec::with_capacity(rendered_prompt.len() + line.len() + 8);
    output.push(b'\r');
    output.extend_from_slice(&rendered_prompt);
    output.extend_from_slice(line);

    let behind = end.saturating_sub(state::point().max(0) as usize);
    if behind > 0 {
        output.extend_from_slice(format!("\x1b[{behind}D").as_bytes());
    }
    write_stdout(&output);
}

#[no_mangle]
pub unsafe extern "C" fn rl_forced_update_display() -> c_int {
    rl_on_new_line();
    // GNU routes forced repaints through rl_redisplay_function when the
    // application installed one, falling back to rl_redisplay.
    crate::completion::redisplay_with_hook();
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_clear_message() -> c_int {
    let prompt = state::prompt();
    state::set_display_prompt(if prompt.is_null() {
        state::empty_string()
    } else {
        prompt
    });
    crate::completion::redisplay_with_hook();
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_clear_visible_line() -> c_int {
    write_stdout(b"\r\x1b[2K");
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_free_line_state() {
    // GNU drops the undo list, any partial history entry, keyboard macros, and
    // a pending numeric argument before the caller cleans up the terminal.
    crate::undo::rl_free_undo_list();
    crate::history::clear_current_history_data();
    crate::macros::kill_kbd_macro();
    rl_clear_message();
    state::reset_argument();
}

#[no_mangle]
pub unsafe extern "C" fn rl_reset_line_state() -> c_int {
    rl_on_new_line();
    let prompt = state::prompt();
    state::set_display_prompt(if prompt.is_null() {
        state::empty_string()
    } else {
        prompt
    });
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_save_prompt() {
    free_c_string(state::SAVED_PROMPT);
    free_c_string(state::SAVED_DISPLAY_PROMPT);

    state::SAVED_PROMPT = duplicate(state::prompt());
    state::SAVED_DISPLAY_PROMPT = duplicate(state::display_prompt());
    state::PROMPT_IS_SAVED = true;
}

#[no_mangle]
pub unsafe extern "C" fn rl_restore_prompt() {
    if !state::PROMPT_IS_SAVED {
        return;
    }

    free_c_string(state::prompt());
    state::set_prompt(state::SAVED_PROMPT);
    state::set_display_prompt(if state::SAVED_DISPLAY_PROMPT.is_null() {
        state::empty_string()
    } else {
        state::SAVED_DISPLAY_PROMPT
    });

    state::SAVED_PROMPT = ptr::null_mut();
    state::SAVED_DISPLAY_PROMPT = ptr::null_mut();
    state::PROMPT_IS_SAVED = false;
}

#[no_mangle]
pub unsafe extern "C" fn rl_set_keyboard_input_timeout(timeout: c_int) -> c_int {
    let previous = state::keyboard_timeout();
    state::set_keyboard_timeout(timeout);
    previous
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_prompt_length_uses_last_line_and_skips_ignored_spans() {
        assert_eq!(visible_prompt_len(b"$ "), 2);
        assert_eq!(visible_prompt_len(b"one\ntwo"), 3);
        assert_eq!(visible_prompt_len(b"\x01\x1b[31m\x02$ "), 2);
        assert_eq!(visible_prompt_len(b""), 0);
    }

    #[test]
    fn display_prompt_drops_only_ignore_markers() {
        assert_eq!(
            prompt_for_display(b"\x01\x1b[31m\x02$ "),
            b"\x1b[31m$ ".to_vec()
        );
    }
}
