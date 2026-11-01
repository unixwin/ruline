//! Round-trips for the remaining non-vi text commands: undo, character
//! search, keyboard macros, history-argument yanks, comment insertion, tilde
//! expansion, and terminal flow control.
//!
//! Everything goes through the exported C ABI.  Tests share the global line
//! and history state, so they take the same process-wide lock the other ABI
//! integration tests use.

use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::sync::{Mutex, MutexGuard};

static TEST_LOCK: Mutex<()> = Mutex::new(());

const RL_STATE_MACROINPUT: std::os::raw::c_ulong = 0x0000_0800;
const RL_STATE_MACRODEF: std::os::raw::c_ulong = 0x0000_1000;
const RL_STATE_DONE: std::os::raw::c_ulong = 0x0200_0000;

fn lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

unsafe extern "C" fn noop_redisplay() {}

fn init() {
    // SAFETY: callers hold TEST_LOCK, which serialises the global ABI state.
    unsafe {
        readline::rl_redisplay_function = Some(noop_redisplay);
        readline::rl_initialize();
        readline::rl_free_line_state();
        readline::rl_done = 0;
        readline::rl_readline_state &= !(RL_STATE_DONE | RL_STATE_MACRODEF | RL_STATE_MACROINPUT);
    }
}

fn set_line(text: &str) {
    let text = CString::new(text).unwrap();
    // SAFETY: `text` is a valid NUL-terminated string.
    unsafe { readline::rl_replace_line(text.as_ptr(), 0) };
}

fn line() -> String {
    // SAFETY: rl_initialize installs a NUL-terminated line buffer.
    unsafe { CStr::from_ptr(readline::rl_line_buffer) }
        .to_string_lossy()
        .into_owned()
}

fn point() -> c_int {
    // SAFETY: plain scalar read of exported state.
    unsafe { readline::rl_point }
}

fn set_point(value: c_int) {
    // SAFETY: plain scalar write of exported state.
    unsafe { readline::rl_point = value };
}

fn insert(text: &str) {
    let text = CString::new(text).unwrap();
    // SAFETY: the string is valid for the call.
    unsafe { readline::rl_insert_text(text.as_ptr()) };
}

fn delete(from: c_int, to: c_int) {
    // SAFETY: the ABI clamps the endpoints.
    unsafe { readline::rl_delete_text(from, to) };
}

#[test]
fn undo_insert_delete_and_group_replay() {
    let _guard = lock();
    init();

    set_line("abcdef");
    set_point(2);
    insert("XY");
    assert_eq!(line(), "abXYcdef");
    assert_eq!(point(), 4);
    // SAFETY: serialised ABI call.
    assert_eq!(unsafe { readline::rl_do_undo() }, 1);
    assert_eq!(line(), "abcdef");
    assert_eq!(point(), 2);
    assert_eq!(unsafe { readline::rl_do_undo() }, 0);

    set_point(3);
    delete(1, 3);
    assert_eq!(line(), "adef");
    assert_eq!(unsafe { readline::rl_do_undo() }, 1);
    assert_eq!(line(), "abcdef");
    assert_eq!(point(), 3);

    // A group is undone atomically.
    unsafe {
        readline::rl_begin_undo_group();
        readline::rl_end_undo_group();
    }
    set_point(3);
    unsafe {
        readline::rl_begin_undo_group();
    }
    insert("1");
    insert("2");
    unsafe {
        readline::rl_end_undo_group();
    }
    assert_eq!(line(), "abc12def");
    assert_eq!(unsafe { readline::rl_do_undo() }, 1);
    assert_eq!(line(), "abcdef");
}

#[test]
fn modifying_and_revert_line() {
    let _guard = lock();
    init();

    set_line("abc");
    // SAFETY: serialised ABI call.
    unsafe { readline::rl_modifying(0, 2) };
    assert_eq!(line(), "abc");
    assert_eq!(unsafe { readline::rl_do_undo() }, 1);
    assert_eq!(line(), "abc");
    assert_eq!(point(), 2);

    set_line("abc");
    set_point(3);
    insert("d");
    assert_eq!(line(), "abcd");
    assert_eq!(unsafe { readline::rl_revert_line(1, 0) }, 0);
    assert_eq!(line(), "abc");

    // Reverting an empty list only dings.
    unsafe {
        readline::rl_free_undo_list();
    }
    assert_eq!(unsafe { readline::rl_revert_line(1, 0) }, 0);
}

#[test]
fn undo_command_counts_and_negative_argument() {
    let _guard = lock();
    init();

    set_line("ab");
    set_point(1);
    insert("X");
    insert("Y");
    // Two one-byte inserts coalesce, so one undo removes both.
    assert_eq!(line(), "aXYb");
    assert_eq!(unsafe { readline::rl_undo_command(1, 0) }, 0);
    assert_eq!(line(), "ab");
    assert_eq!(unsafe { readline::rl_undo_command(1, 0) }, 0);
    assert_eq!(line(), "ab");
    assert_eq!(unsafe { readline::rl_undo_command(-1, 0) }, 0);
}

#[test]
fn save_unsave_replace_and_legacy_aliases() {
    let _guard = lock();
    init();

    set_line("original");
    set_point(8);
    assert_eq!(unsafe { readline::rl_maybe_save_line() }, 0);
    set_line("changed");
    assert_eq!(unsafe { readline::rl_maybe_unsave_line() }, 0);
    assert_eq!(line(), "original");
    assert_eq!(point(), 8);
    assert_eq!(unsafe { readline::rl_maybe_unsave_line() }, 0);

    unsafe {
        readline::clear_history();
        readline::add_history(CString::new("hist").unwrap().as_ptr());
        readline::using_history();
        readline::history_set_pos(0);
    }
    set_line("edited");
    // Give the line an undo list so the history entry actually changes.
    unsafe { readline::rl_add_undo(readline::UNDO_BEGIN, 0, 0, std::ptr::null_mut()) };
    assert_eq!(unsafe { readline::rl_maybe_replace_line() }, 0);
    unsafe {
        let list = readline::history_list();
        assert!(!list.is_null());
        assert_eq!(CStr::from_ptr((**list).line).to_str().unwrap(), "edited");
    }

    // The compat.c spellings delegate to the rl_* bodies.
    unsafe {
        readline::maybe_replace_line();
        readline::maybe_save_line();
        readline::maybe_unsave_line();
        readline::free_undo_list();
    }
}

#[test]
fn character_search_direction_and_failure() {
    let _guard = lock();
    init();

    set_line("a-b-c-d");
    set_point(0);
    // SAFETY: rl_pending_input is the documented queued-key slot.
    unsafe { readline::rl_pending_input = '-' as c_int };
    assert_eq!(unsafe { readline::rl_char_search(2, 0) }, 0);
    assert_eq!(point(), 3);

    unsafe { readline::rl_pending_input = '-' as c_int };
    assert_eq!(unsafe { readline::rl_backward_char_search(1, 0) }, 0);
    assert_eq!(point(), 1);

    set_point(6);
    unsafe { readline::rl_pending_input = '-' as c_int };
    assert_eq!(unsafe { readline::rl_char_search(-1, 0) }, 0);
    assert_eq!(point(), 5);

    set_point(0);
    unsafe { readline::rl_pending_input = 'z' as c_int };
    assert_eq!(unsafe { readline::rl_char_search(1, 0) }, 1);
    assert_eq!(point(), 0);

    // No key available: fail without moving or faking a terminal read.
    assert_eq!(unsafe { readline::rl_char_search(1, 0) }, 1);
    assert_eq!(point(), 0);
}

#[test]
fn macro_state_print_and_push_input() {
    let _guard = lock();
    init();

    assert_eq!(unsafe { readline::rl_start_kbd_macro(1, 0) }, 0);
    assert_ne!(
        unsafe { readline::rl_readline_state } & RL_STATE_MACRODEF,
        0
    );
    // A second start aborts the definition.
    assert_eq!(unsafe { readline::rl_start_kbd_macro(1, 0) }, 1);
    assert_eq!(
        unsafe { readline::rl_readline_state } & RL_STATE_MACRODEF,
        0
    );

    // No macro defined: printing and replaying report failure without output.
    assert_eq!(unsafe { readline::rl_print_last_kbd_macro(1, 0) }, 0);
    assert_eq!(unsafe { readline::rl_call_last_kbd_macro(1, 0) }, 1);

    let text = CString::new("abc").unwrap();
    // SAFETY: rl_push_macro_input takes ownership of malloc storage.
    let owned = unsafe { libc::strdup(text.as_ptr()) };
    unsafe { readline::rl_push_macro_input(owned) };
    assert_eq!(
        unsafe { CStr::from_ptr(readline::rl_executing_macro) }
            .to_str()
            .unwrap(),
        "abc"
    );
    assert_ne!(
        unsafe { readline::rl_readline_state } & RL_STATE_MACROINPUT,
        0
    );
}

#[test]
fn execute_named_command_uses_macro_input() {
    let _guard = lock();
    init();

    set_line("one two");
    set_point(0);
    let name = CString::new("forward-word\n").unwrap();
    unsafe {
        let owned = libc::strdup(name.as_ptr());
        readline::rl_push_macro_input(owned);
    }
    assert_eq!(unsafe { readline::rl_execute_named_command(1, 0) }, 0);
    assert_eq!(point(), 3);

    let missing = CString::new("no-such-command\n").unwrap();
    unsafe {
        let owned = libc::strdup(missing.as_ptr());
        readline::rl_push_macro_input(owned);
    }
    assert_eq!(unsafe { readline::rl_execute_named_command(1, 0) }, 1);

    // No input available.
    assert_eq!(unsafe { readline::rl_execute_named_command(1, 0) }, 1);
}

#[test]
fn bracketed_paste_and_clipboard() {
    let _guard = lock();
    init();

    set_line("");
    set_point(0);
    // A single queued byte is the whole available paste payload; the scanner
    // stops when the input source is exhausted.
    unsafe { readline::rl_pending_input = 'x' as c_int };
    assert_eq!(unsafe { readline::rl_bracketed_paste_begin(1, 0) }, 0);
    assert_eq!(line(), "x");
    assert_eq!(unsafe { readline::rl_paste_from_clipboard(1, 0) }, 0);
}

#[test]
fn yank_nth_and_last_history_argument() {
    let _guard = lock();
    init();

    unsafe {
        readline::clear_history();
        readline::add_history(CString::new("cmd four five six").unwrap().as_ptr());
        readline::add_history(CString::new("cmd one two three").unwrap().as_ptr());
        readline::using_history();
        readline::rl_free_undo_list();
    }
    set_line("");
    set_point(0);
    assert_eq!(unsafe { readline::rl_yank_nth_arg(1, 0) }, 0);
    assert_eq!(line(), "one");
    assert_eq!(point(), 3);
    assert_eq!(unsafe { readline::rl_mark }, 0);

    set_line("");
    set_point(0);
    assert_eq!(unsafe { readline::rl_yank_nth_arg('$' as c_int, 0) }, 0);
    assert_eq!(line(), "three");

    set_line("");
    set_point(0);
    unsafe { readline::rl_free_undo_list() };
    assert_eq!(unsafe { readline::rl_yank_last_arg(1, 0) }, 0);
    assert_eq!(line(), "three");
    // Repeating undoes the previous yank and walks one entry further back.
    assert_eq!(unsafe { readline::rl_yank_last_arg(1, 0) }, 0);
    assert_eq!(line(), "six");

    // Out-of-range argument dings and inserts nothing.
    set_line("");
    set_point(0);
    assert_eq!(unsafe { readline::rl_yank_nth_arg(9, 0) }, 1);
    assert_eq!(line(), "");
}

#[test]
fn insert_comment_and_close_paren() {
    let _guard = lock();
    init();

    set_line("echo hi");
    set_point(0);
    // SAFETY: serialised ABI call; redisplay is a no-op in this test.
    assert_eq!(unsafe { readline::rl_insert_comment(1, '#' as c_int) }, 0);
    assert_eq!(line(), "#echo hi");
    assert_eq!(point(), 1);
    unsafe {
        readline::rl_done = 0;
        readline::rl_readline_state &= !RL_STATE_DONE;
    }

    set_line("#echo hi");
    set_point(0);
    unsafe { readline::rl_explicit_arg = 1 };
    assert_eq!(unsafe { readline::rl_insert_comment(1, '#' as c_int) }, 0);
    assert_eq!(line(), "echo hi");
    unsafe {
        readline::rl_done = 0;
        readline::rl_readline_state &= !RL_STATE_DONE;
    }

    set_line("");
    set_point(0);
    assert_eq!(unsafe { readline::rl_insert_close(1, ')' as c_int) }, 0);
    assert_eq!(line(), ")");
}

#[test]
fn tilde_expand_current_word() {
    let _guard = lock();
    init();

    let home = std::env::var("HOME").unwrap_or_default();
    set_line("echo ~");
    set_point(6);
    assert_eq!(unsafe { readline::rl_tilde_expand(1, 0) }, 0);
    assert_eq!(line(), format!("echo {home}"));

    set_line("echo ~/src");
    set_point(10);
    assert_eq!(unsafe { readline::rl_tilde_expand(1, 0) }, 0);
    assert_eq!(line(), format!("echo {home}/src"));

    // A word that does not begin with a tilde is untouched.
    set_line("echo plain");
    set_point(10);
    assert_eq!(unsafe { readline::rl_tilde_expand(1, 0) }, 0);
    assert_eq!(line(), "echo plain");
}

#[test]
fn output_flow_and_tty_status_return_zero() {
    let _guard = lock();
    init();

    assert_eq!(unsafe { readline::rl_restart_output(1, 0) }, 0);
    assert_eq!(unsafe { readline::rl_stop_output(1, 0) }, 0);
    assert_eq!(unsafe { readline::rl_tty_status(1, 0) }, 0);
}

#[test]
fn undo_list_is_visible_through_the_abi() {
    let _guard = lock();
    init();

    set_line("ab");
    set_point(2);
    insert("c");
    unsafe {
        assert!(!readline::rl_undo_list.is_null());
        assert_eq!((*readline::rl_undo_list).what, readline::UNDO_INSERT);
        readline::rl_free_undo_list();
        assert!(readline::rl_undo_list.is_null());
    }
}
