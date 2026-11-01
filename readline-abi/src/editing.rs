//! Bindable editing commands that operate directly on the exported ABI line
//! buffer.
//!
//! This is the standalone half of GNU readline's command table: every command
//! here reads and writes `rl_line_buffer`/`rl_point`/`rl_end`/`rl_mark` and has
//! no dependency on the terminal loop.  Commands that need the live dispatcher
//! (prefix-argument key reading, history recall, incremental search, vi mode,
//! keyboard macros, bracketed paste, redisplay/terminal control) remain inert
//! and are listed at the bottom of this module.
//!
//! Point and mark are byte offsets into `rl_line_buffer`, as in GNU readline.
//! Character motion decodes UTF-8 where the bytes form a valid sequence and
//! falls back to one byte for invalid input.  Case conversion is byte-oriented:
//! ASCII letters are converted, non-ASCII bytes are left untouched (GNU's
//! multibyte case-width adjustment is not part of this slice).

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_ulong};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::state;

type Command = unsafe extern "C" fn(c_int, c_int) -> c_int;

pub(crate) const RL_STATE_NUMERICARG_BIT: c_ulong = 0x0000_0400;
pub(crate) const RL_STATE_OVERWRITE_BIT: c_ulong = 0x0000_2000;
const RL_STATE_INPUTPENDING: c_ulong = 0x0002_0000;

const RL_STATE_NUMERICARG: c_ulong = RL_STATE_NUMERICARG_BIT;
const RL_STATE_OVERWRITE: c_ulong = RL_STATE_OVERWRITE_BIT;

/// GNU's `RL_IM_INSERT` / `RL_IM_OVERWRITE` values.
const RL_IM_INSERT: c_int = 1;
const RL_IM_OVERWRITE: c_int = 0;

/// GNU keeps ten kill-ring slots.
const MAX_KILLS: usize = 10;

/// Active-region state from `text.c`.  GNU stores the mark-active flag and a
/// counter that commands bump when the mark must survive the command.
static MARK_ACTIVE: AtomicBool = AtomicBool::new(false);
static KEEP_MARK_ACTIVE: AtomicI32 = AtomicI32::new(0);

fn activate_mark() {
    MARK_ACTIVE.store(true, Ordering::Relaxed);
    keep_mark_active();
}

fn keep_mark_active() {
    KEEP_MARK_ACTIVE.fetch_add(1, Ordering::Relaxed);
}

fn deactivate_mark() {
    MARK_ACTIVE.store(false, Ordering::Relaxed);
}

fn mark_is_active() -> bool {
    MARK_ACTIVE.load(Ordering::Relaxed)
}

// --- Line-buffer helpers --------------------------------------------------

pub(crate) fn line_slice<'a>() -> &'a [u8] {
    let buffer = state::line_buffer();
    let end = state::end().max(0) as usize;
    if buffer.is_null() {
        &[]
    } else {
        // SAFETY: the buffer holds at least `end` bytes followed by a NUL.
        unsafe { std::slice::from_raw_parts(buffer as *const u8, end) }
    }
}

pub(crate) fn byte_at(position: c_int) -> Option<u8> {
    if position < 0 {
        return None;
    }
    line_slice().get(position as usize).copied()
}

pub(crate) fn insert_bytes(bytes: &[u8]) -> c_int {
    if bytes.is_empty() {
        return 0;
    }
    let mut terminated = Vec::with_capacity(bytes.len() + 1);
    terminated.extend_from_slice(bytes);
    terminated.push(0);
    // SAFETY: `terminated` is NUL-terminated and valid for the call.
    unsafe { crate::line::rl_insert_text(terminated.as_ptr() as *const c_char) }
}

pub(crate) fn delete_range(from: c_int, to: c_int) -> c_int {
    // SAFETY: the exported helper clamps both endpoints itself.
    unsafe { crate::line::rl_delete_text(from, to) }
}

pub(crate) fn copy_range(from: c_int, to: c_int) -> Vec<u8> {
    // SAFETY: the exported helper returns a malloc-compatible string.
    let copy = unsafe { crate::line::rl_copy_text(from, to) };
    if copy.is_null() {
        return Vec::new();
    }
    // SAFETY: `copy` is NUL-terminated and owned by us until freed below.
    let bytes = unsafe { CStr::from_ptr(copy) }.to_bytes().to_vec();
    // SAFETY: `rl_copy_text` allocated this with malloc.
    unsafe { libc::free(copy as *mut libc::c_void) };
    bytes
}

pub(crate) fn ring_bell() {
    // SAFETY: rl_ding only writes a bell to stderr.
    unsafe {
        crate::line::rl_ding();
    }
}

fn emacs_mode() -> bool {
    crate::keymap::emacs_mode()
}

fn state_set(bits: c_ulong) {
    state::set_readline_state(state::readline_state() | bits);
}

fn state_clear(bits: c_ulong) {
    state::set_readline_state(state::readline_state() & !bits);
}

// --- Character and word boundaries ---------------------------------------

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

pub(crate) fn next_position(bytes: &[u8], position: usize, count: c_int) -> usize {
    let mut position = position.min(bytes.len());
    for _ in 0..count.max(0) {
        if position >= bytes.len() {
            break;
        }
        position += character_len(bytes, position);
    }
    position
}

pub(crate) fn previous_position(bytes: &[u8], position: usize, count: c_int) -> usize {
    let mut position = position.min(bytes.len());
    for _ in 0..count.max(0) {
        if position == 0 {
            break;
        }
        let lower = position.saturating_sub(4);
        let mut start = position - 1;
        while start > lower && (bytes[start] & 0xc0) == 0x80 {
            start -= 1;
        }
        position = if position - start == character_len(bytes, start) {
            start
        } else {
            position - 1
        };
    }
    position
}

fn decoded_character(bytes: &[u8], position: usize) -> Option<char> {
    let length = character_len(bytes, position);
    std::str::from_utf8(&bytes[position..position + length])
        .ok()
        .and_then(|text| text.chars().next())
}

/// GNU's `rl_alphabetic` in the default (non-pathname) mode: alphanumeric
/// characters belong in words.
fn word_at(bytes: &[u8], position: usize) -> bool {
    let Some(&byte) = bytes.get(position) else {
        return false;
    };
    if byte.is_ascii() {
        byte.is_ascii_alphanumeric()
    } else {
        decoded_character(bytes, position).is_none_or(|character| character.is_alphanumeric())
    }
}

pub(crate) fn whitespace(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

// --- Kill ring ------------------------------------------------------------

#[derive(Default)]
struct KillRing {
    /// Oldest kill first, newest kill last, matching GNU's ring layout.
    items: Vec<Vec<u8>>,
    index: usize,
    /// GNU's `_rl_last_command_was_kill` counter.  Zero means the previous
    /// command was not a kill, so the next kill starts a fresh ring slot.
    generation: u64,
}

static KILL_RING: Mutex<KillRing> = Mutex::new(KillRing {
    items: Vec::new(),
    index: 0,
    generation: 0,
});

fn ring() -> MutexGuard<'static, KillRing> {
    KILL_RING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn add_kill(text: Vec<u8>, append: bool) {
    let mut ring = ring();
    if ring.generation == 0 || ring.items.is_empty() {
        if ring.items.len() == MAX_KILLS {
            ring.items.remove(0);
        }
        ring.items.push(text);
        ring.index = ring.items.len() - 1;
        return;
    }

    // GNU never coalesces consecutive kills in vi mode: the newest kill
    // replaces the current slot instead of being appended or prepended.
    if crate::keymap::vi_mode() {
        let slot = ring.items.len() - 1;
        ring.items[slot] = text;
        ring.index = slot;
        return;
    }

    // Emacs coalesces consecutive kills in the newest slot.
    let slot = ring.items.len() - 1;
    let old = std::mem::take(&mut ring.items[slot]);
    let mut combined = Vec::with_capacity(old.len() + text.len());
    if append {
        combined.extend_from_slice(&old);
        combined.extend_from_slice(&text);
    } else {
        combined.extend_from_slice(&text);
        combined.extend_from_slice(&old);
    }
    ring.items[slot] = combined;
    ring.index = slot;
}

fn record_kill() {
    let mut ring = ring();
    ring.generation = ring.generation.wrapping_add(1);
}

fn current_kill() -> Option<Vec<u8>> {
    let ring = ring();
    ring.items.get(ring.index).cloned()
}

pub(crate) fn kill_ring_current() -> Option<Vec<u8>> {
    current_kill()
}

/// Rotate the kill ring backwards and return the newly selected slot.  This is
/// the ring half of `yank-pop` and of vi's `p`/`P` pop command.
pub(crate) fn kill_ring_rotate_back() -> Option<Vec<u8>> {
    let mut ring = ring();
    if ring.items.is_empty() {
        return None;
    }
    ring.index = if ring.index == 0 {
        ring.items.len() - 1
    } else {
        ring.index - 1
    };
    ring.items.get(ring.index).cloned()
}

/// GNU's `rl_kill_text`: copy and delete a region, prepending/appending to the
/// kill ring according to the direction of the region.
pub(crate) fn kill_text(from: c_int, to: c_int) {
    if from == to {
        record_kill();
        return;
    }

    let text = copy_range(from, to);
    delete_range(from, to);
    add_kill(text, from < to);
    record_kill();
}

fn region_kill_internal(delete: bool) {
    let point = state::point();
    let mark = state::mark();
    if mark != point {
        let text = copy_range(point, mark);
        if delete {
            delete_range(point, mark);
        }
        add_kill(text, point < mark);
    }
    record_kill();
    state::clamp_point();
    state::clamp_mark();
}

// --- Command bookkeeping --------------------------------------------------

/// Command wrappers call this before and after the body so that consecutive
/// kills coalesce and a numeric argument does not leak into the next command.
pub(crate) fn begin_command() -> u64 {
    state::ensure_line_capacity(1);
    // GNU does not clamp the mark between commands; a negative mark is the
    // "no mark" state that `exchange-point-and-mark` rejects.
    state::clamp_point();
    ring().generation
}

pub(crate) fn end_command(previous_generation: u64, numeric_prefix: bool) {
    if !numeric_prefix {
        state::reset_argument();
        state_clear(RL_STATE_NUMERICARG);
    }
    let mut ring = ring();
    if ring.generation == previous_generation {
        // No kill happened during this command.
        ring.generation = 0;
    }
}

// --- Insertion ------------------------------------------------------------

fn insert_repeated(byte: u8, count: c_int, overwrite: bool) {
    if count <= 0 {
        return;
    }

    if !overwrite {
        let copies = vec![byte; count as usize];
        insert_bytes(&copies);
        return;
    }

    for _ in 0..count {
        insert_bytes(&[byte]);
        let point = state::point();
        if point < state::end() {
            let bytes = line_slice();
            let next = next_position(bytes, point as usize, 1) as c_int;
            delete_range(point, next);
        }
    }
}

pub(crate) fn insert(count: c_int, key: c_int) -> c_int {
    if count <= 0 {
        return 0;
    }
    // In GNU the vi replace keymap dispatches printable keys to
    // `rl_vi_overstrike`.  The standalone adapter has no dispatcher, so the
    // self-insert command consults the selected map.
    if crate::keymap::vi_replace_keymap_active() && (0x20..=0xff).contains(&key) {
        return crate::vi::overstrike(count, key);
    }
    // GNU stores the dispatched key in a C `char`, so out-of-range values wrap
    // to a single byte.  A zero key inserts nothing.
    insert_repeated(key as u8, count, state::insert_mode() == RL_IM_OVERWRITE);
    crate::vi::note_self_insert(key, count);
    0
}

pub(crate) fn quoted_insert(count: c_int, _key: c_int) -> c_int {
    // The standalone library has no key source.  A pending input byte is the
    // honest equivalent of GNU's `rl_read_key()` in `_rl_insert_next`; the
    // live terminal read belongs to the dispatcher (niubash#775).
    let pending = state::pending_input();
    if pending <= 0 || pending > 0xff {
        ring_bell();
        return 1;
    }

    state::set_pending_input(0);
    state_clear(RL_STATE_INPUTPENDING);
    let copies = if count < 0 { 1 } else { count };
    if copies <= 0 {
        return 0;
    }
    insert_repeated(
        pending as u8,
        copies,
        state::insert_mode() == RL_IM_OVERWRITE,
    );
    crate::vi::note_self_insert(pending, copies);
    0
}

pub(crate) fn tab_insert(count: c_int, _key: c_int) -> c_int {
    insert_repeated(b'\t', count, false);
    crate::vi::note_self_insert(b'\t' as c_int, count);
    0
}

pub(crate) fn accept_line(_count: c_int, _key: c_int) -> c_int {
    if mark_is_active() {
        deactivate_mark();
        // SAFETY: the redisplay hook only paints the current line.
        crate::completion::redisplay_with_hook();
    }
    state::set_done(1);
    state_set(crate::line::RL_STATE_DONE);
    0
}

pub(crate) fn abort(_count: c_int, _key: c_int) -> c_int {
    // GNU longjmps out of the dispatch loop here.  The standalone library has
    // no loop to unwind, so reset the same visible state and return.
    ring_bell();
    state::set_pending_input(0);
    state_clear(RL_STATE_INPUTPENDING);
    state::reset_argument();
    state_clear(RL_STATE_NUMERICARG);
    deactivate_mark();
    // SAFETY: clearing the message only resets prompt/display state.
    unsafe {
        crate::line::rl_clear_message();
    }
    state::set_last_func(None);
    0
}

pub(crate) fn discard_argument_body(_count: c_int, _key: c_int) -> c_int {
    ring_bell();
    // SAFETY: clearing the message only resets prompt/display state.
    unsafe {
        crate::line::rl_clear_message();
    }
    state::reset_argument();
    state_clear(RL_STATE_NUMERICARG);
    0
}

// --- Movement -------------------------------------------------------------

pub(crate) fn forward_byte(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return backward_byte(-count, key);
    }
    if count > 0 {
        let end = state::point() + count;
        if end > state::end() {
            state::set_end(state::end().max(0));
            state::set_point(state::end());
            ring_bell();
        } else {
            state::set_point(end);
        }
    }
    if state::end() < 0 {
        state::set_end(0);
    }
    0
}

pub(crate) fn backward_byte(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return forward_byte(-count, key);
    }
    if count > 0 {
        if state::point() < count {
            state::set_point(0);
            ring_bell();
        } else {
            state::set_point(state::point() - count);
        }
    }
    if state::point() < 0 {
        state::set_point(0);
    }
    0
}

pub(crate) fn forward_char(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return backward_char(-count, key);
    }
    if count > 0 {
        if state::point() == state::end() && emacs_mode() {
            ring_bell();
            return 0;
        }
        let bytes = line_slice();
        let position = next_position(bytes, state::point().max(0) as usize, count) as c_int;
        if position == state::point() {
            ring_bell();
        }
        state::set_point(position);
        state::clamp_point();
    }
    0
}

pub(crate) fn backward_char(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return forward_char(-count, key);
    }
    if count > 0 {
        let bytes = line_slice();
        let mut position = state::point().max(0) as usize;
        let mut moved = 0;
        while moved < count && position > 0 {
            position = previous_position(bytes, position, 1);
            moved += 1;
        }
        if moved < count {
            ring_bell();
        }
        state::set_point(position as c_int);
        state::clamp_point();
    }
    0
}

pub(crate) fn beginning_of_line(_count: c_int, _key: c_int) -> c_int {
    state::set_point(0);
    0
}

pub(crate) fn end_of_line(_count: c_int, _key: c_int) -> c_int {
    state::set_point(state::end());
    0
}

pub(crate) fn forward_word(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return backward_word(-count, key);
    }

    let mut remaining = count;
    while remaining != 0 {
        if state::point() > state::end() {
            state::set_point(state::end());
        }
        if state::point() == state::end() {
            return 0;
        }

        let bytes = line_slice();
        if !word_at(bytes, state::point() as usize) {
            state::set_point(next_position(bytes, state::point() as usize, 1) as c_int);
            while state::point() < state::end() {
                let bytes = line_slice();
                if word_at(bytes, state::point() as usize) {
                    break;
                }
                state::set_point(next_position(bytes, state::point() as usize, 1) as c_int);
            }
        }

        if state::point() > state::end() {
            state::set_point(state::end());
        }
        if state::point() == state::end() {
            return 0;
        }

        let bytes = line_slice();
        state::set_point(next_position(bytes, state::point() as usize, 1) as c_int);
        while state::point() < state::end() {
            let bytes = line_slice();
            if !word_at(bytes, state::point() as usize) {
                break;
            }
            state::set_point(next_position(bytes, state::point() as usize, 1) as c_int);
        }

        remaining -= 1;
    }
    0
}

pub(crate) fn backward_word(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return forward_word(-count, key);
    }

    let mut remaining = count;
    while remaining != 0 {
        if state::point() == 0 {
            return 0;
        }

        let bytes = line_slice();
        let mut position = previous_position(bytes, state::point() as usize, 1);
        if !word_at(bytes, position) {
            state::set_point(position as c_int);
            while state::point() > 0 {
                let bytes = line_slice();
                position = previous_position(bytes, state::point() as usize, 1);
                if word_at(bytes, position) {
                    break;
                }
                state::set_point(position as c_int);
            }
        }

        while state::point() > 0 {
            let bytes = line_slice();
            position = previous_position(bytes, state::point() as usize, 1);
            if !word_at(bytes, position) {
                break;
            }
            state::set_point(position as c_int);
        }

        remaining -= 1;
    }
    0
}

pub(crate) fn set_mark(count: c_int, _key: c_int) -> c_int {
    let position = if state::explicit_arg() != 0 {
        count
    } else {
        state::point()
    };
    if !(0..=state::end()).contains(&position) {
        return 1;
    }
    state::set_mark(position);
    0
}

pub(crate) fn exchange_point_and_mark(_count: c_int, _key: c_int) -> c_int {
    if state::mark() > state::end() {
        state::set_mark(-1);
    }
    if state::mark() < 0 {
        ring_bell();
        state::set_mark(0);
        return 1;
    }

    let point = state::point();
    state::set_point(state::mark());
    state::set_mark(point);
    activate_mark();
    0
}

// --- Deletion and killing -------------------------------------------------

fn overwrite_rubout(count: c_int, key: c_int) -> c_int {
    if state::point() == 0 {
        ring_bell();
        return 1;
    }

    let orig_point = state::point();
    let mut width = 0;
    for _ in 0..count.max(1) {
        backward_char(1, key);
        width += 1;
    }

    if count > 1 || state::explicit_arg() != 0 {
        kill_text(orig_point, state::point());
    } else {
        delete_range(orig_point, state::point());
    }

    // GNU leaves the display columns in place by inserting spaces when text
    // follows the erased region.
    if state::point() < state::end() {
        let start = state::point();
        insert_repeated(b' ', width, false);
        state::set_point(start);
    }
    0
}

fn rubout_char(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return delete_char(-count, key);
    }
    if state::point() == 0 {
        ring_bell();
        return 1;
    }

    let orig_point = state::point();
    if count > 1 || state::explicit_arg() != 0 {
        backward_char(count, key);
        kill_text(orig_point, state::point());
    } else {
        let bytes = line_slice();
        let position = previous_position(bytes, orig_point as usize, 1) as c_int;
        delete_range(position, orig_point);
        state::set_point(position);
    }
    0
}

pub(crate) fn backward_delete_char(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return delete_char(-count, key);
    }
    // RUBOUT in the vi replace keymap is `rl_vi_overstrike_delete`.
    if crate::keymap::vi_replace_keymap_active() {
        return crate::vi::overstrike_delete(count, key);
    }
    if state::point() == 0 {
        ring_bell();
        return 1;
    }
    if state::insert_mode() == RL_IM_OVERWRITE {
        return overwrite_rubout(count, key);
    }
    rubout_char(count, key)
}

pub(crate) fn delete_char(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return rubout_char(-count, key);
    }
    if state::point() == state::end() {
        ring_bell();
        return 1;
    }

    if count > 1 || state::explicit_arg() != 0 {
        let start = state::point();
        forward_char(count, key);
        kill_text(start, state::point());
        state::set_point(start);
    } else {
        let bytes = line_slice();
        let next = next_position(bytes, state::point() as usize, 1) as c_int;
        delete_range(state::point(), next);
    }
    0
}

pub(crate) fn forward_backward_delete_char(count: c_int, key: c_int) -> c_int {
    if state::end() != 0 && state::point() == state::end() {
        rubout_char(count, key)
    } else {
        delete_char(count, key)
    }
}

pub(crate) fn delete_char_or_list(count: c_int, key: c_int) -> c_int {
    if state::end() != 0 && state::point() == state::end() {
        // SAFETY: the completion entry point only touches line/completion state.
        unsafe { crate::completion::rl_possible_completions(count, key) }
    } else {
        delete_char(count, key)
    }
}

pub(crate) fn delete_horizontal_space(_count: c_int, _key: c_int) -> c_int {
    while state::point() > 0 && byte_at(state::point() - 1).is_some_and(whitespace) {
        state::set_point(state::point() - 1);
    }

    let start = state::point();
    while state::point() < state::end() && byte_at(state::point()).is_some_and(whitespace) {
        state::set_point(state::point() + 1);
    }

    if start != state::point() {
        delete_range(start, state::point());
        state::set_point(start);
    }
    if state::point() < 0 {
        state::set_point(0);
    }
    0
}

pub(crate) fn kill_word(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return backward_kill_word(-count, key);
    }

    let orig_point = state::point();
    forward_word(count, key);
    if state::point() != orig_point {
        kill_text(orig_point, state::point());
    }
    state::set_point(orig_point);
    if emacs_mode() {
        state::set_mark(state::point());
    }
    0
}

pub(crate) fn backward_kill_word(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return kill_word(-count, key);
    }

    let orig_point = state::point();
    backward_word(count, key);
    if state::point() != orig_point {
        kill_text(orig_point, state::point());
    }
    if emacs_mode() {
        state::set_mark(state::point());
    }
    0
}

pub(crate) fn kill_line(direction: c_int, key: c_int) -> c_int {
    if direction < 0 {
        return backward_kill_line(1, key);
    }

    let orig_point = state::point();
    end_of_line(1, key);
    if orig_point != state::point() {
        kill_text(orig_point, state::point());
    }
    state::set_point(orig_point);
    if emacs_mode() {
        state::set_mark(state::point());
    }
    0
}

pub(crate) fn backward_kill_line(direction: c_int, key: c_int) -> c_int {
    if direction < 0 {
        return kill_line(1, key);
    }
    if state::point() == 0 {
        ring_bell();
        return 0;
    }

    let orig_point = state::point();
    beginning_of_line(1, key);
    if orig_point != state::point() {
        kill_text(orig_point, state::point());
    }
    if emacs_mode() {
        state::set_mark(state::point());
    }
    0
}

pub(crate) fn unix_line_discard(_count: c_int, _key: c_int) -> c_int {
    if state::point() == 0 {
        ring_bell();
        return 0;
    }

    kill_text(state::point(), 0);
    state::set_point(0);
    if emacs_mode() {
        state::set_mark(0);
    }
    0
}

pub(crate) fn unix_word_rubout(count: c_int, _key: c_int) -> c_int {
    if state::point() == 0 {
        ring_bell();
        return 0;
    }

    let orig_point = state::point();
    let mut remaining = if count <= 0 { 1 } else { count };
    while remaining > 0 {
        while state::point() > 0 && byte_at(state::point() - 1).is_some_and(whitespace) {
            state::set_point(state::point() - 1);
        }
        while state::point() > 0
            && byte_at(state::point() - 1).is_some_and(|byte| !whitespace(byte))
        {
            state::set_point(state::point() - 1);
        }
        remaining -= 1;
    }

    kill_text(orig_point, state::point());
    if emacs_mode() {
        state::set_mark(state::point());
    }
    0
}

pub(crate) fn unix_filename_rubout(count: c_int, _key: c_int) -> c_int {
    if state::point() == 0 {
        ring_bell();
        return 0;
    }

    let orig_point = state::point();
    let mut remaining = if count <= 0 { 1 } else { count };
    while state::point() > 0 && remaining > 0 {
        remaining -= 1;
        let mut current = byte_at(state::point() - 1).unwrap_or(0);

        while state::point() > 0 && whitespace(current) {
            state::set_point(state::point() - 1);
            if state::point() > 0 {
                current = byte_at(state::point() - 1).unwrap_or(0);
            }
        }

        if current == b'/' {
            let mut index = state::point().saturating_sub(1);
            while index > 0 && current == b'/' {
                index -= 1;
                current = byte_at(index).unwrap_or(0);
            }
            if index == 0 || whitespace(current) {
                let trailing_whitespace = if whitespace(current) { 1 } else { 0 };
                state::set_point(index + trailing_whitespace);
                continue;
            }
            current = b'/';
        }

        while state::point() > 0 && (whitespace(current) || current == b'/') {
            state::set_point(state::point() - 1);
            if state::point() > 0 {
                current = byte_at(state::point() - 1).unwrap_or(0);
            }
        }

        while state::point() > 0 && !whitespace(current) && current != b'/' {
            state::set_point(state::point() - 1);
            if state::point() > 0 {
                current = byte_at(state::point() - 1).unwrap_or(0);
            }
        }
    }

    kill_text(orig_point, state::point());
    if emacs_mode() {
        state::set_mark(state::point());
    }
    0
}

pub(crate) fn kill_whole_line(_count: c_int, _key: c_int) -> c_int {
    state::set_point(0);
    kill_text(state::point(), state::end());
    state::set_mark(0);
    0
}

pub(crate) fn kill_region(_count: c_int, _key: c_int) -> c_int {
    let new_point = state::point().min(state::mark());
    region_kill_internal(true);
    state::set_point(new_point);
    state::clamp_point();
    state::clamp_mark();
    0
}

pub(crate) fn copy_region_as_kill(_count: c_int, _key: c_int) -> c_int {
    region_kill_internal(false);
    0
}

fn copy_word_as_kill(count: c_int, direction: c_int) -> c_int {
    let saved_mark = state::mark();
    let saved_point = state::point();

    if direction > 0 {
        forward_word(count, 0);
    } else {
        backward_word(count, 0);
    }
    state::set_mark(state::point());

    if direction > 0 {
        backward_word(count, 0);
    } else {
        forward_word(count, 0);
    }

    region_kill_internal(false);
    state::set_mark(saved_mark);
    state::set_point(saved_point);
    0
}

pub(crate) fn copy_forward_word(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return copy_backward_word(-count, key);
    }
    copy_word_as_kill(count, 1)
}

pub(crate) fn copy_backward_word(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return copy_forward_word(-count, key);
    }
    copy_word_as_kill(count, -1)
}

// --- Yanking --------------------------------------------------------------

fn last_command_is(name: &str) -> bool {
    let Some(last) = state::last_func() else {
        return false;
    };
    let Some(expected) = crate::keymap_defaults::default_command(name) else {
        return false;
    };
    std::ptr::fn_addr_eq(last, expected)
}

pub(crate) fn abort_without_loop() -> c_int {
    ring_bell();
    state::set_pending_input(0);
    state_clear(RL_STATE_INPUTPENDING);
    state::reset_argument();
    state_clear(RL_STATE_NUMERICARG);
    1
}

pub(crate) fn yank(_count: c_int, _key: c_int) -> c_int {
    let Some(text) = current_kill() else {
        return abort_without_loop();
    };
    state::set_mark(state::point());
    insert_bytes(&text);
    0
}

pub(crate) fn yank_pop(_count: c_int, _key: c_int) -> c_int {
    if !last_command_is("yank") && !last_command_is("yank-pop") {
        return abort_without_loop();
    }
    let Some(text) = current_kill() else {
        return abort_without_loop();
    };

    let length = text.len() as c_int;
    let start = state::point() - length;
    if start < 0 {
        return abort_without_loop();
    }
    let bytes = line_slice();
    if bytes.get(start as usize..state::point() as usize) != Some(text.as_slice()) {
        return abort_without_loop();
    }

    delete_range(start, state::point());
    state::set_point(start);

    let next = {
        let mut ring = ring();
        if ring.items.is_empty() {
            None
        } else {
            ring.index = if ring.index == 0 {
                ring.items.len() - 1
            } else {
                ring.index - 1
            };
            ring.items.get(ring.index).cloned()
        }
    };
    let Some(next) = next else {
        return abort_without_loop();
    };

    state::set_mark(state::point());
    insert_bytes(&next);
    0
}

// --- Case and transposition ----------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaseOperation {
    Up,
    Down,
    Capitalize,
}

fn change_case(count: c_int, operation: CaseOperation) -> c_int {
    let mut start = state::point();
    forward_word(count, 0);
    let mut end = state::point();

    if count < 0 {
        std::mem::swap(&mut start, &mut end);
    }

    let mut in_word = false;
    let mut position = start.max(0) as usize;
    let end = end.max(0) as usize;
    while position < end {
        let bytes = line_slice();
        if position >= bytes.len() {
            break;
        }
        let length = character_len(bytes, position);
        if !word_at(bytes, position) {
            in_word = false;
            position += length;
            continue;
        }

        let selected = match operation {
            CaseOperation::Capitalize => {
                let selected = if in_word {
                    CaseOperation::Down
                } else {
                    CaseOperation::Up
                };
                in_word = true;
                selected
            }
            other => other,
        };

        if length == 1 {
            let byte = bytes[position];
            let converted = match selected {
                CaseOperation::Up => byte.to_ascii_uppercase(),
                CaseOperation::Down | CaseOperation::Capitalize => byte.to_ascii_lowercase(),
            };
            // SAFETY: `position` is inside the NUL-terminated line buffer.
            unsafe {
                *state::char_ptr_as_u8(state::line_buffer()).add(position) = converted;
            }
        }

        position += length;
    }

    state::set_point(end as c_int);
    0
}

pub(crate) fn upcase_word(count: c_int, _key: c_int) -> c_int {
    change_case(count, CaseOperation::Up)
}

pub(crate) fn downcase_word(count: c_int, _key: c_int) -> c_int {
    change_case(count, CaseOperation::Down)
}

pub(crate) fn capitalize_word(count: c_int, _key: c_int) -> c_int {
    change_case(count, CaseOperation::Capitalize)
}

pub(crate) fn transpose_chars(count: c_int, _key: c_int) -> c_int {
    if count == 0 {
        return 0;
    }
    if state::point() == 0 || state::end() < 2 {
        ring_bell();
        return 1;
    }

    let mut count = count;
    if state::point() == state::end() {
        let bytes = line_slice();
        state::set_point(previous_position(bytes, state::point() as usize, 1) as c_int);
        count = 1;
    }

    let prev_point = state::point();
    let bytes = line_slice();
    let start = previous_position(bytes, prev_point as usize, 1) as c_int;
    state::set_point(start);
    let character = copy_range(start, prev_point);

    delete_range(start, prev_point);
    let bytes = line_slice();
    let advanced = next_position(bytes, start.max(0) as usize, count) as c_int;
    state::set_point(advanced);
    state::clamp_point();
    insert_bytes(&character);
    0
}

pub(crate) fn transpose_words(count: c_int, key: c_int) -> c_int {
    if count == 0 {
        return 0;
    }

    let orig_point = state::point();
    let orig_end = state::end();

    forward_word(count, key);
    let w2_end = state::point();
    backward_word(1, key);
    let w2_beg = state::point();
    backward_word(count, key);
    let w1_beg = state::point();
    forward_word(1, key);
    let w1_end = state::point();

    if w1_beg == w2_beg || w2_beg < w1_end {
        ring_bell();
        state::set_point(orig_point);
        return 1;
    }

    let word1 = copy_range(w1_beg, w1_end);
    let word2 = copy_range(w2_beg, w2_end);

    state::set_point(w2_beg);
    delete_range(w2_beg, w2_end);
    insert_bytes(&word1);

    state::set_point(w1_beg);
    delete_range(w1_beg, w1_end);
    insert_bytes(&word2);

    state::set_point(w2_end);
    state::set_end(orig_end);
    0
}

// --- Modes and numeric arguments -----------------------------------------

fn set_insert_mode(mode: c_int) {
    state::set_insert_mode(mode);
    if mode == RL_IM_OVERWRITE {
        state_set(RL_STATE_OVERWRITE);
    } else {
        state_clear(RL_STATE_OVERWRITE);
    }
}

pub(crate) fn do_lowercase_version(_count: c_int, _key: c_int) -> c_int {
    // GNU's `_rl_dispatch_subseq` treats this sentinel specially: it retries
    // the same key in lowercase.  The standalone library has no dispatch loop
    // to retry into, so it reports the sentinel exactly as GNU's body does.
    99999
}

pub(crate) fn emacs_editing_mode(_count: c_int, _key: c_int) -> c_int {
    set_insert_mode(RL_IM_INSERT);
    crate::keymap::set_emacs_mode();
    0
}

pub(crate) fn overwrite_mode(count: c_int, _key: c_int) -> c_int {
    if state::explicit_arg() == 0 {
        set_insert_mode(state::insert_mode() ^ 1);
    } else if count > 0 {
        set_insert_mode(RL_IM_OVERWRITE);
    } else {
        set_insert_mode(RL_IM_INSERT);
    }
    0
}

pub(crate) fn digit_argument(_count: c_int, key: c_int) -> c_int {
    // The standalone command records the prefix argument but cannot read the
    // following key or redispatch with it; that is the live dispatcher's job.
    if (b'0' as c_int..=b'9' as c_int).contains(&key) {
        let digit = key - b'0' as c_int;
        let value = if state::explicit_arg() != 0 {
            state::numeric_arg()
                .saturating_mul(10)
                .saturating_add(digit)
        } else {
            digit
        };
        state::set_numeric_arg(value);
        state::set_explicit_arg(1);
        state_set(RL_STATE_NUMERICARG);
        return 0;
    }

    if key == b'-' as c_int && state::explicit_arg() == 0 {
        state::set_numeric_arg(1);
        state::set_argument_sign(-1);
        state_set(RL_STATE_NUMERICARG);
        return 0;
    }

    ring_bell();
    1
}

pub(crate) fn universal_argument(_count: c_int, _key: c_int) -> c_int {
    state::set_numeric_arg(state::numeric_arg().saturating_mul(4));
    state::set_explicit_arg(1);
    state_set(RL_STATE_NUMERICARG);
    0
}

// --- Exported GNU command ABI ---------------------------------------------
//
// Every implemented command is exported under its GNU `rl_*` name with the
// documented `(count, key)` signature.  The default funmap points at these
// symbols (not at private wrappers), so `rl_named_function`, `rl_add_defun`,
// key dispatch and the ABI audit all resolve to the same body.
//
// Bookkeeping that GNU performs in its dispatch loop (`rl_last_func`,
// resetting the numeric argument, clamping point) lives here because the
// standalone library has no loop to do it.

macro_rules! export_command {
    ($export:ident => $body:path) => {
        #[no_mangle]
        pub unsafe extern "C" fn $export(count: c_int, key: c_int) -> c_int {
            let previous = begin_command();
            let result = $body(count, key);
            end_command(previous, false);
            state::set_last_func(Some($export as Command));
            result
        }
    };
}

macro_rules! export_prefix_command {
    ($export:ident => $body:path) => {
        #[no_mangle]
        pub unsafe extern "C" fn $export(count: c_int, key: c_int) -> c_int {
            let previous = begin_command();
            let result = $body(count, key);
            // A prefix command keeps the argument for the command it prefixes
            // and does not become `rl_last_func` (GNU's dispatcher rule).
            end_command(previous, true);
            result
        }
    };
}

/// Abort the current editing command.
#[no_mangle]
pub unsafe extern "C" fn rl_abort(count: c_int, key: c_int) -> c_int {
    let previous = begin_command();
    let result = abort(count, key);
    end_command(previous, false);
    result
}

export_command!(rl_newline => accept_line);
export_command!(rl_forward => forward_char);
export_command!(rl_forward_char => forward_char);
export_command!(rl_forward_byte => forward_byte);
export_command!(rl_backward => backward_char);
export_command!(rl_backward_char => backward_char);
export_command!(rl_backward_byte => backward_byte);
export_command!(rl_beg_of_line => beginning_of_line);
export_command!(rl_end_of_line => end_of_line);
export_command!(rl_forward_word => forward_word);
export_command!(rl_backward_word => backward_word);
export_command!(rl_insert => insert);
export_command!(rl_quoted_insert => quoted_insert);
export_command!(rl_tab_insert => tab_insert);
export_command!(rl_rubout => backward_delete_char);
export_command!(rl_delete => delete_char);
export_command!(rl_rubout_or_delete => forward_backward_delete_char);
export_command!(rl_delete_horizontal_space => delete_horizontal_space);
export_command!(rl_delete_or_show_completions => delete_char_or_list);
export_command!(rl_upcase_word => upcase_word);
export_command!(rl_downcase_word => downcase_word);
export_command!(rl_capitalize_word => capitalize_word);
export_command!(rl_transpose_words => transpose_words);
export_command!(rl_transpose_chars => transpose_chars);
export_command!(rl_set_mark => set_mark);
export_command!(rl_exchange_point_and_mark => exchange_point_and_mark);
export_command!(rl_kill_word => kill_word);
export_command!(rl_backward_kill_word => backward_kill_word);
export_command!(rl_kill_line => kill_line);
export_command!(rl_backward_kill_line => backward_kill_line);
export_command!(rl_kill_full_line => kill_whole_line);
export_command!(rl_unix_word_rubout => unix_word_rubout);
export_command!(rl_unix_filename_rubout => unix_filename_rubout);
export_command!(rl_unix_line_discard => unix_line_discard);
export_command!(rl_copy_region_to_kill => copy_region_as_kill);
export_command!(rl_kill_region => kill_region);
export_command!(rl_copy_forward_word => copy_forward_word);
export_command!(rl_copy_backward_word => copy_backward_word);
export_command!(rl_yank => yank);
export_command!(rl_yank_pop => yank_pop);
export_command!(rl_overwrite_mode => overwrite_mode);
export_command!(rl_emacs_editing_mode => emacs_editing_mode);
export_prefix_command!(rl_digit_argument => digit_argument);
export_prefix_command!(rl_universal_argument => universal_argument);

/// GNU's placeholder command that asks the dispatcher to retry the key in
/// lowercase.  With no dispatcher it returns the same sentinel body value.
#[no_mangle]
pub unsafe extern "C" fn rl_do_lowercase_version(count: c_int, key: c_int) -> c_int {
    do_lowercase_version(count, key)
}

/// Discard a partially typed numeric argument.
#[no_mangle]
pub unsafe extern "C" fn rl_discard_argument() -> c_int {
    discard_argument_body(0, 0)
}

/// Mark the region as active and keep it active for the current command.
#[no_mangle]
pub unsafe extern "C" fn rl_activate_mark() {
    activate_mark();
}

/// Mark the region as inactive.
#[no_mangle]
pub unsafe extern "C" fn rl_deactivate_mark() {
    deactivate_mark();
}

/// Preserve an active mark across the current command.
#[no_mangle]
pub unsafe extern "C" fn rl_keep_mark_active() {
    keep_mark_active();
}

/// Whether the region is active.
#[no_mangle]
pub unsafe extern "C" fn rl_mark_active_p() -> c_int {
    c_int::from(mark_is_active())
}

/// GNU's word-character predicate for the C locale.
#[no_mangle]
pub unsafe extern "C" fn rl_alphabetic(character: c_int) -> c_int {
    if (0..=0xff).contains(&character) && (character as u8).is_ascii_alphanumeric() {
        1
    } else {
        0
    }
}

/// Legacy spelling of `rl_alphabetic` (compat.c).
#[no_mangle]
pub unsafe extern "C" fn alphabetic(character: c_int) -> c_int {
    rl_alphabetic(character)
}

/// Legacy spelling of `rl_ding` (compat.c).
#[no_mangle]
pub unsafe extern "C" fn ding() -> c_int {
    // SAFETY: the bell writer only touches the output stream.
    unsafe { crate::line::rl_ding() }
}

/// Free memory allocated by readline (compat.c).
#[no_mangle]
pub unsafe extern "C" fn rl_free(memory: *mut libc::c_void) {
    if !memory.is_null() {
        libc::free(memory);
    }
}

// --- Commands still waiting for the live dispatcher -----------------------

/// Body used by the remaining GNU funmap names.  It is deliberately inert:
/// each of those commands needs the terminal/dispatch loop, history editing,
/// undo, vi mode, or the keyboard-macro subsystem, none of which this
/// standalone slice owns.
///
/// The names still mapped here are: beginning-of-history, end-of-history,
/// previous-history, next-history, fetch-history, operate-and-get-next,
/// *-search-history, the history-substring-search commands,
/// reverse/forward-search-history, the non-incremental-*-search-history
/// commands, insert-comment, revert-line, undo, yank-nth-arg, yank-last-arg,
/// character-search(-backward), clear-display, clear-screen,
/// redraw-current-line, next/previous-screen-line, skip-csi-sequence,
/// execute-named-command, export-completions, the kbd-macro commands,
/// bracketed-paste-begin, paste-from-clipboard, tilde-expand, tty-status,
/// arrow-key-prefix, and every vi-* command.
pub(crate) unsafe extern "C" fn remaining(_count: c_int, _key: c_int) -> c_int {
    0
}
