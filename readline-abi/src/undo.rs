//! Undo lists, undo groups, and the saved history line.
//!
//! GNU readline records every buffer mutation on a singly linked `UNDO_LIST`
//! whose layout is part of the public ABI.  The same list is attached to a
//! history entry through `HIST_ENTRY.data`, which lets a recalled line carry
//! its own edit history.  This module reproduces that model: entries are
//! libc allocations, `UNDO_DELETE` owns its saved text, and `rl_do_undo`
//! replays entries until the matching `UNDO_BEGIN` closes a group.
//!
//! The mutation hooks live in `line.rs`: `rl_insert_text` and `rl_delete_text`
//! call [`record_insert`] / [`record_delete`] unless an undo is in progress.
//! `rl_modifying` and the history helpers mirror `undo.c` and the
//! `_rl_maybe_*_line` helpers from `misc.c`.

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;

use crate::history;
use crate::state;

/// GNU's `enum undo_code` values (undo.c, readline.h).
pub const UNDO_DELETE: c_int = 0;
pub const UNDO_INSERT: c_int = 1;
pub const UNDO_BEGIN: c_int = 2;
pub const UNDO_END: c_int = 3;

const RL_STATE_UNDOING: std::os::raw::c_ulong = 0x0001_0000;

/// Public layout of GNU's `UNDO_LIST`.
#[repr(C)]
pub struct UNDO_LIST {
    pub next: *mut UNDO_LIST,
    pub start: c_int,
    pub end: c_int,
    pub text: *mut c_char,
    pub what: c_int,
}

/// The undo list for the current line.  GNU exports this symbol.
#[no_mangle]
pub static mut rl_undo_list: *mut UNDO_LIST = ptr::null_mut();

/// Non-zero while `rl_do_undo` is replaying an entry, so the buffer mutation
/// it performs does not itself become undoable.
static DOING_AN_UNDO: AtomicBool = AtomicBool::new(false);
/// Number of unclosed `rl_begin_undo_group` calls (GNU's
/// `_rl_undo_group_level`).  Kept for parity even though undo playback uses
/// the BEGIN/END entries themselves.
static UNDO_GROUP_LEVEL: AtomicI32 = AtomicI32::new(0);

/// The line saved by `rl_maybe_save_line`, with the undo list it owned.
struct SavedLine {
    line: Vec<u8>,
    data: usize,
}

static SAVED_LINE: Mutex<Option<SavedLine>> = Mutex::new(None);

fn undo_list() -> *mut UNDO_LIST {
    unsafe { ptr::addr_of!(rl_undo_list).read() }
}

fn set_undo_list(value: *mut UNDO_LIST) {
    unsafe { ptr::addr_of_mut!(rl_undo_list).write(value) }
}

pub(crate) fn is_undoing() -> bool {
    DOING_AN_UNDO.load(Ordering::Relaxed)
}

fn set_undoing(value: bool) {
    DOING_AN_UNDO.store(value, Ordering::Relaxed);
}

fn saved_line() -> std::sync::MutexGuard<'static, Option<SavedLine>> {
    SAVED_LINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn current_line_bytes() -> Vec<u8> {
    let buffer = state::line_buffer();
    let end = state::end().max(0) as usize;
    if buffer.is_null() {
        Vec::new()
    } else {
        // SAFETY: the buffer holds at least `end` bytes followed by a NUL.
        unsafe { std::slice::from_raw_parts(buffer as *const u8, end) }.to_vec()
    }
}

fn alloc_undo(what: c_int, start: c_int, end: c_int, text: *mut c_char) -> *mut UNDO_LIST {
    // SAFETY: `UNDO_LIST` is a plain C struct; malloc provides the storage
    // that C callers may inspect through `rl_undo_list`.
    let entry = unsafe { libc::malloc(std::mem::size_of::<UNDO_LIST>()) as *mut UNDO_LIST };
    if entry.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::write(
            entry,
            UNDO_LIST {
                next: ptr::null_mut(),
                start,
                end,
                text,
                what,
            },
        );
    }
    entry
}

/// Free a whole undo list, releasing the text owned by delete entries.
///
/// SAFETY: `list` must be a list produced by `alloc_undo` (or null).
pub(crate) unsafe fn free_undo_list_entries(mut list: *mut UNDO_LIST) {
    while !list.is_null() {
        let release = list;
        list = (*release).next;
        if (*release).what == UNDO_DELETE && !(*release).text.is_null() {
            libc::free((*release).text as *mut c_void);
        }
        libc::free(release as *mut c_void);
    }
}

fn fix_point(fix_mark_too: bool) {
    state::clamp_point();
    if fix_mark_too {
        state::clamp_mark();
    }
}

fn transform(position: c_int) -> c_int {
    match position {
        -1 => state::point(),
        -2 => state::end(),
        other => other,
    }
}

/// Record that `length` bytes were inserted at `point`.
///
/// Mirrors `rl_insert_text`'s coalescing rule: consecutive one-byte inserts at
/// the end of an existing insert run extend that run, up to twenty bytes.
pub(crate) fn record_insert(point: c_int, length: c_int) {
    if length <= 0 {
        return;
    }
    let list = undo_list();
    if length == 1
        && !list.is_null()
        && unsafe { (*list).what == UNDO_INSERT }
        && unsafe { (*list).end == point }
        && unsafe { (*list).end - (*list).start < 20 }
    {
        unsafe { (*list).end += 1 };
        return;
    }
    let entry = alloc_undo(UNDO_INSERT, point, point + length, ptr::null_mut());
    if !entry.is_null() {
        unsafe { (*entry).next = list };
        set_undo_list(entry);
    }
}

/// Record that bytes `from..to` were deleted, saving a copy of the text.
pub(crate) fn record_delete(from: c_int, to: c_int) {
    if from == to {
        return;
    }
    // SAFETY: rl_copy_text clamps the endpoints and returns malloc storage.
    let text = unsafe { crate::line::rl_copy_text(from, to) };
    if text.is_null() {
        return;
    }
    let entry = alloc_undo(UNDO_DELETE, from, to, text);
    if entry.is_null() {
        unsafe { libc::free(text as *mut c_void) };
        return;
    }
    unsafe {
        (*entry).next = undo_list();
    }
    set_undo_list(entry);
}

// --- Exported undo ABI ----------------------------------------------------

/// Add an undo entry.  `text` is owned by the list when `what` is
/// `UNDO_DELETE`.
#[no_mangle]
pub unsafe extern "C" fn rl_add_undo(what: c_int, start: c_int, end: c_int, text: *mut c_char) {
    let entry = alloc_undo(what, start, end, text);
    if entry.is_null() {
        return;
    }
    (*entry).next = undo_list();
    set_undo_list(entry);
}

/// Free the current undo list and detach it from history and saved lines.
#[no_mangle]
pub unsafe extern "C" fn rl_free_undo_list() {
    let original = undo_list();
    free_undo_list_entries(original);
    set_undo_list(ptr::null_mut());

    history::replace_history_data(original as *mut c_void, ptr::null_mut());
    if let Some(saved) = saved_line().as_mut() {
        if saved.data == original as usize {
            saved.data = 0;
        }
    }
}

/// Begin an undo group: everything until the matching end is one operation.
#[no_mangle]
pub unsafe extern "C" fn rl_begin_undo_group() -> c_int {
    rl_add_undo(UNDO_BEGIN, 0, 0, ptr::null_mut());
    UNDO_GROUP_LEVEL.fetch_add(1, Ordering::Relaxed);
    0
}

/// Close the most recent undo group.
#[no_mangle]
pub unsafe extern "C" fn rl_end_undo_group() -> c_int {
    rl_add_undo(UNDO_END, 0, 0, ptr::null_mut());
    UNDO_GROUP_LEVEL.fetch_sub(1, Ordering::Relaxed);
    0
}

/// Record the text between `start` and `end` as one reversible change.
#[no_mangle]
pub unsafe extern "C" fn rl_modifying(start: c_int, end: c_int) -> c_int {
    let (start, end) = if start > end {
        (end, start)
    } else {
        (start, end)
    };
    if start == end {
        return 0;
    }

    let text = crate::line::rl_copy_text(start, end);
    rl_begin_undo_group();
    rl_add_undo(UNDO_DELETE, start, end, text);
    rl_add_undo(UNDO_INSERT, start, end, ptr::null_mut());
    rl_end_undo_group();
    0
}

/// Undo the next entry, or a whole group.  Returns 0 when nothing is left.
#[no_mangle]
pub unsafe extern "C" fn rl_do_undo() -> c_int {
    let mut waiting_for_begin = 0;

    loop {
        let list = undo_list();
        if list.is_null() {
            return 0;
        }

        set_undoing(true);
        state::set_readline_state(state::readline_state() | RL_STATE_UNDOING);

        let mut start = 0;
        let mut end = 0;
        if (*list).what == UNDO_DELETE || (*list).what == UNDO_INSERT {
            start = transform((*list).start);
            end = transform((*list).end);
        }

        match (*list).what {
            UNDO_DELETE => {
                state::set_point(start);
                fix_point(true);
                crate::line::rl_insert_text((*list).text);
                if !(*list).text.is_null() {
                    libc::free((*list).text as *mut c_void);
                    (*list).text = ptr::null_mut();
                }
            }
            UNDO_INSERT => {
                crate::line::rl_delete_text(start, end);
                state::set_point(start);
                fix_point(true);
            }
            UNDO_END => waiting_for_begin += 1,
            UNDO_BEGIN => {
                if waiting_for_begin > 0 {
                    waiting_for_begin -= 1;
                } else {
                    crate::line::rl_ding();
                }
            }
            _ => {}
        }

        set_undoing(false);
        state::set_readline_state(state::readline_state() & !RL_STATE_UNDOING);

        let release = list;
        let next = (*release).next;
        (*release).next = ptr::null_mut();
        set_undo_list(next);

        // Keep a history entry that owns this undo list in sync with the
        // current buffer, exactly as undo.c does.
        let current = history::current_history();
        if !current.is_null()
            && !(*current).data.is_null()
            && (*current).data == release as *mut c_void
        {
            let replaced = history::replace_history_entry(
                history::where_history(),
                state::line_buffer(),
                next as *mut c_void,
            );
            if !replaced.is_null() {
                history::free_history_entry(replaced);
            }
        }

        history::replace_history_data(release as *mut c_void, next as *mut c_void);

        if let Some(saved) = saved_line().as_mut() {
            if saved.data == release as usize {
                saved.data = next as usize;
            } else if saved.data != 0 {
                let mut search = saved.data as *mut UNDO_LIST;
                while !search.is_null() && !(*search).next.is_null() {
                    if (*search).next == release {
                        (*search).next = next;
                        break;
                    }
                    search = (*search).next;
                }
            }
        }

        libc::free(release as *mut c_void);

        if waiting_for_begin == 0 {
            break;
        }
    }

    1
}

/// Undo everything on the current line.
#[no_mangle]
pub unsafe extern "C" fn rl_revert_line(_count: c_int, _key: c_int) -> c_int {
    if undo_list().is_null() {
        crate::line::rl_ding();
    } else {
        while !undo_list().is_null() {
            rl_do_undo();
        }
    }
    0
}

/// Undo `count` entries, stopping at the first empty list.
#[no_mangle]
pub unsafe extern "C" fn rl_undo_command(count: c_int, _key: c_int) -> c_int {
    if count < 0 {
        return 0;
    }
    let mut remaining = count;
    while remaining != 0 {
        if rl_do_undo() != 0 {
            remaining -= 1;
        } else {
            crate::line::rl_ding();
            break;
        }
    }
    0
}

// --- Saved line and history integration ----------------------------------

/// Save the current line if it has not been saved already.
#[no_mangle]
pub unsafe extern "C" fn rl_maybe_save_line() -> c_int {
    let mut saved = saved_line();
    if saved.is_none() {
        let line = current_line_bytes();
        *saved = Some(SavedLine {
            line,
            data: undo_list() as usize,
        });
    }
    0
}

/// Restore the line saved by `rl_maybe_save_line`, if any.
#[no_mangle]
pub unsafe extern "C" fn rl_maybe_unsave_line() -> c_int {
    let saved = saved_line().take();
    match saved {
        Some(saved) => {
            let mut terminated = saved.line.clone();
            terminated.push(0);
            crate::line::rl_replace_line(terminated.as_ptr() as *const c_char, 0);
            set_undo_list(saved.data as *mut UNDO_LIST);
            state::set_point(state::end());
            0
        }
        None => {
            crate::line::rl_ding();
            0
        }
    }
}

/// Store the current line (and its undo list) into the current history entry
/// when it has changed.
#[no_mangle]
pub unsafe extern "C" fn rl_maybe_replace_line() -> c_int {
    let current = history::current_history();
    if !current.is_null() && (*current).data != undo_list() as *mut c_void {
        let replaced = history::replace_history_entry(
            history::where_history(),
            state::line_buffer(),
            undo_list() as *mut c_void,
        );
        if !replaced.is_null() {
            history::free_history_entry(replaced);
        }
        if let Some(saved) = saved_line().as_mut() {
            if saved.data == undo_list() as usize {
                saved.data = 0;
            }
        }
    }
    0
}

// --- Legacy compat.c aliases ---------------------------------------------

/// Legacy spelling of `rl_free_undo_list` (compat.c).
#[no_mangle]
pub unsafe extern "C" fn free_undo_list() {
    rl_free_undo_list();
}

/// Legacy spelling of `rl_maybe_replace_line` (compat.c).
#[no_mangle]
pub unsafe extern "C" fn maybe_replace_line() -> c_int {
    rl_maybe_replace_line()
}

/// Legacy spelling of `rl_maybe_save_line` (compat.c).
#[no_mangle]
pub unsafe extern "C" fn maybe_save_line() -> c_int {
    rl_maybe_save_line()
}

/// Legacy spelling of `rl_maybe_unsave_line` (compat.c).
#[no_mangle]
pub unsafe extern "C" fn maybe_unsave_line() -> c_int {
    rl_maybe_unsave_line()
}
