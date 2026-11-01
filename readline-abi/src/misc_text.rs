//! Remaining non-vi text commands that do not need the terminal loop.
//!
//! * `rl_insert_comment` and `rl_insert_close` edit the current line.
//! * `rl_tilde_expand` performs readline-style `~` expansion on the word at
//!   point (`~`, `~/...`, and `~user/...`).
//! * `rl_tty_status` reports terminal status; GNU only implements the BSD
//!   `TIOCSTAT` ioctl, so on Linux it rings the bell exactly like the oracle.
//! * `rl_restart_output` / `rl_stop_output` drive `tcflow` on the standard
//!   streams, matching rltty.c's POSIX path.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

use crate::state;

const COMMENT_BEGIN_DEFAULT: &[u8] = b"#";

fn line_bytes() -> Vec<u8> {
    let buffer = state::line_buffer();
    let end = state::end().max(0) as usize;
    if buffer.is_null() {
        Vec::new()
    } else {
        // SAFETY: the buffer holds at least `end` bytes followed by a NUL.
        unsafe { std::slice::from_raw_parts(buffer as *const u8, end) }.to_vec()
    }
}

fn whitespace(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

fn insert_bytes(bytes: &[u8]) -> c_int {
    if bytes.is_empty() {
        return 0;
    }
    let mut terminated = bytes.to_vec();
    terminated.push(0);
    // SAFETY: `terminated` is NUL-terminated for the duration of the call.
    unsafe { crate::line::rl_insert_text(terminated.as_ptr() as *const c_char) }
}

/// GNU `_rl_replace_text`: undoable replace of `start..=end`.
fn replace_text(bytes: &[u8], start: c_int, end: c_int) {
    // SAFETY: the undo group and buffer helpers clamp their own inputs.
    unsafe {
        crate::undo::rl_begin_undo_group();
        crate::line::rl_delete_text(start, end + 1);
        state::set_point(start);
        insert_bytes(bytes);
        crate::undo::rl_end_undo_group();
    }
}

// --- Tilde expansion ------------------------------------------------------

fn home_directory() -> Vec<u8> {
    if let Some(home) = std::env::var_os("HOME") {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            return home.into_vec();
        }
        #[cfg(not(unix))]
        return home.to_string_lossy().into_owned().into_bytes();
    }

    // SAFETY: getpwuid returns a process-owned passwd record or NULL.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_dir.is_null() {
            Vec::new()
        } else {
            CStr::from_ptr((*entry).pw_dir).to_bytes().to_vec()
        }
    }
}

fn user_home(name: &[u8]) -> Option<Vec<u8>> {
    let name = CString::new(name).ok()?;
    // SAFETY: getpwnam returns a process-owned passwd record or NULL.  The
    // bytes are copied before another lookup can overwrite the record.
    unsafe {
        let entry = libc::getpwnam(name.as_ptr());
        if entry.is_null() || (*entry).pw_dir.is_null() {
            None
        } else {
            Some(CStr::from_ptr((*entry).pw_dir).to_bytes().to_vec())
        }
    }
}

/// Port of `tilde_expand_word` for the cases the line editor can encounter:
/// a bare `~`, `~/...`, and `~user/...`.  Unknown users are left unchanged,
/// as GNU does when no expansion hook is installed.
fn tilde_expand_word(word: &[u8]) -> Vec<u8> {
    if word.is_empty() || word[0] != b'~' {
        return word.to_vec();
    }

    if word.len() == 1 || word[1] == b'/' {
        let mut expanded = home_directory();
        expanded.extend_from_slice(&word[1..]);
        return expanded;
    }

    let user_len = word
        .iter()
        .position(|byte| *byte == b'/')
        .unwrap_or(word.len());
    let username = &word[1..user_len];
    match user_home(username) {
        Some(mut home) => {
            home.extend_from_slice(&word[user_len..]);
            home
        }
        None => word.to_vec(),
    }
}

/// Expand the tilde word at point (`rl_tilde_expand` in util.c).
#[no_mangle]
pub unsafe extern "C" fn rl_tilde_expand(_ignore: c_int, _key: c_int) -> c_int {
    let bytes = line_bytes();
    let point = state::point();
    let end_limit = state::end();

    let mut end = point;
    let mut start = end - 1;

    // GNU's first branch: a tilde at the (NUL) terminator position.
    if point == end_limit && bytes.get(point as usize).copied() == Some(b'~') {
        let expanded = tilde_expand_word(b"~");
        replace_text(&expanded, start, end);
        return 0;
    }

    if start >= 0 && bytes.get(start as usize).copied() != Some(b'~') {
        while start >= 0 && !whitespace(bytes[start as usize]) {
            start -= 1;
        }
        start += 1;
    } else if start < 0 {
        start = 0;
    }

    end = start;
    loop {
        end += 1;
        if end >= end_limit || whitespace(bytes[end as usize]) {
            break;
        }
    }
    if end >= end_limit || whitespace(bytes[end as usize]) {
        end -= 1;
    }

    if bytes.get(start as usize).copied() == Some(b'~') {
        let word = &bytes[start as usize..=end as usize];
        let expanded = tilde_expand_word(word);
        replace_text(&expanded, start, end);
    }
    0
}

// --- Comment insertion ----------------------------------------------------

/// Turn the current line into a comment and accept it.
#[no_mangle]
pub unsafe extern "C" fn rl_insert_comment(_count: c_int, key: c_int) -> c_int {
    // Capture the argument before calling another command: this library's
    // command wrappers reset `rl_explicit_arg` at the end of each call, while
    // GNU's dispatcher resets it between commands.
    let explicit = state::explicit_arg() != 0;

    crate::editing::rl_beg_of_line(1, key);

    let comment = crate::keymap::variable_value_bytes("comment-begin")
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| COMMENT_BEGIN_DEFAULT.to_vec());

    if !explicit {
        insert_bytes(&comment);
    } else {
        let line = line_bytes();
        if line.starts_with(&comment) {
            crate::line::rl_delete_text(state::point(), state::point() + comment.len() as c_int);
        } else {
            insert_bytes(&comment);
        }
    }

    crate::completion::redisplay_with_hook();
    crate::editing::rl_newline(1, b'\n' as c_int);
    0
}

// --- Paren matching -------------------------------------------------------

fn matching_open(bytes: &[u8], from: c_int, closer: u8) -> c_int {
    let opener = match closer {
        b']' => b'[',
        b'}' => b'{',
        b')' => b'(',
        _ => return -1,
    };

    let quote_characters = unsafe {
        let pointer = crate::completion::rl_basic_quote_characters;
        if pointer.is_null() {
            Vec::new()
        } else {
            CStr::from_ptr(pointer as *const c_char).to_bytes().to_vec()
        }
    };

    let mut level = 1;
    let mut delimiter = 0u8;
    let mut index = from;
    while index >= 0 {
        let character = bytes[index as usize];
        if delimiter != 0 && character == delimiter {
            delimiter = 0;
        } else if quote_characters.contains(&character) {
            delimiter = character;
        } else if delimiter == 0 && character == closer {
            level += 1;
        } else if delimiter == 0 && character == opener {
            level -= 1;
        }
        if level == 0 {
            return index;
        }
        index -= 1;
    }
    -1
}

/// Insert a closing paren, optionally checking for its match.
///
/// The timed blink GNU performs between highlighting the opener and restoring
/// point needs a terminal select; this implementation locates the match and
/// restores point immediately.  The observable line edit is identical.
#[no_mangle]
pub unsafe extern "C" fn rl_insert_close(count: c_int, invoking_key: c_int) -> c_int {
    if state::explicit_arg() != 0 || !crate::keymap::variable_boolean("blink-matching-paren") {
        return crate::editing::rl_insert(count, invoking_key);
    }

    crate::editing::rl_insert(1, invoking_key);
    crate::completion::redisplay_with_hook();

    let bytes = line_bytes();
    let match_point = matching_open(&bytes, state::point() - 2, invoking_key as u8);
    if match_point < 0 {
        return 1;
    }
    0
}

// --- Terminal status and flow control ------------------------------------

/// GNU only implements `rl_tty_status` with the BSD `TIOCSTAT` ioctl; on
/// Linux the body is `rl_ding ()`.  Match that instead of inventing a status
/// read.
#[no_mangle]
pub unsafe extern "C" fn rl_tty_status(_count: c_int, _key: c_int) -> c_int {
    crate::line::rl_ding();
    0
}

/// Resume terminal output (`^Q`): GNU's rltty.c POSIX path is
/// `tcflow (fileno (rl_outstream), TCOON)`.
#[no_mangle]
pub unsafe extern "C" fn rl_restart_output(_count: c_int, _key: c_int) -> c_int {
    // SAFETY: tcflow is a plain termios call; errors are ignored like GNU.
    unsafe { libc::tcflow(libc::STDOUT_FILENO, libc::TCOON) };
    0
}

/// Suspend terminal output (`^S`): `tcflow (fileno (rl_instream), TCOOFF)`.
#[no_mangle]
pub unsafe extern "C" fn rl_stop_output(_count: c_int, _key: c_int) -> c_int {
    // SAFETY: tcflow is a plain termios call; errors are ignored like GNU.
    unsafe { libc::tcflow(libc::STDIN_FILENO, libc::TCOOFF) };
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion_matches_readline_cases() {
        assert_eq!(tilde_expand_word(b"~"), home_directory());
        let mut expected = home_directory();
        expected.extend_from_slice(b"/src");
        assert_eq!(tilde_expand_word(b"~/src"), expected);
        // Unknown users are returned unchanged.
        assert_eq!(
            tilde_expand_word(b"~this-user-should-not-exist/x"),
            b"~this-user-should-not-exist/x".to_vec()
        );
        assert_eq!(tilde_expand_word(b"plain"), b"plain".to_vec());
    }

    #[test]
    fn matching_open_skips_quoted_parens() {
        let line = b"(a (b) c)";
        assert_eq!(matching_open(line, 7, b')'), 0);
    }
}
