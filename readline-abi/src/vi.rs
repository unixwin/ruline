//! GNU readline vi-mode commands.
//!
//! The ABI entry points here are the standalone implementation of GNU's
//! `vi_mode.c`, `kill.c:rl_vi_yank_pop`, `misc.c:rl_vi_editing_mode`, and
//! `util.c:rl_tilde_expand` bodies.  Like the rest of this crate they operate
//! directly on the exported line buffer and never pretend to own a terminal.
//!
//! Two seams deliberately remain in the live dispatcher's half:
//!
//! * Commands that need the next key (`f`/`t`, marks, `r`, operator motions)
//!   consume `rl_pending_input`, which is exactly the first thing GNU's
//!   `rl_read_key` checks.  With no pending key they fail honestly instead of
//!   blocking on a terminal read.
//! * Vi search needs a whole search string, not one key.  A pending printable
//!   key is used as the pattern; without one the command rings and records the
//!   seam.  `rl_vi_search_again` reuses the saved pattern and position.
//!
//! Undo is a real entry list (`UNDO_DELETE`/`UNDO_INSERT` plus group markers)
//! fed by the exported line primitives, so `u` undoes exactly the text that a
//! vi delete/insert/change unit changed, including GNU's cursor placement.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::state;

type Command = unsafe extern "C" fn(c_int, c_int) -> c_int;

const RL_STATE_INPUTPENDING: std::os::raw::c_ulong = 0x0002_0000;
const RL_STATE_VICMDONCE: std::os::raw::c_ulong = 0x0040_0000;
const RL_IM_INSERT: c_int = 1;
const RL_IM_OVERWRITE: c_int = 0;

const FFIND: c_int = 2;
const BFIND: c_int = -2;
const FTO: c_int = 1;
const BTO: c_int = -1;

const ESC: c_int = 0x1b;
const CTRL_C: c_int = 0x03;

// --- Undo list ------------------------------------------------------------

enum UndoOp {
    Begin,
    End,
    Insert { start: c_int, end: c_int },
    Delete { start: c_int, text: Vec<u8> },
}

struct UndoState {
    ops: Vec<UndoOp>,
    group_level: i32,
}

static UNDO: Mutex<UndoState> = Mutex::new(UndoState {
    ops: Vec::new(),
    group_level: 0,
});
static UNDOING: AtomicBool = AtomicBool::new(false);

fn undo_state() -> MutexGuard<'static, UndoState> {
    UNDO.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn undo_in_progress() -> bool {
    UNDOING.load(Ordering::Relaxed)
}

pub(crate) fn record_insert(start: c_int, end: c_int) {
    if end <= start {
        return;
    }
    undo_state().ops.push(UndoOp::Insert { start, end });
}

pub(crate) fn record_delete(start: c_int, _end: c_int, text: Vec<u8>) {
    undo_state().ops.push(UndoOp::Delete { start, text });
}

fn begin_undo_group() {
    let mut undo = undo_state();
    undo.ops.push(UndoOp::Begin);
    undo.group_level += 1;
}

fn end_undo_group() {
    let mut undo = undo_state();
    undo.ops.push(UndoOp::End);
    undo.group_level -= 1;
}

fn undo_group_level() -> i32 {
    undo_state().group_level
}

fn clear_undo() {
    let mut undo = undo_state();
    undo.ops.clear();
    undo.group_level = 0;
}

/// Undo one unit, following GNU's `rl_do_undo`: group markers keep the loop
/// running until the matching `BEGIN` has been consumed.
fn do_undo() -> bool {
    let mut waiting_for_begin = 0;
    let mut did = false;
    loop {
        let Some(op) = undo_state().ops.pop() else {
            return did;
        };
        did = true;
        match op {
            UndoOp::End => waiting_for_begin += 1,
            UndoOp::Begin => {
                if waiting_for_begin > 0 {
                    waiting_for_begin -= 1;
                } else {
                    crate::editing::ring_bell();
                }
            }
            UndoOp::Insert { start, end } => {
                UNDOING.store(true, Ordering::Relaxed);
                let end = end.min(state::end());
                // SAFETY: the exported primitive only touches line state.
                unsafe { crate::line::rl_delete_text(start, end) };
                state::set_point(start);
                state::clamp_point();
                UNDOING.store(false, Ordering::Relaxed);
            }
            UndoOp::Delete { start, text } => {
                UNDOING.store(true, Ordering::Relaxed);
                state::set_point(start);
                state::clamp_point();
                crate::editing::insert_bytes(&text);
                UNDOING.store(false, Ordering::Relaxed);
            }
        }
        if waiting_for_begin == 0 {
            break;
        }
    }
    did
}

fn undo_command(count: c_int) {
    if count < 0 {
        return;
    }
    let mut remaining = count;
    while remaining > 0 {
        if do_undo() {
            remaining -= 1;
        } else {
            crate::editing::ring_bell();
            break;
        }
    }
}

// --- Vi state -------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
}

#[derive(Clone, Copy)]
struct MotionContext {
    op: Op,
    start: c_int,
    key: c_int,
    motion: c_int,
    count: c_int,
}

struct ViState {
    last_command: c_int,
    last_repeat: c_int,
    last_arg_sign: c_int,
    last_motion: c_int,
    last_key_before_insert: c_int,
    insert_active: bool,
    insert_buffer: Vec<u8>,
    doing_insert: bool,
    replace_count: c_int,
    last_replacement: Vec<u8>,
    marks: [c_int; 26],
    cs_dir: c_int,
    cs_orig_dir: c_int,
    last_search: Vec<u8>,
    search_pattern: Option<Vec<u8>>,
    search_pos: c_int,
    redo_active: bool,
}

static VI: Mutex<ViState> = Mutex::new(ViState {
    last_command: b'i' as c_int,
    last_repeat: 1,
    last_arg_sign: 1,
    last_motion: 0,
    last_key_before_insert: 0,
    insert_active: false,
    insert_buffer: Vec::new(),
    doing_insert: false,
    replace_count: 0,
    last_replacement: Vec::new(),
    marks: [-1; 26],
    cs_dir: 0,
    cs_orig_dir: 0,
    last_search: Vec::new(),
    search_pattern: None,
    search_pos: 0,
    redo_active: false,
});

static PENDING_MOTION: Mutex<Option<MotionContext>> = Mutex::new(None);

fn vi_state() -> MutexGuard<'static, ViState> {
    VI.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn motion_context() -> MutexGuard<'static, Option<MotionContext>> {
    PENDING_MOTION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn set_last(key: c_int, repeat: c_int, sign: c_int) {
    let mut vi = vi_state();
    vi.last_command = key;
    vi.last_repeat = repeat;
    vi.last_arg_sign = sign;
}

fn redoing() -> bool {
    vi_state().redo_active
}

pub(crate) fn initialize_line() {
    let mut vi = vi_state();
    vi.marks = [-1; 26];
    vi.cs_dir = 0;
    vi.cs_orig_dir = 0;
    vi.last_search.clear();
    vi.search_pattern = None;
    vi.search_pos = 0;
    vi.insert_active = false;
    vi.insert_buffer.clear();
    vi.doing_insert = false;
    vi.replace_count = 0;
    drop(vi);
    clear_undo();
    state::set_readline_state(state::readline_state() & !RL_STATE_VICMDONCE);
}

/// Record the self-inserted text of the current vi insert session so `.` can
/// replay `i`/`a`/`I`/`A`/`R` like GNU's `_rl_vi_save_insert`.
pub(crate) fn note_self_insert(key: c_int, count: c_int) {
    if count <= 0 || !(0..=0xff).contains(&key) {
        return;
    }
    let mut vi = vi_state();
    if !vi.insert_active || vi.redo_active {
        return;
    }
    vi.insert_buffer
        .extend(std::iter::repeat_n(key as u8, count as usize));
}

// --- Small line helpers ---------------------------------------------------

fn byte_at(position: c_int) -> u8 {
    crate::editing::byte_at(position).unwrap_or(0)
}

fn is_whitespace(byte: u8) -> bool {
    crate::editing::whitespace(byte)
}

fn whitespace_at(position: c_int) -> bool {
    is_whitespace(byte_at(position))
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn ident_at(position: c_int) -> bool {
    is_ident(byte_at(position))
}

fn is_punct(byte: u8) -> bool {
    byte.is_ascii_punctuation()
}

fn uppercase_p(key: c_int) -> bool {
    (b'A' as c_int..=b'Z' as c_int).contains(&key)
}

fn to_upper(key: c_int) -> u8 {
    if (0..=0xff).contains(&key) {
        (key as u8).to_ascii_uppercase()
    } else {
        0
    }
}

fn digit_p(key: c_int) -> bool {
    (b'0' as c_int..=b'9' as c_int).contains(&key)
}

fn digit_value(key: c_int) -> c_int {
    key - b'0' as c_int
}

fn advance_point() -> c_int {
    let old = state::point();
    if state::point() < state::end() {
        let bytes = crate::editing::line_slice();
        state::set_point(
            crate::editing::next_position(bytes, state::point().max(0) as usize, 1) as c_int,
        );
    }
    old
}

fn backup_point() -> c_int {
    let old = state::point();
    if state::point() > 0 {
        let bytes = crate::editing::line_slice();
        state::set_point(
            crate::editing::previous_position(bytes, state::point() as usize, 1) as c_int,
        );
    }
    old
}

fn backup() {
    backup_point();
}

fn advance_byte_position(position: c_int) -> c_int {
    let bytes = crate::editing::line_slice();
    crate::editing::next_position(bytes, position.max(0) as usize, 1) as c_int
}

/// GNU's `rl_read_key` checks `rl_pending_input` before it touches the input
/// stream.  This adapter stops at that seam: it consumes one pending key and
/// reports failure when the live dispatcher would have had to read a terminal.
fn read_key_seam() -> c_int {
    let pending = state::pending_input();
    if pending != 0 {
        state::set_pending_input(0);
        state::set_readline_state(state::readline_state() & !RL_STATE_INPUTPENDING);
        return pending;
    }
    -1
}

fn stuff_key(key: c_int) {
    if state::pending_input() == 0 {
        state::set_pending_input(key);
        state::set_readline_state(state::readline_state() | RL_STATE_INPUTPENDING);
    }
}

fn last_func_is(function: Command) -> bool {
    state::last_func().is_some_and(|last| std::ptr::fn_addr_eq(last, function))
}

// --- Mode commands --------------------------------------------------------

fn set_insert_mode(mode: c_int) {
    state::set_insert_mode(mode);
    if mode == RL_IM_OVERWRITE {
        state::set_readline_state(state::readline_state() | crate::editing::RL_STATE_OVERWRITE_BIT);
    } else {
        state::set_readline_state(
            state::readline_state() & !crate::editing::RL_STATE_OVERWRITE_BIT,
        );
    }
}

fn set_insertion_mode(key: c_int) -> c_int {
    crate::keymap::set_vi_insertion_keymap();
    set_insert_mode(RL_IM_INSERT);
    let open_group = {
        let mut vi = vi_state();
        vi.last_key_before_insert = key;
        if vi.insert_active {
            false
        } else {
            vi.insert_active = true;
            vi.insert_buffer.clear();
            true
        }
    };
    if open_group {
        begin_undo_group();
    }
    0
}

fn insertion_mode_body(_count: c_int, key: c_int) -> c_int {
    set_insertion_mode(key)
}

fn start_inserting(key: c_int, repeat: c_int, sign: c_int) {
    set_last(key, repeat, sign);
    set_insertion_mode(key);
}

fn insert_mode_body(_count: c_int, key: c_int) -> c_int {
    start_inserting(key, 1, state::argument_sign());
    0
}

fn editing_mode_body(_count: c_int, key: c_int) -> c_int {
    set_insert_mode(RL_IM_INSERT);
    crate::keymap::set_vi_mode();
    insert_mode_body(1, key);
    0
}

fn insert_beg_body(_count: c_int, key: c_int) -> c_int {
    crate::editing::beginning_of_line(1, key);
    insert_mode_body(1, key);
    0
}

fn append_mode_body(_count: c_int, key: c_int) -> c_int {
    advance_point();
    start_inserting(key, 1, state::argument_sign());
    0
}

fn append_eol_body(_count: c_int, key: c_int) -> c_int {
    crate::editing::end_of_line(1, key);
    append_mode_body(1, key);
    0
}

fn movement_mode_body(_count: c_int, key: c_int) -> c_int {
    if state::point() > 0 {
        crate::editing::backward_char(1, key);
    }
    crate::keymap::set_vi_movement_keymap();

    let (was_inserting, replace_session) = {
        let mut vi = vi_state();
        let active = vi.insert_active;
        let replace_session = vi.last_key_before_insert == b'R' as c_int;
        vi.insert_active = false;
        // GNU's `_rl_vi_done_inserting` clears this before command mode.
        vi.doing_insert = false;
        (active, replace_session)
    };
    if replace_session {
        vi_state().replace_count = 0;
    }
    if was_inserting {
        end_undo_group();
    }
    while undo_group_level() > 0 {
        end_undo_group();
    }

    // POSIX.2: everything up to the first command-mode entry is not undoable.
    if state::readline_state() & RL_STATE_VICMDONCE == 0 {
        clear_undo();
    }
    state::set_readline_state(state::readline_state() | RL_STATE_VICMDONCE);
    0
}

fn check_body() -> c_int {
    if state::point() != 0 && state::point() == state::end() {
        backup();
    }
    0
}

// --- Word motion ----------------------------------------------------------

fn fWord_body(count: c_int, _key: c_int) -> c_int {
    let mut remaining = count;
    while remaining > 0 {
        remaining -= 1;
        if state::point() >= state::end() - 1 {
            break;
        }
        while !whitespace_at(state::point()) && state::point() < state::end() {
            advance_point();
        }
        while whitespace_at(state::point()) && state::point() < state::end() {
            advance_point();
        }
    }
    0
}

fn bWord_body(count: c_int, _key: c_int) -> c_int {
    // Vi `B`: same walk, but a "word" is the whole non-whitespace run.
    let mut remaining = count.max(0);
    let mut point = state::point();
    while remaining > 0 && point > 0 {
        remaining -= 1;
        point -= 1;
        while point > 0 && whitespace_at(point) {
            point -= 1;
        }
        while point > 0 && !whitespace_at(point - 1) {
            point -= 1;
        }
        if point == 0 {
            break;
        }
    }
    state::set_point(point);
    0
}

fn eWord_body(count: c_int, _key: c_int) -> c_int {
    let mut remaining = count;
    while remaining > 0 {
        remaining -= 1;
        if state::point() >= state::end() - 1 {
            break;
        }
        if !whitespace_at(state::point()) {
            advance_point();
        }
        while state::point() < state::end() && whitespace_at(state::point()) {
            advance_point();
        }
        if state::point() != 0 && state::point() < state::end() {
            let mut opoint = state::point();
            while state::point() < state::end() && whitespace_at(state::point()) {
                opoint = advance_point();
            }
            while state::point() < state::end() && !whitespace_at(state::point()) {
                opoint = advance_point();
            }
            state::set_point(opoint);
        }
    }
    0
}

fn fword_body(count: c_int, _key: c_int) -> c_int {
    let mut remaining = count;
    while remaining > 0 {
        remaining -= 1;
        if state::point() >= state::end() - 1 {
            break;
        }
        if ident_at(state::point()) {
            while ident_at(state::point()) && state::point() < state::end() {
                advance_point();
            }
        } else {
            while !ident_at(state::point())
                && !whitespace_at(state::point())
                && state::point() < state::end()
            {
                advance_point();
            }
        }
        while whitespace_at(state::point()) && state::point() < state::end() {
            advance_point();
        }
    }
    0
}

fn bword_body(count: c_int, _key: c_int) -> c_int {
    // GNU vi_mode.c `b`/`B`: step back at least one character, skip the
    // whitespace run, then walk to the start of the word. Normal `b`
    // treats a punctuation run as its own word; `B` uses whitespace only.
    let mut remaining = count.max(0);
    let mut point = state::point();
    while remaining > 0 && point > 0 {
        remaining -= 1;
        point -= 1;
        while point > 0 && whitespace_at(point) {
            point -= 1;
        }
        if point == 0 {
            break;
        }
        if ident_at(point) {
            while point > 0 && !whitespace_at(point - 1) && ident_at(point - 1) {
                point -= 1;
            }
        } else {
            while point > 0 && !whitespace_at(point - 1) && !ident_at(point - 1) {
                point -= 1;
            }
        }
    }
    state::set_point(point);
    0
}

fn eword_body(count: c_int, _key: c_int) -> c_int {
    let mut remaining = count;
    while remaining > 0 {
        remaining -= 1;
        if state::point() >= state::end() - 1 {
            break;
        }
        if !whitespace_at(state::point()) {
            advance_point();
        }
        while state::point() < state::end() && whitespace_at(state::point()) {
            advance_point();
        }

        let mut opoint = state::point();
        if state::point() < state::end() {
            if ident_at(state::point()) {
                loop {
                    opoint = advance_point();
                    if !(state::point() < state::end() && ident_at(state::point())) {
                        break;
                    }
                }
            } else {
                loop {
                    opoint = advance_point();
                    if !(state::point() < state::end()
                        && !ident_at(state::point())
                        && !whitespace_at(state::point()))
                    {
                        break;
                    }
                }
            }
        }
        state::set_point(opoint);
    }
    0
}

fn next_word_body(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return prev_word_body(-count, key);
    }
    if state::point() >= state::end() - 1 {
        crate::editing::ring_bell();
        return 0;
    }
    if uppercase_p(key) {
        fWord_body(count, key);
    } else {
        fword_body(count, key);
    }
    0
}

fn prev_word_body(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return next_word_body(-count, key);
    }
    if state::point() == 0 {
        crate::editing::ring_bell();
        return 0;
    }
    if uppercase_p(key) {
        bWord_body(count, key);
    } else {
        bword_body(count, key);
    }
    0
}

fn end_word_body(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        crate::editing::ring_bell();
        return 1;
    }
    if uppercase_p(key) {
        eWord_body(count, key);
    } else {
        eword_body(count, key);
    }
    0
}

fn first_print_body(_count: c_int, key: c_int) -> c_int {
    back_to_indent_body(1, key)
}

fn back_to_indent_body(_count: c_int, _key: c_int) -> c_int {
    state::set_point(0);
    while state::point() < state::end() && whitespace_at(state::point()) {
        state::set_point(state::point() + 1);
    }
    0
}

fn column_body(count: c_int, _key: c_int) -> c_int {
    if count > state::end() {
        state::set_point(state::end());
    } else {
        state::set_point(0);
        let bytes = crate::editing::line_slice();
        let steps = count.saturating_sub(1).max(0);
        state::set_point(crate::editing::next_position(bytes, 0, steps) as c_int);
    }
    0
}

// --- Bracket match and character search ----------------------------------

fn bracktype(character: c_int) -> c_int {
    match character {
        x if x == b'(' as c_int => 1,
        x if x == b')' as c_int => -1,
        x if x == b'[' as c_int => 2,
        x if x == b']' as c_int => -2,
        x if x == b'{' as c_int => 3,
        x if x == b'}' as c_int => -3,
        _ => 0,
    }
}

fn match_body(_ignore: c_int, key: c_int) -> c_int {
    let saved = state::point();
    let mut brack = bracktype(byte_at(state::point()) as c_int);
    if brack == 0 {
        while bracktype(byte_at(state::point()) as c_int) == 0 && state::point() < state::end() - 1
        {
            crate::editing::forward_char(1, key);
        }
        brack = bracktype(byte_at(state::point()) as c_int);
        if brack <= 0 {
            state::set_point(saved);
            crate::editing::ring_bell();
            return 1;
        }
    }

    let mut pos = state::point();
    let mut count = 1;
    if brack < 0 {
        while count != 0 {
            let previous = pos;
            let bytes = crate::editing::line_slice();
            pos = crate::editing::previous_position(bytes, pos.max(0) as usize, 1) as c_int;
            if previous == pos {
                pos -= 1;
            }
            if pos >= 0 {
                let found = bracktype(byte_at(pos) as c_int);
                if found == -brack {
                    count -= 1;
                } else if found == brack {
                    count += 1;
                }
            } else {
                crate::editing::ring_bell();
                return 1;
            }
        }
    } else {
        while count != 0 {
            let bytes = crate::editing::line_slice();
            pos = crate::editing::next_position(bytes, pos.max(0) as usize, 1) as c_int;
            if pos < state::end() {
                let found = bracktype(byte_at(pos) as c_int);
                if found == -brack {
                    count -= 1;
                } else if found == brack {
                    count += 1;
                }
            } else {
                crate::editing::ring_bell();
                return 1;
            }
        }
    }
    state::set_point(pos);
    0
}

fn charsearch_command(character: c_int) -> bool {
    if !(0..=0xff).contains(&character) {
        return false;
    }
    matches!(character as u8, b'f' | b'F' | b't' | b'T' | b';' | b',')
}

fn char_search_internal(count: c_int, dir: c_int, target: u8) -> c_int {
    if dir == 0 {
        return 1;
    }
    let mut remaining = count;
    let mut pos = state::point();
    while remaining != 0 {
        if (dir < 0 && pos <= 0) || (dir > 0 && pos >= state::end()) {
            crate::editing::ring_bell();
            return 1;
        }
        let bytes = crate::editing::line_slice();
        pos = if dir < 0 {
            crate::editing::previous_position(bytes, pos.max(0) as usize, 1) as c_int
        } else {
            crate::editing::next_position(bytes, pos.max(0) as usize, 1) as c_int
        };

        loop {
            if pos >= 0 && pos < state::end() && byte_at(pos) == target {
                remaining -= 1;
                state::set_point(if dir < 0 {
                    if dir == BTO {
                        let bytes = crate::editing::line_slice();
                        crate::editing::next_position(bytes, pos.max(0) as usize, 1) as c_int
                    } else {
                        pos
                    }
                } else if dir == FTO {
                    let bytes = crate::editing::line_slice();
                    crate::editing::previous_position(bytes, pos.max(0) as usize, 1) as c_int
                } else {
                    pos
                });
                break;
            }

            let previous = pos;
            let bytes = crate::editing::line_slice();
            if dir < 0 {
                if pos == 0 {
                    pos = -1;
                    break;
                }
                pos = crate::editing::previous_position(bytes, pos.max(0) as usize, 1) as c_int;
            } else {
                if pos >= state::end() {
                    break;
                }
                pos = crate::editing::next_position(bytes, pos.max(0) as usize, 1) as c_int;
                if pos >= state::end() {
                    break;
                }
            }
            if pos == previous {
                break;
            }
        }
    }
    0
}

fn char_search_body(count: c_int, key: c_int) -> c_int {
    let (dir, mut target);
    {
        let vi = vi_state();
        if key == b';' as c_int || key == b',' as c_int {
            if vi.cs_orig_dir == 0 || vi.last_search.is_empty() {
                return 1;
            }
            let dir = if key == b';' as c_int {
                vi.cs_orig_dir
            } else {
                -vi.cs_orig_dir
            };
            target = vi.last_search[0];
            drop(vi);
            vi_state().cs_dir = dir;
            return char_search_internal(count, dir, target);
        }
        dir = match key as u8 {
            b't' => FTO,
            b'T' => BTO,
            b'f' => FFIND,
            b'F' => BFIND,
            _ => vi.cs_dir,
        };
        target = vi.last_search.first().copied().unwrap_or(0);
    }

    if !redoing() {
        let character = read_key_seam();
        if character < 0 {
            return -1;
        }
        if !(0..=0xff).contains(&character) {
            return -1;
        }
        target = character as u8;
        let mut vi = vi_state();
        vi.last_search = vec![target];
        vi.cs_orig_dir = dir;
        vi.cs_dir = dir;
    } else {
        let mut vi = vi_state();
        vi.cs_dir = dir;
    }
    char_search_internal(count, dir, target)
}

// --- Marks ---------------------------------------------------------------

fn set_mark_body(_count: c_int, _key: c_int) -> c_int {
    let character = read_key_seam();
    if !(b'a' as c_int..=b'z' as c_int).contains(&character) {
        crate::editing::ring_bell();
        return 1;
    }
    vi_state().marks[(character - b'a' as c_int) as usize] = state::point();
    0
}

fn goto_mark_body(_count: c_int, _key: c_int) -> c_int {
    let character = read_key_seam();
    if character == b'`' as c_int {
        state::set_point(state::mark());
        state::clamp_point();
        state::clamp_mark();
        return 0;
    }
    if !(b'a' as c_int..=b'z' as c_int).contains(&character) {
        crate::editing::ring_bell();
        return 1;
    }
    let position = vi_state().marks[(character - b'a' as c_int) as usize];
    if position == -1 {
        crate::editing::ring_bell();
        return 1;
    }
    state::set_point(position);
    state::clamp_point();
    state::clamp_mark();
    0
}

// --- Operators and their motions -----------------------------------------

fn is_motion(character: c_int) -> bool {
    if !(0..=0xff).contains(&character) {
        return false;
    }
    matches!(
        character as u8,
        b' ' | b'h'
            | b'l'
            | b'^'
            | b'$'
            | b'0'
            | b'f'
            | b't'
            | b'F'
            | b'T'
            | b';'
            | b','
            | b'%'
            | b'w'
            | b'b'
            | b'e'
            | b'W'
            | b'B'
            | b'E'
            | b'|'
            | b'`'
    )
}

fn motion_in_set(set: &[u8], motion: c_int) -> bool {
    (0..=0xff).contains(&motion) && set.contains(&(motion as u8))
}

fn apply_motion(motion: c_int, count: c_int) -> c_int {
    if !(0..=0xff).contains(&motion) {
        return 1;
    }
    match motion as u8 {
        b' ' | b'l' => crate::editing::forward_char(count, motion),
        b'h' => crate::editing::backward_char(count, motion),
        b'^' => back_to_indent_body(count, motion),
        b'$' => crate::editing::end_of_line(1, motion),
        b'0' => crate::editing::beginning_of_line(1, motion),
        b'f' | b'F' | b't' | b'T' | b';' | b',' => char_search_body(count, motion),
        b'%' => match_body(count, motion),
        b'w' | b'W' => next_word_body(count, motion),
        b'b' | b'B' => prev_word_body(count, motion),
        b'e' | b'E' => end_word_body(count, motion),
        b'|' => column_body(count, motion),
        b'`' => goto_mark_body(count, motion),
        _ => 1,
    }
}

fn operator_dispatch(context: MotionContext) -> c_int {
    let motion = context.motion;
    match context.op {
        Op::Delete => {
            if !motion_in_set(b" l|h^0bBFT`", motion)
                && state::point() >= context.start
                && state::mark() < state::end()
            {
                state::set_mark(advance_byte_position(state::mark()));
            }
            crate::editing::kill_text(state::point(), state::mark());
            0
        }
        Op::Change => {
            if !motion_in_set(b" l|hwW^0bBFT`", motion)
                && state::point() >= context.start
                && state::mark() < state::end()
            {
                state::set_mark(advance_byte_position(state::mark()));
            }
            if to_upper(motion) == b'W' && state::point() < context.start {
                state::set_point(context.start);
            }

            if redoing() {
                let buffer = vi_state().insert_buffer.clone();
                if !buffer.is_empty() {
                    begin_undo_group();
                }
                crate::editing::delete_range(state::point(), state::mark());
                if !buffer.is_empty() {
                    crate::editing::insert_bytes(&buffer);
                    end_undo_group();
                }
            } else {
                begin_undo_group();
                crate::editing::kill_text(state::point(), state::mark());
                if !uppercase_p(context.key) {
                    vi_state().doing_insert = true;
                }
                start_inserting(context.key, context.count, state::argument_sign());
            }
            0
        }
        Op::Yank => {
            if !motion_in_set(b" l|h^0%bBFT`", motion)
                && state::point() >= context.start
                && state::mark() < state::end()
            {
                state::set_mark(advance_byte_position(state::mark()));
            }
            begin_undo_group();
            crate::editing::kill_text(state::point(), state::mark());
            end_undo_group();
            do_undo();
            state::set_point(context.start);
            state::clamp_point();
            0
        }
    }
}

fn domove_motion(context: MotionContext) -> c_int {
    let motion = context.motion;
    vi_state().last_motion = motion;

    // GNU appends a temporary blank so motions work at the end of the line.
    let saved_end = state::end();
    state::ensure_line_capacity(saved_end as usize + 2);
    let buffer = state::line_buffer();
    if !buffer.is_null() {
        // SAFETY: `ensure_line_capacity` reserved room for the blank and NUL.
        unsafe {
            *buffer.add(saved_end as usize) = b' ' as c_char;
            *buffer.add(saved_end as usize + 1) = 0;
        }
    }
    state::set_end(saved_end + 1);
    let original_point = state::point();

    let result = apply_motion(motion, context.count);

    state::set_end(saved_end);
    let buffer = state::line_buffer();
    if !buffer.is_null() {
        // SAFETY: the buffer still owns `saved_end` bytes after restoration.
        unsafe { *buffer.add(saved_end as usize) = 0 };
    }
    state::clamp_point();

    let failed = result != 0 && state::point() == original_point && charsearch_command(motion);

    if state::mark() == state::point() {
        if matches!(to_upper(context.key), b'C' | b'D' | b'Y') && is_motion(motion) && !failed {
            return operator_dispatch(context);
        }
        return -1;
    }

    if to_upper(motion) == b'W'
        && state::point() < state::end()
        && state::point() > state::mark()
        && !whitespace_at(state::point())
    {
        state::set_point(state::point() - 1);
    }

    if context.key == b'c' as c_int && state::point() >= state::mark() && to_upper(motion) == b'W' {
        while state::point() > state::mark() && whitespace_at(state::point()) {
            state::set_point(state::point() - 1);
        }
        if state::point() == state::mark()
            || (state::point() >= 0
                && state::point() < state::end() - 1
                && !whitespace_at(state::point()))
        {
            advance_point();
        }
    }

    if state::mark() < state::point() {
        let point = state::point();
        state::set_point(state::mark());
        state::set_mark(point);
    }

    operator_dispatch(context)
}

fn domove_read(context: MotionContext, character: c_int) -> c_int {
    if is_motion(character) {
        let mut context = context;
        context.motion = character;
        return domove_motion(context);
    }
    if context.key == character
        && matches!(character as u8, b'd' | b'y' | b'c')
        && (0..=0xff).contains(&character)
    {
        state::set_mark(state::end());
        crate::editing::beginning_of_line(1, character);
        vi_state().last_motion = character;
        let mut context = context;
        context.motion = character;
        return operator_dispatch(context);
    }
    1
}

fn operator_body(op: Op, count: c_int, key: c_int) -> c_int {
    let context = MotionContext {
        op,
        start: state::point(),
        key,
        motion: -1,
        count,
    };
    // GNU's `rl_vi_delete_to` / `rl_vi_change_to` / `rl_vi_yank_to` seed the
    // mark with the operator start before reading the motion.
    state::set_mark(state::point());
    *motion_context() = Some(context);

    let result = if uppercase_p(key) {
        let mut context = context;
        context.motion = b'$' as c_int;
        domove_motion(context)
    } else {
        let (redo, last_motion) = {
            let vi = vi_state();
            (vi.redo_active, vi.last_motion)
        };
        if redo && last_motion != key {
            let mut context = context;
            context.motion = last_motion;
            domove_motion(context)
        } else if redo {
            let mut context = context;
            context.motion = last_motion;
            state::set_mark(state::end());
            crate::editing::beginning_of_line(1, key);
            operator_dispatch(context)
        } else {
            let mut ignored = 0;
            // SAFETY: the exported entry point only reads the pending-key seam.
            unsafe { rl_vi_domove(key, &mut ignored) }
        }
    };

    *motion_context() = None;

    if result < 0 {
        crate::editing::ring_bell();
        -1
    } else {
        result
    }
}

fn delete_to_body(count: c_int, key: c_int) -> c_int {
    operator_body(Op::Delete, count, key)
}

fn change_to_body(count: c_int, key: c_int) -> c_int {
    operator_body(Op::Change, count, key)
}

fn yank_to_body(count: c_int, key: c_int) -> c_int {
    operator_body(Op::Yank, count, key)
}

// --- Delete, rubout, and change ------------------------------------------

fn delete_body(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return rubout_body(-count, key);
    }
    if state::end() == 0 {
        crate::editing::ring_bell();
        return 1;
    }

    let bytes = crate::editing::line_slice();
    let mut end =
        crate::editing::next_position(bytes, state::point().max(0) as usize, count.max(0)) as c_int;
    if end >= state::end() {
        end = state::end();
    }

    crate::editing::kill_text(state::point(), end);
    if state::point() > 0 && state::point() == state::end() {
        crate::editing::backward_char(1, key);
    }
    0
}

fn rubout_body(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return delete_body(-count, key);
    }
    if state::point() == 0 {
        crate::editing::ring_bell();
        return 1;
    }

    let original = state::point();
    let bytes = crate::editing::line_slice();
    let position =
        crate::editing::previous_position(bytes, original as usize, count.max(0)) as c_int;
    state::set_point(position.max(0));
    crate::editing::kill_text(state::point(), original);
    0
}

fn unix_word_boundary(byte: u8) -> bool {
    is_whitespace(byte) || is_punct(byte)
}

fn unix_word_rubout_body(count: c_int, _key: c_int) -> c_int {
    if state::point() == 0 {
        crate::editing::ring_bell();
        return 0;
    }

    let original = state::point();
    let mut remaining = if count <= 0 { 1 } else { count };
    while remaining > 0 {
        remaining -= 1;

        if state::point() > 0 && byte_at(state::point()) == 0 && whitespace_at(state::point() - 1) {
            loop {
                state::set_point(state::point() - 1);
                if !(state::point() > 0 && whitespace_at(state::point())) {
                    break;
                }
            }
        }

        if state::point() > 0
            && !unix_word_boundary(byte_at(state::point()))
            && unix_word_boundary(byte_at(state::point() - 1))
        {
            state::set_point(state::point() - 1);
        }

        if state::point() > 0 && unix_word_boundary(byte_at(state::point())) {
            while state::point() > 0 && unix_word_boundary(byte_at(state::point() - 1)) {
                state::set_point(state::point() - 1);
            }
        } else if state::point() > 0 && !unix_word_boundary(byte_at(state::point())) {
            while state::point() > 0 && !unix_word_boundary(byte_at(state::point() - 1)) {
                backup_point();
            }
        }
    }

    crate::editing::kill_text(original, state::point());
    0
}

fn change_case_body(mut count: c_int, _key: c_int) -> c_int {
    if state::point() >= state::end() {
        return 0;
    }

    while count > 0 && state::point() < state::end() {
        count -= 1;
        let byte = byte_at(state::point());
        let converted = if byte.is_ascii_uppercase() {
            byte.to_ascii_lowercase()
        } else if byte.is_ascii_lowercase() {
            byte.to_ascii_uppercase()
        } else {
            crate::editing::forward_char(1, byte as c_int);
            continue;
        };

        let point = state::point();
        begin_undo_group();
        delete_body(1, converted as c_int);
        if state::point() < point {
            state::set_point(state::point() + 1);
        }
        crate::editing::insert_bytes(&[converted]);
        end_undo_group();
        check_body();
    }
    0
}

fn change_char_body(count: c_int, _key: c_int) -> c_int {
    let replacement = if redoing() {
        vi_state().last_replacement.clone()
    } else {
        let character = read_key_seam();
        if character < 0 || !(0..=0xff).contains(&character) {
            return -1;
        }
        let bytes = vec![character as u8];
        vi_state().last_replacement = bytes.clone();
        bytes
    };

    let Some(&character) = replacement.first() else {
        return -1;
    };
    if character as c_int == ESC || character as c_int == CTRL_C {
        return -1;
    }

    begin_undo_group();
    let mut remaining = count;
    while remaining > 0 && state::point() < state::end() {
        remaining -= 1;
        let point = state::point();
        delete_body(1, character as c_int);
        if state::point() < point {
            advance_point();
        }
        crate::editing::insert_bytes(&[character]);
    }
    crate::editing::backward_char(1, character as c_int);
    end_undo_group();
    0
}

fn subst_body(count: c_int, key: c_int) -> c_int {
    if !redoing() {
        stuff_key(if key == b'S' as c_int {
            b'c' as c_int
        } else {
            b'l' as c_int
        });
    }
    change_to_body(count, b'c' as c_int)
}

// --- Replace / overstrike -------------------------------------------------

fn overwrite_char(count: c_int, key: c_int) -> c_int {
    if count <= 0 || !(0..=0xff).contains(&key) {
        return 0;
    }
    begin_undo_group();
    for _ in 0..count {
        crate::editing::insert_bytes(&[key as u8]);
        if state::point() < state::end() {
            let bytes = crate::editing::line_slice();
            let next = crate::editing::next_position(bytes, state::point() as usize, 1) as c_int;
            crate::editing::delete_range(state::point(), next);
        }
    }
    end_undo_group();
    0
}

pub(crate) fn overstrike(count: c_int, key: c_int) -> c_int {
    overstrike_body(count, key)
}

pub(crate) fn overstrike_delete(count: c_int, key: c_int) -> c_int {
    overstrike_delete_body(count, key)
}

fn overstrike_body(count: c_int, key: c_int) -> c_int {
    {
        let mut vi = vi_state();
        if !vi.doing_insert {
            vi.doing_insert = true;
            drop(vi);
            begin_undo_group();
        }
    }

    if count > 0 {
        if overwrite_char(count, key) != 0 {
            return 1;
        }
        vi_state().replace_count += count;
        note_self_insert(key, count);
    }
    0
}

fn overstrike_delete_body(count: c_int, key: c_int) -> c_int {
    let mut index = 0;
    while index < count {
        index += 1;
        if vi_state().replace_count == 0 {
            crate::editing::ring_bell();
            break;
        }
        let saved = state::point();
        if do_undo() {
            vi_state().replace_count -= 1;
        }
        if state::point() == saved {
            crate::editing::backward_char(1, key);
        }
    }

    if vi_state().replace_count == 0 && vi_state().doing_insert {
        end_undo_group();
        do_undo();
        vi_state().doing_insert = false;
    }
    0
}

fn replace_body(_count: c_int, key: c_int) -> c_int {
    vi_state().replace_count = 0;
    start_inserting(key, 1, state::argument_sign());
    vi_state().last_key_before_insert = b'R' as c_int;
    // GNU builds a private replace keymap whose printable slots call
    // `rl_vi_overstrike` and whose RUBOUT slot calls
    // `rl_vi_overstrike_delete`.  The standalone editing commands consult the
    // same map (see `editing::insert` / `backward_delete_char`), so `R` keeps
    // GNU's insert-mode flag while still replacing text.
    crate::keymap::set_vi_replace_keymap();
    0
}

// --- Put, yank, and kill-ring popping ------------------------------------

fn put_body(count: c_int, key: c_int) -> c_int {
    if !uppercase_p(key) && state::point() < state::end() {
        state::set_point(advance_byte_position(state::point()));
        state::clamp_point();
    }
    let mut remaining = count.max(0);
    while remaining > 0 {
        crate::editing::yank(1, key);
        remaining -= 1;
    }
    crate::editing::backward_char(1, key);
    0
}

fn yank_pop_body(_count: c_int, _key: c_int) -> c_int {
    if !(last_func_is(rl_vi_yank_pop) || last_func_is(rl_vi_put))
        || crate::editing::kill_ring_current().is_none()
    {
        return crate::editing::abort_without_loop();
    }
    let Some(text) = crate::editing::kill_ring_current() else {
        return crate::editing::abort_without_loop();
    };

    let length = text.len() as c_int;
    let original = state::point();
    let start = original - length + 1;
    if start < 0 || start + length > state::end() {
        return crate::editing::abort_without_loop();
    }

    let bytes = crate::editing::line_slice();
    if bytes.get(start as usize..(start + length) as usize) != Some(text.as_slice()) {
        return crate::editing::abort_without_loop();
    }

    crate::editing::delete_range(start, start + length);
    state::set_point(original - length);
    crate::editing::kill_ring_rotate_back();
    put_body(1, b'p' as c_int);
    0
}

fn yank_arg_body(count: c_int, key: c_int) -> c_int {
    let wanted = if state::explicit_arg() != 0 {
        count - 1
    } else {
        b'$' as c_int
    };

    let saved = unsafe { crate::history::where_history() };
    let entry = unsafe { crate::history::previous_history() };
    unsafe {
        crate::history::history_set_pos(saved);
    }
    if entry.is_null() {
        crate::editing::ring_bell();
        return 1;
    }

    // SAFETY: `entry` points at a history entry owned by the history store.
    let line = unsafe { (*entry).line };
    let argument = unsafe { crate::history::history_arg_extract(wanted, wanted, line) };
    if argument.is_null() {
        crate::editing::ring_bell();
        return 1;
    }
    // SAFETY: `argument` is a NUL-terminated malloc allocation.
    let bytes = unsafe { CStr::from_ptr(argument) }.to_bytes().to_vec();
    unsafe { libc::free(argument as *mut libc::c_void) };
    if bytes.is_empty() {
        crate::editing::ring_bell();
        return 1;
    }

    begin_undo_group();
    state::set_mark(state::point());
    if crate::keymap::vi_movement_keymap_active() {
        append_mode_body(1, key);
        crate::editing::insert_bytes(b" ");
    }
    crate::editing::insert_bytes(&bytes);
    end_undo_group();
    0
}

// --- Tilde expansion ------------------------------------------------------

fn expand_tilde(word: &[u8]) -> Option<Vec<u8>> {
    let rest_start = word.iter().position(|byte| *byte == b'/');
    let user = match rest_start {
        Some(index) => &word[1..index],
        None => &word[1..],
    };
    let rest = match rest_start {
        Some(index) => &word[index..],
        None => &[][..],
    };

    let home = if user.is_empty() {
        std::env::var_os("HOME").map(|value| value.as_encoded_bytes().to_vec())?
    } else {
        crate::misc_text::user_home(user)?
    };

    let mut expanded = home;
    expanded.extend_from_slice(rest);
    Some(expanded)
}

fn tilde_expand_body(_ignore: c_int, key: c_int) -> c_int {
    let point = state::point();
    let end = state::end();
    let line = crate::editing::line_slice().to_vec();

    let mut start = point - 1;
    if start >= 0 && line.get(start as usize).copied() != Some(b'~') {
        while start >= 0 && !is_whitespace(line.get(start as usize).copied().unwrap_or(0)) {
            start -= 1;
        }
        start += 1;
    } else if start < 0 {
        start = 0;
    }

    let mut stop = start;
    loop {
        stop += 1;
        if !(stop < end && !is_whitespace(line.get(stop as usize).copied().unwrap_or(0))) {
            break;
        }
    }
    if stop >= end || is_whitespace(line.get(stop as usize).copied().unwrap_or(0)) {
        stop -= 1;
    }

    if start >= 0 && line.get(start as usize).copied() == Some(b'~') && stop >= start {
        let word = &line[start as usize..=(stop as usize).min(line.len().saturating_sub(1))];
        if let Some(expanded) = expand_tilde(word) {
            crate::editing::delete_range(start, stop + 1);
            state::set_point(start);
            crate::editing::insert_bytes(&expanded);
        }
    }

    start_inserting(key, 1, state::argument_sign());
    0
}

// --- History navigation and search ---------------------------------------

unsafe fn history_entry_line(entry: *mut crate::history::HIST_ENTRY) -> Vec<u8> {
    if entry.is_null() || (*entry).line.is_null() {
        return Vec::new();
    }
    CStr::from_ptr((*entry).line).to_bytes().to_vec()
}

unsafe fn set_line_from_history(entry: *mut crate::history::HIST_ENTRY) {
    let line = history_entry_line(entry);
    state::set_line_bytes(&line);
    // GNU's `make_history_line_current` drops the old undo list before
    // `rl_replace_from_history` puts vi command mode at the start of the line
    // with the whole line marked.
    clear_undo();
    state::set_point(0);
    state::set_mark(state::end());
}

fn fetch_history_body(count: c_int, key: c_int) -> c_int {
    if state::explicit_arg() != 0 {
        // SAFETY: the history entry points are only read.
        let nhist = unsafe { crate::history::history_base + crate::history::where_history() };
        let wanted = if count >= 0 { nhist - count } else { -count };
        if wanted <= 0 || wanted >= nhist {
            // Vi's `G` does not replace the line with an out-of-range entry.
            crate::editing::ring_bell();
        } else {
            // GNU routes vi `G` through rl_get_previous_history, so the saved
            // current line and the emacs/vi point rules stay shared with the
            // history-navigation commands.
            // SAFETY: same history store, no ownership transfer.
            unsafe {
                crate::history_nav::rl_get_previous_history(wanted, key);
            }
        }
    } else {
        // SAFETY: same history store, no ownership transfer.
        let offset = unsafe { crate::history::where_history() };
        unsafe {
            crate::history_nav::rl_get_previous_history(1 + offset, 0);
        }
    }
    0
}

fn pattern_matches(line: &[u8], pattern: &[u8]) -> bool {
    let (anchored, pattern) = if pattern.first() == Some(&b'^') {
        (true, &pattern[1..])
    } else {
        (false, pattern)
    };
    if pattern.is_empty() {
        return true;
    }
    if !pattern.iter().any(|byte| matches!(byte, b'*' | b'?')) {
        if anchored {
            return line.starts_with(pattern);
        }
        return line.windows(pattern.len()).any(|window| window == pattern);
    }

    fn glob(line: &[u8], pattern: &[u8]) -> bool {
        if pattern.is_empty() {
            return line.is_empty();
        }
        match pattern[0] {
            b'*' => {
                if glob(line, &pattern[1..]) {
                    return true;
                }
                !line.is_empty() && glob(&line[1..], pattern)
            }
            b'?' => !line.is_empty() && glob(&line[1..], &pattern[1..]),
            byte => !line.is_empty() && line[0] == byte && glob(&line[1..], &pattern[1..]),
        }
    }
    if anchored {
        glob(line, pattern)
    } else {
        (0..=line.len()).any(|index| glob(&line[index..], pattern))
    }
}

fn do_search(pattern: &[u8], direction: c_int) -> c_int {
    let length = unsafe { crate::history::history_length };
    let mut position = vi_state().search_pos + direction;
    if position < 0 || position >= length {
        crate::editing::ring_bell();
        state::set_point(0);
        return 0;
    }

    loop {
        if position < 0 || position >= length {
            crate::editing::ring_bell();
            state::set_point(0);
            return 0;
        }
        // SAFETY: `history_get` clamps to the live list; the returned entry is
        // borrowed only for this iteration.
        let entry = unsafe { crate::history::history_get(crate::history::history_base + position) };
        if !entry.is_null() {
            let line = unsafe { history_entry_line(entry) };
            if pattern_matches(&line, pattern) {
                unsafe {
                    crate::history::history_set_pos(position);
                    set_line_from_history(entry);
                }
                state::set_point(0);
                state::set_mark(state::end());
                vi_state().search_pos = position;
                return 1;
            }
        }
        position += direction;
    }
}

fn take_search_pattern() -> Option<Vec<u8>> {
    let pending = state::pending_input();
    if pending > 0 && pending <= 0xff {
        state::set_pending_input(0);
        state::set_readline_state(state::readline_state() & !RL_STATE_INPUTPENDING);
        Some(vec![pending as u8])
    } else {
        None
    }
}

fn search_body(_count: c_int, key: c_int) -> c_int {
    let direction = match key as u8 {
        b'/' => -1,
        b'?' => 1,
        _ => {
            crate::editing::ring_bell();
            return 0;
        }
    };

    let Some(pattern) = take_search_pattern() else {
        // The live dispatcher reads the search string here; record the seam by
        // ringing and leaving the saved search untouched.
        crate::editing::ring_bell();
        return 0;
    };

    let start = unsafe { crate::history::where_history() };
    {
        let mut vi = vi_state();
        vi.search_pattern = Some(pattern.clone());
        vi.search_pos = start;
    }
    do_search(&pattern, direction);
    0
}

fn search_again_body(_count: c_int, key: c_int) -> c_int {
    let direction = match key as u8 {
        b'n' => -1,
        b'N' => 1,
        _ => return 0,
    };
    let Some(pattern) = vi_state().search_pattern.clone() else {
        crate::editing::ring_bell();
        return 1;
    };
    do_search(&pattern, direction);
    0
}

fn eof_maybe_body(_count: c_int, _key: c_int) -> c_int {
    crate::editing::accept_line(1, b'\n' as c_int)
}

fn arg_digit_body(count: c_int, key: c_int) -> c_int {
    if key == b'0' as c_int && state::numeric_arg() == 1 && state::explicit_arg() == 0 {
        crate::editing::beginning_of_line(1, key)
    } else {
        crate::editing::digit_argument(count, key)
    }
}

// --- Redo ----------------------------------------------------------------

fn stuff_insert(count: c_int) {
    let buffer = vi_state().insert_buffer.clone();
    if buffer.is_empty() {
        return;
    }
    begin_undo_group();
    let mut remaining = count.max(0);
    while remaining > 0 {
        crate::editing::insert_bytes(&buffer);
        remaining -= 1;
    }
    end_undo_group();
}

fn replace_insert(count: c_int) {
    let buffer = vi_state().insert_buffer.clone();
    if buffer.is_empty() {
        return;
    }
    begin_undo_group();
    let mut remaining = count.max(0);
    while remaining > 0 {
        let start = state::point();
        let stop = (start + buffer.len() as c_int).min(state::end());
        crate::editing::delete_range(start, stop);
        state::set_point(start);
        crate::editing::insert_bytes(&buffer);
        remaining -= 1;
    }
    end_undo_group();
}

fn redo_insert_special(last: c_int, count: c_int) -> bool {
    if vi_state().insert_buffer.is_empty() {
        return false;
    }
    match last as u8 {
        b'i' => {
            stuff_insert(count);
            backup();
        }
        b'R' => {
            replace_insert(count);
            backup();
        }
        b'I' => {
            crate::editing::beginning_of_line(1, last);
            stuff_insert(count);
            backup();
        }
        b'a' => {
            advance_point();
            stuff_insert(count);
            backup();
        }
        b'A' => {
            crate::editing::end_of_line(1, last);
            stuff_insert(count);
            backup();
        }
        _ => return false,
    }
    true
}

fn dispatch_last(key: c_int, _count: c_int) -> c_int {
    vi_state().redo_active = true;
    let result = match key as u8 {
        b'x' => delete_body(state::numeric_arg(), key),
        b'X' => rubout_body(state::numeric_arg(), key),
        b's' | b'S' => subst_body(state::numeric_arg(), key),
        b'D' | b'd' => delete_to_body(state::numeric_arg(), key),
        b'C' | b'c' => change_to_body(state::numeric_arg(), key),
        b'P' | b'p' => put_body(state::numeric_arg(), key),
        b'Y' | b'y' => yank_to_body(state::numeric_arg(), key),
        b'R' => replace_body(state::numeric_arg(), key),
        b'r' => change_char_body(state::numeric_arg(), key),
        b'~' => change_case_body(state::numeric_arg(), key),
        b'.' => {
            crate::editing::ring_bell();
            0
        }
        _ => 1,
    };
    vi_state().redo_active = false;
    result
}

fn redo_body(count: c_int, _key: c_int) -> c_int {
    if state::explicit_arg() == 0 {
        let (repeat, sign) = {
            let vi = vi_state();
            (vi.last_repeat, vi.last_arg_sign)
        };
        state::set_numeric_arg(repeat);
        state::set_argument_sign(sign);
    }

    let last = vi_state().last_command;
    if redo_insert_special(last, count) {
        return 0;
    }
    if last == b'.' as c_int && crate::keymap::vi_movement_keymap_active() {
        crate::editing::ring_bell();
        return 0;
    }
    dispatch_last(last, count)
}

// --- Exported GNU ABI -----------------------------------------------------

const VI_TEXTMOD: &[u8] = b"_*\\AaIiCcDdPpYyRrSsXx~";

fn record_dispatch(key: c_int, count: c_int) {
    if !crate::keymap::vi_movement_keymap_active() {
        return;
    }
    if !(0..=0xff).contains(&key) || !VI_TEXTMOD.contains(&(key as u8)) {
        return;
    }
    // GNU's dispatcher records this before the command body runs.
    set_last(key, count, state::argument_sign());
}

macro_rules! export_vi_command {
    ($export:ident => $body:path) => {
        #[no_mangle]
        pub unsafe extern "C" fn $export(count: c_int, key: c_int) -> c_int {
            let previous = crate::editing::begin_command();
            record_dispatch(key, count);
            let result = $body(count, key);
            crate::editing::end_command(previous, false);
            state::set_last_func(Some($export as Command));
            result
        }
    };
}

export_vi_command!(rl_vi_editing_mode => editing_mode_body);
export_vi_command!(rl_vi_insertion_mode => insertion_mode_body);
export_vi_command!(rl_vi_insert_mode => insert_mode_body);
export_vi_command!(rl_vi_movement_mode => movement_mode_body);
export_vi_command!(rl_vi_append_eol => append_eol_body);
export_vi_command!(rl_vi_append_mode => append_mode_body);
export_vi_command!(rl_vi_insert_beg => insert_beg_body);
export_vi_command!(rl_vi_fword => fword_body);
export_vi_command!(rl_vi_bword => bword_body);
export_vi_command!(rl_vi_end_word => end_word_body);
export_vi_command!(rl_vi_eword => eword_body);
export_vi_command!(rl_vi_bWord => bWord_body);
export_vi_command!(rl_vi_fWord => fWord_body);
export_vi_command!(rl_vi_eWord => eWord_body);
export_vi_command!(rl_vi_next_word => next_word_body);
export_vi_command!(rl_vi_prev_word => prev_word_body);
export_vi_command!(rl_vi_column => column_body);
export_vi_command!(rl_vi_first_print => first_print_body);
export_vi_command!(rl_vi_back_to_indent => back_to_indent_body);
export_vi_command!(rl_vi_match => match_body);
export_vi_command!(rl_vi_char_search => char_search_body);
export_vi_command!(rl_vi_goto_mark => goto_mark_body);
export_vi_command!(rl_vi_set_mark => set_mark_body);
export_vi_command!(rl_vi_delete => delete_body);
export_vi_command!(rl_vi_delete_to => delete_to_body);
export_vi_command!(rl_vi_change_to => change_to_body);
export_vi_command!(rl_vi_change_char => change_char_body);
export_vi_command!(rl_vi_change_case => change_case_body);
export_vi_command!(rl_vi_subst => subst_body);
export_vi_command!(rl_vi_replace => replace_body);
export_vi_command!(rl_vi_overstrike => overstrike_body);
export_vi_command!(rl_vi_overstrike_delete => overstrike_delete_body);
export_vi_command!(rl_vi_rubout => rubout_body);
export_vi_command!(rl_vi_unix_word_rubout => unix_word_rubout_body);
export_vi_command!(rl_vi_tilde_expand => tilde_expand_body);
export_vi_command!(rl_vi_put => put_body);
export_vi_command!(rl_vi_yank_to => yank_to_body);
export_vi_command!(rl_vi_yank_arg => yank_arg_body);
export_vi_command!(rl_vi_yank_pop => yank_pop_body);
export_vi_command!(rl_vi_undo => undo_body);
export_vi_command!(rl_vi_redo => redo_body);
export_vi_command!(rl_vi_fetch_history => fetch_history_body);
export_vi_command!(rl_vi_search => search_body);
export_vi_command!(rl_vi_search_again => search_again_body);
export_vi_command!(rl_vi_eof_maybe => eof_maybe_body);

fn undo_body(count: c_int, _key: c_int) -> c_int {
    undo_command(count);
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_vi_arg_digit(count: c_int, key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = arg_digit_body(count, key);
    // A numeric argument is a prefix command: keep it for the next command.
    crate::editing::end_command(previous, true);
    result
}

#[no_mangle]
pub unsafe extern "C" fn rl_vi_start_inserting(key: c_int, repeat: c_int, sign: c_int) {
    start_inserting(key, repeat, sign);
}

#[no_mangle]
pub unsafe extern "C" fn rl_vi_check() -> c_int {
    check_body()
}

#[no_mangle]
pub unsafe extern "C" fn rl_vi_bracktype(character: c_int) -> c_int {
    bracktype(character)
}

#[no_mangle]
pub unsafe extern "C" fn rl_vi_domove(x: c_int, ignore: *mut c_int) -> c_int {
    let _ = x;
    let Some(mut context) = *motion_context() else {
        let character = read_key_seam();
        if !ignore.is_null() {
            *ignore = character;
        }
        return if character < 0 { -1 } else { 1 };
    };

    let character = read_key_seam();
    if !ignore.is_null() {
        *ignore = character;
    }
    if character < 0 {
        return -1;
    }

    let mut motion = character;
    let mut multiplier = 1;
    if digit_p(character) {
        let mut value = digit_value(character);
        loop {
            let next = read_key_seam();
            if next < 0 {
                return -1;
            }
            if digit_p(next) {
                value = value.saturating_mul(10).saturating_add(digit_value(next));
            } else {
                motion = next;
                break;
            }
        }
        multiplier = value.max(1);
    }

    context.count = context.count.saturating_mul(multiplier);
    context.motion = motion;
    domove_read(context, motion)
}
