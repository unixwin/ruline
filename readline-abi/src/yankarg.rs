//! History-argument yanks: `rl_yank_nth_arg` and `rl_yank_last_arg`.
//!
//! Both commands pull an argument out of the previous history entry and insert
//! it as one undoable unit.  `rl_yank_last_arg` remembers its last invocation
//! and walks further back through history when repeated, undoing the previous
//! yank first (kill.c `rl_yank_nth_arg_internal`, `rl_yank_last_arg`).

use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::history;
use crate::state;

type Command = unsafe extern "C" fn(c_int, c_int) -> c_int;

static HISTORY_SKIP: AtomicI32 = AtomicI32::new(0);
static EXPLICIT_ARG: AtomicBool = AtomicBool::new(false);
static COUNT_PASSED: AtomicI32 = AtomicI32::new(1);
static DIRECTION: AtomicI32 = AtomicI32::new(1);
static UNDO_NEEDED: AtomicBool = AtomicBool::new(false);

fn ring_bell() {
    // SAFETY: the bell writer only touches the output stream.
    unsafe { crate::line::rl_ding() };
}

fn set_mark_at_pos(position: c_int) -> c_int {
    if position < 0 || position > state::end() {
        return 1;
    }
    state::set_mark(position);
    0
}

unsafe fn yank_nth_arg_internal(count: c_int, _key: c_int, history_skip: c_int) -> c_int {
    let position = history::where_history();

    for _ in 0..history_skip.max(0) {
        let _ = history::previous_history();
    }
    let entry = history::previous_history();

    history::history_set_pos(position);

    if entry.is_null() {
        ring_bell();
        return 1;
    }

    let argument = history::history_arg_extract(count, count, (*entry).line);
    if argument.is_null() || *argument == 0 {
        ring_bell();
        if !argument.is_null() {
            libc::free(argument as *mut libc::c_void);
        }
        return 1;
    }

    crate::undo::rl_begin_undo_group();
    set_mark_at_pos(state::point());

    crate::line::rl_insert_text(argument);
    libc::free(argument as *mut libc::c_void);

    crate::undo::rl_end_undo_group();
    0
}

/// Yank the `count`th argument from the previous history line.
#[no_mangle]
pub unsafe extern "C" fn rl_yank_nth_arg(count: c_int, key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = yank_nth_arg_internal(count, key, 0);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_yank_nth_arg as Command));
    result
}

/// Yank the last argument from the previous history line, walking back when
/// repeated.
#[no_mangle]
pub unsafe extern "C" fn rl_yank_last_arg(count: c_int, key: c_int) -> c_int {
    let previous = crate::editing::begin_command();
    let result = yank_last_arg_body(count, key);
    crate::editing::end_command(previous, false);
    state::set_last_func(Some(rl_yank_last_arg as Command));
    result
}

unsafe fn yank_last_arg_body(count: c_int, key: c_int) -> c_int {
    let repeated = state::last_func()
        .is_some_and(|function| std::ptr::fn_addr_eq(function, rl_yank_last_arg as Command));

    if !repeated {
        HISTORY_SKIP.store(0, Ordering::Relaxed);
        EXPLICIT_ARG.store(state::explicit_arg() != 0, Ordering::Relaxed);
        COUNT_PASSED.store(count, Ordering::Relaxed);
        DIRECTION.store(1, Ordering::Relaxed);
    } else {
        if UNDO_NEEDED.load(Ordering::Relaxed) {
            crate::undo::rl_do_undo();
        }
        if count < 0 {
            DIRECTION.store(-DIRECTION.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        let mut skip = HISTORY_SKIP.load(Ordering::Relaxed) + DIRECTION.load(Ordering::Relaxed);
        if skip < 0 {
            skip = 0;
        }
        HISTORY_SKIP.store(skip, Ordering::Relaxed);
    }

    let result = if EXPLICIT_ARG.load(Ordering::Relaxed) {
        yank_nth_arg_internal(
            COUNT_PASSED.load(Ordering::Relaxed),
            key,
            HISTORY_SKIP.load(Ordering::Relaxed),
        )
    } else {
        yank_nth_arg_internal(b'$' as c_int, key, HISTORY_SKIP.load(Ordering::Relaxed))
    };

    UNDO_NEEDED.store(result == 0, Ordering::Relaxed);
    result
}
