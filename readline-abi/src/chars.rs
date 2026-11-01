//! Character search (`rl_char_search`, `rl_backward_char_search`).
//!
//! GNU reads the target character from the input stream with
//! `_rl_bracketed_read_key`, then scans the line in the requested direction.
//! The scan core here is complete and multibyte-aware; the key read goes
//! through [`crate::macros::read_key`], which serves `rl_pending_input` and
//! macro input and returns -1 when this standalone library has no terminal
//! loop to read from.  That is the documented dispatcher seam.
//!
//! Direction and failure semantics follow text.c:
//! * a positive `count` searches forward, a negative one backward;
//! * the point lands on the matched character (not past it);
//! * running off either end rings the bell, returns 1, and leaves the point
//!   unchanged.

use std::os::raw::c_int;

use crate::macros::read_key;
use crate::state;

/// GNU `rldefs.h` search directions.
const FFIND: c_int = 2;
const BFIND: c_int = -2;

fn character_len(bytes: &[u8], position: usize) -> usize {
    let Some(&first) = bytes.get(position) else {
        return 1;
    };
    let length = match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    if position + length <= bytes.len()
        && std::str::from_utf8(&bytes[position..position + length]).is_ok()
    {
        length
    } else {
        1
    }
}

fn next_position(bytes: &[u8], position: usize) -> usize {
    if position >= bytes.len() {
        position
    } else {
        position + character_len(bytes, position)
    }
}

fn previous_position(bytes: &[u8], position: usize) -> usize {
    if position == 0 {
        return 0;
    }
    let lower = position.saturating_sub(4);
    let mut start = position - 1;
    while start > lower && (bytes[start] & 0xc0) == 0x80 {
        start -= 1;
    }
    if position - start == character_len(bytes, start) {
        start
    } else {
        position - 1
    }
}

fn matches_at(bytes: &[u8], position: usize, target: &[u8]) -> bool {
    !target.is_empty()
        && position + target.len() <= bytes.len()
        && &bytes[position..position + target.len()] == target
}

/// Scan `bytes` from `point` in `direction`, moving `count` matches.
///
/// Returns the new point or `None` when the search fails (the caller rings the
/// bell, exactly like GNU's `_rl_char_search_internal`).
fn char_search_internal(
    bytes: &[u8],
    point: c_int,
    count: c_int,
    direction: c_int,
    target: &[u8],
) -> Option<c_int> {
    if direction == 0 || target.is_empty() {
        return None;
    }

    let end = bytes.len();
    let mut position = point.clamp(0, end as c_int) as usize;
    let mut remaining = count;

    while remaining != 0 {
        if (direction < 0 && position == 0) || (direction > 0 && position >= end) {
            return None;
        }

        position = if direction > 0 {
            next_position(bytes, position)
        } else {
            previous_position(bytes, position)
        };

        loop {
            if matches_at(bytes, position, target) {
                remaining -= 1;
                break;
            }
            let previous = position;
            position = if direction < 0 {
                previous_position(bytes, position)
            } else {
                next_position(bytes, position)
            };
            if position == previous {
                break;
            }
        }
    }

    Some(position as c_int)
}

fn line_bytes() -> &'static [u8] {
    let buffer = state::line_buffer();
    let end = state::end().max(0) as usize;
    if buffer.is_null() {
        &[]
    } else {
        // SAFETY: the line buffer holds `end` bytes followed by a NUL.
        unsafe { std::slice::from_raw_parts(buffer as *const u8, end) }
    }
}

/// Read one (possibly multibyte) target character from the input source.
fn read_search_character() -> Option<Vec<u8>> {
    let first = read_key();
    if first < 0 {
        return None;
    }

    let mut bytes = vec![first as u8];
    // The first byte alone is not a valid UTF-8 sequence yet, so length comes
    // from the lead byte rather than `character_len`'s validation.
    let expected = match bytes[0] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    while bytes.len() < expected {
        let next = read_key();
        if next < 0 {
            break;
        }
        bytes.push(next as u8);
    }
    Some(bytes)
}

fn search(count: c_int, forward: c_int, backward: c_int) -> c_int {
    let Some(target) = read_search_character() else {
        return 1;
    };

    let bytes = line_bytes();
    let (magnitude, direction) = if count < 0 {
        (-count, backward)
    } else {
        (count, forward)
    };

    match char_search_internal(bytes, state::point(), magnitude, direction, &target) {
        Some(position) => {
            state::set_point(position);
            0
        }
        None => {
            // SAFETY: the bell writer only touches the output stream.
            unsafe { crate::line::rl_ding() };
            1
        }
    }
}

/// Search forward for a character read from the input source.
#[no_mangle]
pub unsafe extern "C" fn rl_char_search(count: c_int, _key: c_int) -> c_int {
    search(count, FFIND, BFIND)
}

/// Search backward for a character read from the input source.
#[no_mangle]
pub unsafe extern "C" fn rl_backward_char_search(count: c_int, _key: c_int) -> c_int {
    search(count, BFIND, FFIND)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_and_backward_land_on_the_match() {
        let line = b"a-b-c-d";
        assert_eq!(char_search_internal(line, 0, 2, FFIND, b"-"), Some(3));
        assert_eq!(char_search_internal(line, 6, 1, BFIND, b"-"), Some(5));
        assert_eq!(char_search_internal(line, 0, 0, FFIND, b"-"), Some(0));
    }

    #[test]
    fn running_off_the_end_fails() {
        let line = b"abc";
        assert_eq!(char_search_internal(line, 0, 1, FFIND, b"z"), None);
        assert_eq!(char_search_internal(line, 0, 1, BFIND, b"a"), None);
        assert_eq!(char_search_internal(line, 3, 1, FFIND, b"a"), None);
    }

    #[test]
    fn multibyte_target_matches_whole_character() {
        let line = "aé-bé".as_bytes();
        assert_eq!(
            char_search_internal(line, 0, 1, FFIND, "é".as_bytes()),
            Some(1)
        );
        assert_eq!(
            char_search_internal(line, line.len() as c_int, 1, BFIND, "é".as_bytes()),
            Some(line.len() as c_int - 2)
        );
    }
}
