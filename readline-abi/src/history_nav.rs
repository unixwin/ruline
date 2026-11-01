//! History navigation, incremental search, and non-incremental search
//! commands from GNU readline's `misc.c`, `isearch.c`, and `search.c`.
//!
//! The standalone library has no terminal dispatch loop, so commands that GNU
//! implements as a read loop are written as explicit state machines.  The next
//! key comes from the exported `rl_pending_input` slot: a command consumes one
//! queued byte per invocation and either keeps its search state or finishes.
//! This is the same seam GNU's callback interface uses (`_rl_isearch_callback`
//! and `_rl_nsearch_callback` process one key per callback), and it is the
//! documented way for an embedding application to drive these commands:
//!
//! * queue a key with `rl_pending_input = c`,
//! * call the search command again to process that key,
//! * clear `rl_pending_input` naturally as each key is consumed.
//!
//! No code here reads the terminal.  When no key is queued an active search
//! stays active and the command returns zero, exactly like the callback path.
//!
//! History navigation keeps GNU's saved-current-line contract: the first
//! `previous-history` saves the line being edited, later `next-history` past
//! the newest entry restores it, and edits made to a recalled line are written
//! back into the history entry before moving away.  There is no undo list in
//! this ABI slice, so "edited" means the line buffer differs from the stored
//! history line.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_ulong};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::{history, line, state};

type Command = unsafe extern "C" fn(c_int, c_int) -> c_int;

const RL_STATE_ISEARCH: c_ulong = 0x0000_0080;
const RL_STATE_NSEARCH: c_ulong = 0x0000_0100;
const RL_STATE_SEARCH: c_ulong = 0x0000_0200;
const RL_STATE_INPUTPENDING: c_ulong = 0x0002_0000;

const CTRL_C: c_int = 3;
const CTRL_G: c_int = 7;
const CTRL_H: c_int = 8;
const CTRL_Q: c_int = 17;
const CTRL_R: c_int = 18;
const CTRL_S: c_int = 19;
const CTRL_U: c_int = 21;
const CTRL_V: c_int = 22;
const CTRL_W: c_int = 23;
const CTRL_Y: c_int = 25;
const ESC: c_int = 27;
const RUBOUT: c_int = 127;
const RETURN: c_int = 13;
const NEWLINE: c_int = 10;

// --- Small helpers --------------------------------------------------------

fn state_set(bits: c_ulong) {
    state::set_readline_state(state::readline_state() | bits);
}

fn state_clear(bits: c_ulong) {
    state::set_readline_state(state::readline_state() & !bits);
}

fn ding() {
    // SAFETY: rl_ding only writes the bell to the error stream.
    unsafe { line::rl_ding() };
}

fn clear_message() {
    // SAFETY: clearing the message only resets prompt/display state.
    unsafe { line::rl_clear_message() };
}

fn line_buffer_bytes() -> Vec<u8> {
    let buffer = state::line_buffer();
    if buffer.is_null() {
        return Vec::new();
    }
    // SAFETY: rl_line_buffer is NUL-terminated and owned by this library.
    unsafe { CStr::from_ptr(buffer) }.to_bytes().to_vec()
}

fn entry_line(entry: *mut history::HIST_ENTRY) -> Option<Vec<u8>> {
    if entry.is_null() {
        return None;
    }
    let line_pointer = unsafe { (*entry).line };
    if line_pointer.is_null() {
        return None;
    }
    // SAFETY: history entries keep a NUL-terminated line until history changes.
    Some(unsafe { CStr::from_ptr(line_pointer) }.to_bytes().to_vec())
}

fn c_string(bytes: &[u8]) -> Vec<u8> {
    let mut terminated = Vec::with_capacity(bytes.len() + 1);
    terminated.extend_from_slice(bytes);
    terminated.push(0);
    terminated
}

/// Replace the current line without touching history or the saved line.
fn set_line_text(bytes: &[u8]) {
    let terminated = c_string(bytes);
    // SAFETY: `terminated` is NUL-terminated and valid for the call.
    unsafe { line::rl_replace_line(terminated.as_ptr() as *const c_char, 0) };
}

/// Take the next queued key, if the application supplied one.  This is the
/// only input source for the search state machines; see the module comment.
fn take_pending_key() -> Option<c_int> {
    let key = state::pending_input();
    if key == 0 {
        return None;
    }
    state::set_pending_input(0);
    state_clear(RL_STATE_INPUTPENDING);
    Some(key)
}

fn push_back_key(key: c_int) {
    state::set_pending_input(key);
    state_set(RL_STATE_INPUTPENDING);
}

fn history_base() -> c_int {
    // SAFETY: plain scalar read of the exported history global.
    unsafe { ptr::addr_of!(history::history_base).read() }
}

fn where_history() -> c_int {
    // SAFETY: the history module serializes access to its offset.
    unsafe { history::where_history() }
}

// --- Saved current line ---------------------------------------------------

/// GNU's `_rl_saved_line_for_history`: the line that was being edited before
/// the first `previous-history`.
static SAVED_LINE: Mutex<Option<Vec<u8>>> = Mutex::new(None);

fn saved_line() -> MutexGuard<'static, Option<Vec<u8>>> {
    SAVED_LINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn saved_line_bytes() -> Option<Vec<u8>> {
    saved_line().clone()
}

fn save_line_if_missing() {
    let mut saved = saved_line();
    if saved.is_none() {
        *saved = Some(line_buffer_bytes());
    }
}

fn take_saved_line() -> Option<Vec<u8>> {
    saved_line().take()
}

/// Write edits made to a recalled history line back into the entry, the
/// standalone equivalent of GNU's `_rl_maybe_replace_line`.  GNU compares undo
/// list identity; without undo lists, a changed line buffer is the signal.
fn maybe_replace_line() {
    let entry = unsafe { history::current_history() };
    let Some(stored) = entry_line(entry) else {
        return;
    };
    let current = line_buffer_bytes();
    if stored == current {
        return;
    }

    let terminated = c_string(&current);
    // SAFETY: the pointer is NUL-terminated and history copies it.
    let old = unsafe {
        history::replace_history_entry(
            where_history(),
            terminated.as_ptr() as *const c_char,
            ptr::null_mut(),
        )
    };
    if !old.is_null() {
        // SAFETY: replace_history_entry hands ownership of the old entry back.
        unsafe { history::free_history_entry(old) };
    }
}

fn history_set_point() {
    // GNU's emacs branch: point at the end and no mark when point is at the
    // end.  The vi branch (point at zero, mark at the end) is owned by the vi
    // implementation, but using the live editing mode keeps this honest.
    if crate::keymap::emacs_mode() {
        state::set_point(state::end());
        state::set_mark(0);
    } else {
        state::set_point(0);
        state::set_mark(state::end());
    }
}

fn replace_from_history(line_bytes: &[u8]) {
    set_line_text(line_bytes);
    history_set_point();
}

fn maybe_unsave_line() {
    match take_saved_line() {
        Some(bytes) => {
            set_line_text(&bytes);
            state::set_point(state::end());
        }
        None => ding(),
    }
}

fn previous_history_internal(count: c_int) -> bool {
    let mut remaining = count.max(0);
    let mut last: Option<Vec<u8>> = None;
    let mut found: Option<Vec<u8>> = None;

    while remaining != 0 {
        // SAFETY: previous_history returns a pointer valid until history changes.
        let entry = unsafe { history::previous_history() };
        let Some(bytes) = entry_line(entry) else {
            break;
        };
        last = Some(bytes.clone());
        found = Some(bytes);
        remaining -= 1;
    }

    // A large argument that reaches the start of history is not an error.
    if found.is_none() {
        found = last;
    }

    match found {
        Some(bytes) => {
            replace_from_history(&bytes);
            true
        }
        None => {
            ding();
            false
        }
    }
}

fn next_history_internal(count: c_int) -> bool {
    let mut remaining = count.max(0);
    let mut found: Option<Vec<u8>> = None;

    while remaining != 0 {
        // SAFETY: next_history returns a pointer valid until history changes.
        let entry = unsafe { history::next_history() };
        let Some(bytes) = entry_line(entry) else {
            break;
        };
        found = Some(bytes);
        remaining -= 1;
    }

    match found {
        Some(bytes) => {
            replace_from_history(&bytes);
            true
        }
        None => false,
    }
}

// --- History navigation commands -----------------------------------------

fn get_previous_history_body(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return get_next_history_body(-count, key);
    }
    if count == 0 {
        return 0;
    }
    // SAFETY: history_list only reports the current list pointer.
    if unsafe { history::history_list() }.is_null() {
        return 0;
    }

    let had_saved_line = saved_line_bytes().is_some();
    save_line_if_missing();
    maybe_replace_line();

    let found = previous_history_internal(count);
    if !found && !had_saved_line {
        // GNU frees the saved line it just created when no entry was found.
        let _ = take_saved_line();
    }
    0
}

fn get_next_history_body(count: c_int, key: c_int) -> c_int {
    if count < 0 {
        return get_previous_history_body(-count, key);
    }
    if count == 0 {
        return 0;
    }

    maybe_replace_line();
    if !next_history_internal(count) {
        maybe_unsave_line();
    }
    0
}

fn beginning_of_history_body(_count: c_int, key: c_int) -> c_int {
    get_previous_history_body(1 + where_history(), key)
}

fn end_of_history_body(_count: c_int, _key: c_int) -> c_int {
    maybe_replace_line();
    // SAFETY: using_history only resets the history offset.
    unsafe { history::using_history() };
    maybe_unsave_line();
    0
}

fn fetch_history_body(count: c_int, key: c_int) -> c_int {
    if state::explicit_arg() != 0 {
        let nhist = history_base() + where_history();
        let wanted = if count >= 0 { nhist - count } else { -count };

        if wanted <= 0 || wanted >= nhist {
            // GNU leaves the line unchanged for an out-of-range vi `G`.
            if crate::keymap::emacs_mode() {
                beginning_of_history_body(0, 0);
            } else {
                ding();
            }
        } else {
            get_previous_history_body(wanted, key);
        }
    } else {
        beginning_of_history_body(count, 0);
    }
    0
}

/// Logical history position that `operate-and-get-next` asks the *next*
/// readline invocation to select.  A negative value means "no pending move".
static SAVED_HISTORY_OFFSET: AtomicI32 = AtomicI32::new(-1);

fn operate_and_get_next_body(count: c_int, key: c_int) -> c_int {
    // GNU reads rl_explicit_arg after rl_newline returns, before the editor
    // dispatcher resets it.  Our rl_newline wrapper resets the argument, so
    // capture the state and the offset first.
    let explicit = state::explicit_arg();
    let current_offset = where_history();

    // SAFETY: the exported accept-line command only touches line state.
    unsafe { crate::editing::rl_newline(1, key) };

    let saved = if explicit != 0 {
        count
    } else {
        current_offset + history_base() + 1
    };
    SAVED_HISTORY_OFFSET.store(saved, Ordering::SeqCst);
    0
}

/// One-shot startup seam for `operate-and-get-next`, equivalent to GNU's
/// `_rl_internal_startup_hook = set_saved_history`.  The readline and callback
/// entry points call this after the application's startup hook.
pub(crate) fn run_saved_history_hook() {
    let saved = SAVED_HISTORY_OFFSET.swap(-1, Ordering::SeqCst);
    if saved < 0 {
        return;
    }

    // GNU has already run using_history() by now, so the current position is
    // the end of the history list including the just-accepted line.
    // SAFETY: using_history only resets the history offset.
    unsafe { history::using_history() };

    let absolute_offset = saved - history_base();
    let count = where_history() - absolute_offset;
    get_previous_history_body(count, 0);
}

fn clear_history_body(_count: c_int, _key: c_int) -> c_int {
    // GNU rl_clear_history resets length and offset but deliberately preserves
    // history_base (unlike history's clear_history()).
    let base = history_base();
    // SAFETY: clear_history frees entries under the history lock.
    unsafe { history::clear_history() };
    // SAFETY: plain scalar write of the exported history global.
    unsafe { ptr::addr_of_mut!(history::history_base).write(base) };
    0
}

// --- Non-incremental search (isearch.c) ----------------------------------

#[derive(Clone)]
struct NonIncContext {
    direction: c_int,
    save_point: c_int,
    save_mark: c_int,
    save_line: c_int,
    saved_search_line: Vec<u8>,
    /// A quoted-insert key was requested but no key was queued yet.
    quoted: bool,
}

static NONINC_CONTEXT: Mutex<Option<NonIncContext>> = Mutex::new(None);
static NONINC_SEARCH_STRING: Mutex<Option<Vec<u8>>> = Mutex::new(None);
static NONINC_HISTORY_POS: AtomicI32 = AtomicI32::new(-1);

fn noninc_context() -> MutexGuard<'static, Option<NonIncContext>> {
    NONINC_CONTEXT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn noninc_search_string() -> Option<Vec<u8>> {
    NONINC_SEARCH_STRING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn set_noninc_search_string(value: Option<Vec<u8>>) {
    *NONINC_SEARCH_STRING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
}

fn noninc_search_from_pos(
    string: &[u8],
    position: c_int,
    direction: c_int,
) -> Option<(c_int, c_int)> {
    if position < 0 {
        return None;
    }

    let old = where_history();
    // SAFETY: history_set_pos validates the position and only moves the offset.
    if unsafe { history::history_set_pos(position) } == 0 {
        return None;
    }

    let (needle, anchored) = match string.first() {
        Some(b'^') => (&string[1..], true),
        _ => (string, false),
    };
    if needle.is_empty() {
        // SAFETY: restore the offset before returning.
        unsafe { history::history_set_pos(old) };
        return None;
    }

    let terminated = c_string(needle);
    state_set(RL_STATE_SEARCH);
    // SAFETY: the needle is NUL-terminated and history only reads it.
    let offset = unsafe {
        if anchored {
            history::history_search_prefix(terminated.as_ptr() as *const c_char, direction)
        } else {
            history::history_search(terminated.as_ptr() as *const c_char, direction)
        }
    };
    state_clear(RL_STATE_SEARCH);

    let found = if offset >= 0 { where_history() } else { -1 };
    // SAFETY: restore the caller's position as GNU does.
    unsafe { history::history_set_pos(old) };

    if found < 0 {
        None
    } else {
        Some((found, offset))
    }
}

fn make_history_line_current(current_position: c_int, new_position: c_int) {
    if new_position < current_position {
        get_previous_history_body(current_position - new_position, 0);
    } else {
        get_next_history_body(new_position - current_position, 0);
    }
    state::clamp_point();
}

fn noninc_dosearch(string: &[u8], direction: c_int) -> bool {
    let start = NONINC_HISTORY_POS.load(Ordering::SeqCst);
    if string.is_empty() || start < 0 {
        ding();
        return false;
    }

    let Some((position, match_offset)) =
        noninc_search_from_pos(string, start + direction, direction)
    else {
        // Search failed; leave the history position unchanged.
        clear_message();
        state::set_point(0);
        ding();
        return false;
    };

    let old_position = where_history();
    // The caller has just restored the saved search line, which need not be
    // the text stored at `old_position` (GNU compares undo-list identity and
    // therefore sees no edit).  Sync the buffer with the entry we are leaving
    // so the first traversal does not write the restored line into it.
    if let Some(bytes) = entry_line(unsafe { history::current_history() }) {
        set_line_text(&bytes);
    }
    NONINC_HISTORY_POS.store(position, Ordering::SeqCst);
    make_history_line_current(old_position, position);

    // GNU's active-region default is on: highlight the match when it is
    // visible in the current line, otherwise put point at the start.
    if match_offset >= 0 && match_offset < state::end() {
        state::set_point(match_offset);
        let mark = (match_offset + string.len() as c_int).min(state::end());
        state::set_mark(mark);
        // SAFETY: the exported mark helper only changes mark state.
        unsafe { crate::editing::rl_activate_mark() };
    } else {
        state::set_point(0);
        state::set_mark(state::end());
    }

    clear_message();
    true
}

fn noninc_restore_line(context: &NonIncContext) {
    set_line_text(&context.saved_search_line);
    state::set_point(context.save_point);
    state::set_mark(context.save_mark);
    state::clamp_point();
}

fn noninc_abort(context: NonIncContext) -> c_int {
    noninc_restore_line(&context);
    state::clamp_mark();
    // SAFETY: the exported mark helper only changes mark state.
    unsafe { crate::editing::rl_deactivate_mark() };
    clear_message();
    state_clear(RL_STATE_NSEARCH);
    let _ = noninc_context().take();
    1
}

/// Process one search-string key.  Returns -1 to abort, 0 to finish, 1 to
/// keep reading.
fn noninc_dispatch(context: &mut NonIncContext, key: c_int) -> c_int {
    if context.quoted {
        context.quoted = false;
        crate::editing::insert(1, key);
        return 1;
    }

    if key < 0 {
        return -1;
    }

    match key {
        CTRL_W => {
            crate::editing::unix_word_rubout(1, key);
        }
        CTRL_U => {
            crate::editing::unix_line_discard(1, key);
        }
        CTRL_Q | CTRL_V => match take_pending_key() {
            Some(next) => {
                crate::editing::insert(1, next);
            }
            None => context.quoted = true,
        },
        RETURN | NEWLINE => return 0,
        CTRL_H | RUBOUT => {
            if state::point() == 0 {
                return -1;
            }
            crate::editing::backward_delete_char(1, key);
        }
        CTRL_C | CTRL_G => {
            ding();
            return -1;
        }
        _ => {
            // GNU falls through to insert for ESC and every other byte.
            crate::editing::insert(1, key);
        }
    }
    1
}

fn noninc_init(direction: c_int) -> NonIncContext {
    let context = NonIncContext {
        direction,
        save_point: state::point(),
        save_mark: state::mark(),
        save_line: where_history(),
        saved_search_line: line_buffer_bytes(),
        quoted: false,
    };

    // GNU reads the search string in the line buffer with a temporary prompt.
    set_line_text(b"");
    state::set_point(0);
    state::set_end(0);
    state_set(RL_STATE_NSEARCH);

    *noninc_context() = Some(context.clone());
    context
}

fn noninc_finish(context: NonIncContext) -> c_int {
    state::set_mark(context.save_mark);

    if state::point() == 0 {
        // Reuse the previous search string, if there is one.
        if noninc_search_string().is_none() {
            noninc_restore_line(&context);
            state::clamp_mark();
            ding();
            // SAFETY: restore the temporarily saved search prompt/line state.
            unsafe { crate::editing::rl_deactivate_mark() };
            clear_message();
            state_clear(RL_STATE_NSEARCH);
            let _ = noninc_context().take();
            return 1;
        }
    } else {
        NONINC_HISTORY_POS.store(context.save_line, Ordering::SeqCst);
        set_noninc_search_string(Some(line_buffer_bytes()));
        // The match should not inherit the search-string undo history; this
        // slice has no undo list, but clearing the buffer matches GNU's state.
        set_line_text(b"");
        state::set_point(0);
        state::set_end(0);
    }

    // Restore the line that was being edited before the search prompt.
    set_line_text(&context.saved_search_line);

    let Some(string) = noninc_search_string() else {
        state_clear(RL_STATE_NSEARCH);
        let _ = noninc_context().take();
        return 1;
    };

    let success = noninc_dosearch(&string, context.direction);
    if !success {
        state::set_point(context.save_point);
    }

    state_clear(RL_STATE_NSEARCH);
    let _ = noninc_context().take();
    if success {
        0
    } else {
        1
    }
}

fn noninc_search_command(direction: c_int) -> c_int {
    // Take without keeping the guard: `noninc_init` installs a fresh context
    // and would deadlock if the match scrutinee guard stayed alive.
    let existing = {
        let mut guard = noninc_context();
        guard.take()
    };
    let mut context = match existing {
        Some(context) => context,
        None => noninc_init(direction),
    };

    loop {
        let Some(key) = take_pending_key() else {
            *noninc_context() = Some(context);
            return 0;
        };
        let result = noninc_dispatch(&mut context, key);
        if result < 0 {
            return noninc_abort(context);
        }
        if result == 0 {
            return noninc_finish(context);
        }
    }
}

fn noninc_again(direction: c_int) -> c_int {
    let Some(string) = noninc_search_string() else {
        ding();
        return 1;
    };
    if noninc_dosearch(&string, direction) {
        0
    } else {
        1
    }
}

// --- Incremental search (isearch.c) --------------------------------------

#[derive(Clone)]
struct ISearchContext {
    direction: c_int,
    search: Vec<u8>,
    /// History lines followed by the saved/current line, as GNU's `cxt->lines`.
    lines: Vec<Vec<u8>>,
    sline: Vec<u8>,
    sline_len: c_int,
    sline_index: c_int,
    history_pos: c_int,
    last_found_line: c_int,
    save_line: c_int,
    save_point: c_int,
    save_mark: c_int,
    prev_line_found: Option<Vec<u8>>,
    quoted: bool,
}

static ISEARCH_CONTEXT: Mutex<Option<ISearchContext>> = Mutex::new(None);
static LAST_ISEARCH_STRING: Mutex<Option<Vec<u8>>> = Mutex::new(None);

fn isearch_context() -> MutexGuard<'static, Option<ISearchContext>> {
    ISEARCH_CONTEXT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn last_isearch_string() -> Option<Vec<u8>> {
    LAST_ISEARCH_STRING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn set_last_isearch_string(value: Vec<u8>) {
    *LAST_ISEARCH_STRING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(value);
}

fn history_lines() -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    // SAFETY: the list pointer stays valid until history changes; we only read.
    let list = unsafe { history::history_list() };
    if list.is_null() {
        return lines;
    }
    let mut index = 0;
    loop {
        // SAFETY: GNU terminates the list with a null entry.
        let entry = unsafe { *list.add(index) };
        if entry.is_null() {
            break;
        }
        if let Some(line_bytes) = entry_line(entry) {
            lines.push(line_bytes);
        }
        index += 1;
    }
    lines
}

fn isearch_init(direction: c_int) -> ISearchContext {
    maybe_replace_line();

    let mut lines = history_lines();
    let extra = saved_line_bytes().unwrap_or_else(line_buffer_bytes);
    lines.push(extra);

    let line_count = lines.len() as c_int;
    let save_line = where_history().clamp(0, line_count - 1);
    let sline = line_buffer_bytes();
    let sline_len = sline.len() as c_int;
    let sline_index = state::point().clamp(0, sline_len);
    let direction = if direction >= 0 { 1 } else { -1 };

    ISearchContext {
        direction,
        search: Vec::new(),
        lines,
        sline,
        sline_len,
        sline_index,
        history_pos: save_line,
        last_found_line: save_line,
        save_line,
        save_point: state::point(),
        save_mark: state::mark(),
        prev_line_found: None,
        quoted: false,
    }
}

enum SearchOutcome {
    Found,
    Failed,
}

/// Port of the search loop in `isearch.c:_rl_isearch_dispatch`.
fn isearch_run(context: &mut ISearchContext) -> SearchOutcome {
    let reverse = context.direction < 0;
    loop {
        if context.search.is_empty() {
            return SearchOutcome::Failed;
        }

        let limit = context.sline_len - context.search.len() as c_int + 1;
        let mut found = false;

        while if reverse {
            context.sline_index >= 0
        } else {
            context.sline_index < limit
        } {
            let index = context.sline_index.max(0) as usize;
            if context
                .sline
                .get(index..)
                .is_some_and(|rest| rest.starts_with(&context.search))
            {
                found = true;
                break;
            }
            context.sline_index += context.direction;
            if context.sline_index < 0 || context.sline_index > context.sline_len {
                context.sline_index = 0;
                break;
            }
        }

        if found {
            return SearchOutcome::Found;
        }

        loop {
            context.history_pos += context.direction;
            if if reverse {
                context.history_pos < 0
            } else {
                context.history_pos == context.lines.len() as c_int
            } {
                return SearchOutcome::Failed;
            }

            context.sline = context.lines[context.history_pos as usize].clone();
            context.sline_len = context.sline.len() as c_int;

            let same_line = context.prev_line_found.as_deref() == Some(context.sline.as_slice());
            if !(same_line || context.search.len() as c_int > context.sline_len) {
                break;
            }
        }

        context.sline_index = if reverse {
            context.sline_len - context.search.len() as c_int
        } else {
            0
        };
    }
}

fn isearch_apply(context: &mut ISearchContext) -> c_int {
    match isearch_run(context) {
        SearchOutcome::Found => {
            context.prev_line_found = Some(context.sline.clone());
            set_line_text(&context.sline);
            // GNU enables the active region around the match by default.
            // SAFETY: the exported mark helper only changes mark state.
            unsafe { crate::editing::rl_activate_mark() };
            state::set_point(context.sline_index);
            if unsafe { crate::editing::rl_mark_active_p() } != 0 && !context.search.is_empty() {
                let mark = (context.sline_index + context.search.len() as c_int).min(state::end());
                state::set_mark(mark);
            }
            context.last_found_line = context.history_pos;
        }
        SearchOutcome::Failed => {
            if context.sline_index < 0 || context.sline_index > context.sline_len {
                context.sline_index = 0;
            }
            ding();
            context.history_pos = context.last_found_line;
            // SAFETY: the exported mark helper only changes mark state.
            unsafe { crate::editing::rl_deactivate_mark() };
        }
    }
    1
}

fn isearch_abort(context: &ISearchContext) -> c_int {
    let original = context.lines[context.save_line as usize].clone();
    set_line_text(&original);
    state::set_point(context.save_point);
    state::set_mark(context.save_mark);
    // SAFETY: the exported mark helper only changes mark state.
    unsafe { crate::editing::rl_deactivate_mark() };
    state::clamp_point();
    clear_message();
    state_clear(RL_STATE_ISEARCH);
    -1
}

fn isearch_append(context: &mut ISearchContext, byte: u8) -> c_int {
    context.search.push(byte);
    isearch_apply(context)
}

/// Process one search key.  Returns -1 to finish without `_rl_isearch_fini`,
/// 0 to finish normally, and 1 to keep reading.
fn isearch_dispatch(context: &mut ISearchContext, key: c_int) -> c_int {
    if context.quoted {
        context.quoted = false;
        let byte = key as u8;
        return isearch_append(context, byte);
    }

    if key < 0 {
        context.history_pos = context.last_found_line;
        return -1;
    }

    // GNU maps keys through the active keymap and then uses opcodes.
    let reverse = context.direction < 0;
    match key {
        CTRL_R => {
            let opcode = if reverse { -1 } else { -2 };
            return isearch_opcode(context, opcode);
        }
        CTRL_S => {
            let opcode = if reverse { -2 } else { -1 };
            return isearch_opcode(context, opcode);
        }
        CTRL_H | RUBOUT => {
            if context.search.is_empty() {
                ding();
            } else {
                context.search.pop();
                if context.search.is_empty() {
                    ding();
                }
            }
            return isearch_apply(context);
        }
        CTRL_G => {
            return isearch_abort(context);
        }
        CTRL_W => {
            let opcode = -5;
            return isearch_opcode(context, opcode);
        }
        CTRL_Y => {
            let opcode = -6;
            return isearch_opcode(context, opcode);
        }
        CTRL_Q | CTRL_V => match take_pending_key() {
            Some(next) => {
                let byte = next as u8;
                return isearch_append(context, byte);
            }
            None => {
                context.quoted = true;
                return 1;
            }
        },
        ESC | NEWLINE => return 0,
        RETURN => {
            // RETURN is not an isearch terminator character, so GNU pushes it
            // back for the next command and then finishes.
            push_back_key(RETURN);
            return 0;
        }
        _ => {}
    }

    if !(0x20..0x80).contains(&key) {
        // ENDSRCH_CHAR: control/meta keys terminate and are re-executed.
        push_back_key(key);
        return 0;
    }

    isearch_append(context, key as u8)
}

fn isearch_opcode(context: &mut ISearchContext, opcode: c_int) -> c_int {
    let reverse = context.direction < 0;
    match opcode {
        -1 => {
            // Search again.
            if context.search.is_empty() {
                if let Some(previous) = last_isearch_string() {
                    context.search = previous;
                } else {
                    return 1;
                }
            } else if reverse && context.sline_index >= 0 {
                context.sline_index -= 1;
            } else if context.sline_index != context.sline_len {
                context.sline_index += 1;
            } else {
                ding();
            }
        }
        -2 => {
            context.direction = -context.direction;
        }
        -3 => {
            if context.search.is_empty() {
                ding();
            } else {
                context.search.pop();
                if context.search.is_empty() {
                    ding();
                }
            }
        }
        -5 => {
            // Skip the already matched portion and append a word.
            let buffer = line_buffer_bytes();
            let mut start = state::point() + context.search.len() as c_int;
            if start < 0 || start >= state::end() {
                ding();
                return isearch_apply(context);
            }
            let mut cursor = start as usize;
            if !buffer.get(cursor).is_some_and(u8::is_ascii_alphanumeric) {
                ding();
                return isearch_apply(context);
            }
            while cursor < buffer.len() && buffer[cursor].is_ascii_alphanumeric() {
                cursor += 1;
            }
            while (start as usize) < cursor {
                context.search.push(buffer[start as usize]);
                start += 1;
            }
        }
        -6 => {
            // Yank the rest of the current line into the search string.
            let buffer = line_buffer_bytes();
            let start = state::point() + context.search.len() as c_int;
            if start < 0 || start >= state::end() {
                ding();
                return isearch_apply(context);
            }
            let start = start as usize;
            if let Some(rest) = buffer.get(start..) {
                context.search.extend_from_slice(rest);
            }
        }
        _ => {}
    }
    isearch_apply(context)
}

fn isearch_fini(context: &ISearchContext) {
    // Restore the line where the search started, then move through history to
    // the last match exactly as GNU does (this also restores the saved line).
    let original = context.lines[context.save_line as usize].clone();
    set_line_text(&original);
    set_last_isearch_string(context.search.clone());

    if context.last_found_line < context.save_line {
        get_previous_history_body(context.save_line - context.last_found_line, 0);
    } else {
        get_next_history_body(context.last_found_line - context.save_line, 0);
    }

    let mut point = context.sline_index;
    if point < 0 {
        point = if context.last_found_line == context.save_line {
            context.save_point
        } else {
            line_buffer_bytes().len() as c_int
        };
        state::set_mark(context.save_mark);
        // SAFETY: the exported mark helper only changes mark state.
        unsafe { crate::editing::rl_deactivate_mark() };
    }

    state::set_point(point);
    state::clamp_point();
    // SAFETY: the exported mark helper only changes mark state.
    unsafe { crate::editing::rl_deactivate_mark() };
    clear_message();
}

fn isearch_command(direction: c_int) -> c_int {
    // As with the non-incremental path, drop the guard before the init arm.
    let existing = {
        let mut guard = isearch_context();
        guard.take()
    };
    let mut context = match existing {
        Some(context) => context,
        None => {
            let context = isearch_init(direction);
            state_set(RL_STATE_ISEARCH);
            context
        }
    };

    loop {
        let Some(key) = take_pending_key() else {
            *isearch_context() = Some(context);
            return 0;
        };

        let result = isearch_dispatch(&mut context, key);
        if result < 0 {
            state_clear(RL_STATE_ISEARCH);
            let _ = isearch_context().take();
            return 1;
        }
        if result == 0 {
            isearch_fini(&context);
            state_clear(RL_STATE_ISEARCH);
            let _ = isearch_context().take();
            return 0;
        }
    }
}

// --- Non-incremental history search (search.c) ---------------------------

#[derive(Clone)]
struct HistorySearchState {
    position: c_int,
    length: c_int,
    anchored: bool,
    search: Vec<u8>,
    previous_line: Option<Vec<u8>>,
}

static HISTORY_SEARCH: Mutex<HistorySearchState> = Mutex::new(HistorySearchState {
    position: 0,
    length: 0,
    anchored: false,
    search: Vec::new(),
    previous_line: None,
});

fn history_search_state() -> MutexGuard<'static, HistorySearchState> {
    HISTORY_SEARCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn reinit_history_search(anchored: bool) {
    let mut search = history_search_state();
    search.position = where_history();
    search.length = state::point();
    search.anchored = anchored;
    search.previous_line = None;
    search.search.clear();

    if search.length > 0 {
        let buffer = line_buffer_bytes();
        let length = (search.length as usize).min(buffer.len());
        if anchored {
            search.search.push(b'^');
        }
        search.search.extend_from_slice(&buffer[..length]);
    }
}

fn last_function_is(commands: &[Command]) -> bool {
    let Some(last) = state::last_func() else {
        return false;
    };
    commands
        .iter()
        .any(|command| std::ptr::fn_addr_eq(last, *command))
}

fn history_line_at(position: c_int) -> Option<Vec<u8>> {
    let old = where_history();
    // SAFETY: history_set_pos validates the position.
    if unsafe { history::history_set_pos(position) } == 0 {
        return None;
    }
    let entry = unsafe { history::current_history() };
    let line_bytes = entry_line(entry);
    // SAFETY: restore the caller's position.
    unsafe { history::history_set_pos(old) };
    line_bytes
}

fn history_search_internal_body(count: c_int, direction: c_int) -> c_int {
    let mut search = history_search_state();
    let old_position = where_history();
    let mut remaining = count.max(0);
    let mut temp: Option<Vec<u8>> = None;
    let mut match_offset = -1;

    while remaining != 0 {
        // The stored search string includes a leading `^` for anchored mode;
        // noninc_search_from_pos strips it and chooses the anchored helper.
        match noninc_search_from_pos(&search.search, search.position + direction, direction) {
            Some((position, offset)) => {
                match_offset = offset;
                search.position = position;
                let Some(line_bytes) = history_line_at(position) else {
                    break;
                };
                if search.previous_line.as_deref() == Some(line_bytes.as_slice()) {
                    continue;
                }
                search.previous_line = Some(line_bytes.clone());
                temp = Some(line_bytes);
                remaining -= 1;
            }
            None => {
                match_offset = -1;
                break;
            }
        }
    }

    if temp.is_none() {
        // No match at all: leave the history offset alone and move point to
        // the end of the search prefix, as GNU does on failure.
        ding();
        state::set_point(search.length);
        state::set_mark(state::end());
        return 1;
    }

    make_history_line_current(old_position, search.position);

    if search.anchored {
        state::set_point(search.length);
    } else {
        state::set_point(if match_offset >= 0 {
            match_offset
        } else {
            state::end()
        });
    }
    state::set_mark(state::end());
    0
}

fn history_search_command(count: c_int, anchored: bool, forward: bool) -> c_int {
    if count == 0 {
        return 0;
    }

    let (forward_command, backward_command) = if anchored {
        (
            rl_history_search_forward as Command,
            rl_history_search_backward as Command,
        )
    } else {
        (
            rl_history_substr_search_forward as Command,
            rl_history_substr_search_backward as Command,
        )
    };
    if !last_function_is(&[forward_command, backward_command]) {
        reinit_history_search(anchored);
    }

    if history_search_state().length == 0 {
        return if forward {
            get_next_history_body(count, 0)
        } else {
            get_previous_history_body(count, 0)
        };
    }

    let direction = if count > 0 {
        if forward {
            1
        } else {
            -1
        }
    } else if forward {
        -1
    } else {
        1
    };
    let magnitude = count.unsigned_abs() as c_int;
    history_search_internal_body(magnitude, direction)
}

// --- Exported GNU command ABI --------------------------------------------

macro_rules! export_command {
    ($export:ident => $body:path) => {
        #[no_mangle]
        pub unsafe extern "C" fn $export(count: c_int, key: c_int) -> c_int {
            let previous = crate::editing::begin_command();
            let result = $body(count, key);
            crate::editing::end_command(previous, false);
            state::set_last_func(Some($export as Command));
            result
        }
    };
}

/// Meta-<: move to the oldest history entry.
#[no_mangle]
pub unsafe extern "C" fn rl_beginning_of_history(count: c_int, key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = beginning_of_history_body(count, key);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_beginning_of_history as Command));
    result
}

/// Meta->: move back to the current (saved) line.
#[no_mangle]
pub unsafe extern "C" fn rl_end_of_history(count: c_int, key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = end_of_history_body(count, key);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_end_of_history as Command));
    result
}

export_command!(rl_get_previous_history => get_previous_history_body);
export_command!(rl_get_next_history => get_next_history_body);
export_command!(rl_fetch_history => fetch_history_body);
export_command!(rl_operate_and_get_next => operate_and_get_next_body);

/// Free the history list while preserving `history_base`, matching GNU's
/// readline-specific `rl_clear_history`.
#[no_mangle]
pub unsafe extern "C" fn rl_clear_history() {
    clear_history_body(0, 0);
}

/// Reverse incremental search (C-r).  The next key is taken from
/// `rl_pending_input`; with no queued key the search state stays active.
#[no_mangle]
pub unsafe extern "C" fn rl_reverse_search_history(sign: c_int, _key: c_int) -> c_int {
    let direction = if sign > 0 { -1 } else { 1 };
    let previous = crate::editing::begin_command();
    let result = isearch_command(direction);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_reverse_search_history as Command));
    result
}

/// Forward incremental search (C-s).
#[no_mangle]
pub unsafe extern "C" fn rl_forward_search_history(sign: c_int, _key: c_int) -> c_int {
    let direction = if sign >= 0 { 1 } else { -1 };
    let previous = crate::editing::begin_command();
    let result = isearch_command(direction);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_forward_search_history as Command));
    result
}

/// Search backward for the text from the start of the line to point.
#[no_mangle]
pub unsafe extern "C" fn rl_history_search_backward(count: c_int, _key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = history_search_command(count, true, false);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_history_search_backward as Command));
    result
}

/// Search forward for the text from the start of the line to point.
#[no_mangle]
pub unsafe extern "C" fn rl_history_search_forward(count: c_int, _key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = history_search_command(count, true, true);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_history_search_forward as Command));
    result
}

/// Search backward for the text from the start of the line to point, allowing
/// the text to appear anywhere in a history line.
#[no_mangle]
pub unsafe extern "C" fn rl_history_substr_search_backward(count: c_int, _key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = history_search_command(count, false, false);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_history_substr_search_backward as Command));
    result
}

/// Search forward for the text from the start of the line to point, allowing
/// the text to appear anywhere in a history line.
#[no_mangle]
pub unsafe extern "C" fn rl_history_substr_search_forward(count: c_int, _key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = history_search_command(count, false, true);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_history_substr_search_forward as Command));
    result
}

/// Read a search string (one queued key per call) and search forward.
#[no_mangle]
pub unsafe extern "C" fn rl_noninc_forward_search(_count: c_int, _key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = noninc_search_command(1);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_noninc_forward_search as Command));
    result
}

/// Read a search string (one queued key per call) and search backward.
#[no_mangle]
pub unsafe extern "C" fn rl_noninc_reverse_search(_count: c_int, _key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = noninc_search_command(-1);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_noninc_reverse_search as Command));
    result
}

/// Repeat the last non-incremental search, forward.
#[no_mangle]
pub unsafe extern "C" fn rl_noninc_forward_search_again(_count: c_int, _key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = noninc_again(1);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_noninc_forward_search_again as Command));
    result
}

/// Repeat the last non-incremental search, backward.
#[no_mangle]
pub unsafe extern "C" fn rl_noninc_reverse_search_again(_count: c_int, _key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = noninc_again(-1);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_noninc_reverse_search_again as Command));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_string_terminates_without_changing_content() {
        let terminated = c_string(b"abc");
        assert_eq!(terminated, b"abc\0");
    }

    #[test]
    fn saved_history_offset_arithmetic_matches_gnu_example() {
        // base 1, three entries, cursor on entry 2: the next line after the
        // accepted line was entry 3 (offset 2).
        let base = 1;
        let saved = 1 + base + 1;
        let absolute = saved - base;
        let after_accept = 4;
        assert_eq!(after_accept - absolute, 2);
    }
}
