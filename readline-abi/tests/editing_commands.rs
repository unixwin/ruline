//! Rust round-trips for the standalone editing commands.
//!
//! These tests go through the public ABI (`rl_named_function`, the exported
//! state globals, `rl_replace_line`) rather than internal helpers.  The C
//! smoke test covers the same surface from a C caller; this file keeps the
//! behaviour checked by `cargo test` without needing `cc`.

use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_void};
use std::sync::{Mutex, MutexGuard};

type Command = unsafe extern "C" fn(c_int, c_int) -> c_int;

const RL_STATE_OVERWRITE: std::os::raw::c_ulong = 0x0000_2000;
const RL_STATE_DONE: std::os::raw::c_ulong = 0x0200_0000;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn command(name: &str) -> Command {
    let name = CString::new(name).unwrap();
    // SAFETY: the C string is valid for the call and the name is a default
    // funmap entry.
    unsafe { readline::rl_named_function(name.as_ptr()) }.expect("command is in the default funmap")
}

fn call(name: &str, count: c_int, key: c_int) -> c_int {
    // SAFETY: every command in the default funmap has the bindable signature.
    unsafe { (command(name))(count, key) }
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

fn end() -> c_int {
    // SAFETY: plain scalar read of exported state.
    unsafe { readline::rl_end }
}

fn mark() -> c_int {
    // SAFETY: plain scalar read of exported state.
    unsafe { readline::rl_mark }
}

fn set_point(value: c_int) {
    // SAFETY: plain scalar write of exported state.
    unsafe { readline::rl_point = value };
}

#[test]
fn insert_move_delete_and_mark_round_trip() {
    let _guard = lock();
    // SAFETY: the test lock serialises access to the global ABI state.
    unsafe { readline::rl_initialize() };
    set_line("");

    for byte in b"abc" {
        assert_eq!(call("self-insert", 1, *byte as c_int), 0);
    }
    assert_eq!(line(), "abc");
    assert_eq!((point(), end()), (3, 3));

    assert_eq!(call("backward-char", 1, 0), 0);
    assert_eq!(point(), 2);
    assert_eq!(call("forward-char", 1, 0), 0);
    assert_eq!(point(), 3);
    assert_eq!(call("beginning-of-line", 1, 0), 0);
    assert_eq!(point(), 0);

    set_line("one two");
    assert_eq!(call("forward-word", 1, 0), 0);
    assert_eq!(point(), 3);
    assert_eq!(call("forward-word", 1, 0), 0);
    assert_eq!(point(), 7);
    assert_eq!(call("backward-word", 1, 0), 0);
    assert_eq!(point(), 4);

    set_line("hello");
    set_point(2);
    assert_eq!(call("set-mark", 1, 0), 0);
    assert_eq!(mark(), 2);
    // SAFETY: explicit argument is exported state.
    unsafe { readline::rl_explicit_arg = 1 };
    assert_eq!(call("set-mark", 0, 0), 0);
    assert_eq!((mark(), point()), (0, 2));
    // The wrapper resets the argument after a non-prefix command.
    // SAFETY: plain scalar read.
    assert_eq!(unsafe { readline::rl_explicit_arg }, 0);
    assert_eq!(call("exchange-point-and-mark", 1, 0), 0);
    assert_eq!((point(), mark()), (0, 2));
    // GNU treats a negative mark as "no mark".
    // SAFETY: mark is exported state.
    unsafe { readline::rl_mark = -1 };
    assert_eq!(call("exchange-point-and-mark", 1, 0), 1);
    assert_eq!(mark(), 0);

    set_line("abc");
    set_point(1);
    assert_eq!(call("delete-char", 1, 0), 0);
    assert_eq!((line(), point()), ("ac".to_string(), 1));
    assert_eq!(call("backward-delete-char", 1, 0), 0);
    assert_eq!((line(), point()), ("c".to_string(), 0));
    assert_eq!(call("backward-delete-char", 1, 0), 1);
}

#[test]
fn kill_ring_yank_and_yank_pop_rotate() {
    let _guard = lock();
    // SAFETY: the test lock serialises access to the global ABI state.
    unsafe { readline::rl_initialize() };
    // End any kill sequence left by an earlier test, so the first kill below
    // starts a fresh ring slot.
    assert_eq!(call("beginning-of-line", 1, 0), 0);

    set_line("one two");
    assert_eq!(call("kill-word", 1, 0), 0);
    assert_eq!(line(), " two");
    // A non-kill command separates the two kills into different ring slots.
    assert_eq!(call("self-insert", 1, b'x' as c_int), 0);

    set_line("three four");
    set_point(0);
    assert_eq!(call("kill-word", 1, 0), 0);
    assert_eq!(line(), " four");

    assert_eq!(call("yank", 1, 0), 0);
    assert_eq!(line(), "three four");
    assert_eq!(call("yank-pop", 1, 0), 0);
    assert_eq!(line(), "one four");
    assert_eq!(point(), 3);

    set_line("hello world");
    set_point(5);
    assert_eq!(call("kill-line", 1, 0), 0);
    assert_eq!((line(), point()), ("hello".to_string(), 5));
    assert_eq!(call("yank", 1, 0), 0);
    assert_eq!(line(), "hello world");

    set_line("foo bar");
    set_point(7);
    assert_eq!(call("unix-word-rubout", 1, 0), 0);
    assert_eq!((line(), point()), ("foo ".to_string(), 4));

    set_line("abc");
    set_point(3);
    assert_eq!(call("unix-line-discard", 1, 0), 0);
    assert_eq!((line(), point()), (String::new(), 0));

    set_line("abc");
    set_point(1);
    // SAFETY: mark is exported state.
    unsafe { readline::rl_mark = 3 };
    assert_eq!(call("kill-region", 1, 0), 0);
    assert_eq!((line(), point()), ("a".to_string(), 1));
}

#[test]
fn case_transpose_overwrite_and_numeric_arguments() {
    let _guard = lock();
    // SAFETY: the test lock serialises access to the global ABI state.
    unsafe { readline::rl_initialize() };

    set_line("ab");
    set_point(2);
    assert_eq!(call("transpose-chars", 1, 0), 0);
    assert_eq!((line(), point()), ("ba".to_string(), 2));

    set_line("hello world");
    set_point(0);
    assert_eq!(call("upcase-word", 1, 0), 0);
    assert_eq!(line(), "HELLO world");
    assert_eq!(point(), 5);

    set_line("hello wORLD");
    set_point(0);
    assert_eq!(call("capitalize-word", 2, 0), 0);
    assert_eq!((line(), point()), ("Hello World".to_string(), 11));

    set_line("abc");
    set_point(1);
    assert_eq!(call("overwrite-mode", 1, 0), 0);
    // SAFETY: insert mode and readline state are exported.
    assert_eq!(unsafe { readline::rl_insert_mode }, 0);
    assert_ne!(
        unsafe { readline::rl_readline_state } & RL_STATE_OVERWRITE,
        0
    );
    assert_eq!(call("self-insert", 1, b'X' as c_int), 0);
    assert_eq!(line(), "aXc");
    assert_eq!(call("overwrite-mode", 1, 0), 0);
    // SAFETY: insert mode is exported state.
    assert_eq!(unsafe { readline::rl_insert_mode }, 1);

    assert_eq!(call("digit-argument", 1, b'3' as c_int), 0);
    // SAFETY: numeric argument globals are exported.
    assert_eq!(
        (unsafe { readline::rl_numeric_arg }, unsafe {
            readline::rl_explicit_arg
        }),
        (3, 1)
    );
    assert_eq!(call("beginning-of-line", 1, 0), 0);
    assert_eq!(
        (unsafe { readline::rl_numeric_arg }, unsafe {
            readline::rl_explicit_arg
        }),
        (1, 0)
    );
    assert_eq!(call("universal-argument", 1, 0), 0);
    // SAFETY: numeric argument globals are exported.
    assert_eq!(unsafe { readline::rl_numeric_arg }, 4);

    set_line("");
    // SAFETY: pending input is exported state.
    unsafe { readline::rl_pending_input = b'Q' as c_int };
    assert_eq!(call("quoted-insert", 1, 0), 0);
    assert_eq!(line(), "Q");
    // SAFETY: pending input is exported state.
    assert_eq!(unsafe { readline::rl_pending_input }, 0);

    // SAFETY: done/state are exported.
    assert_eq!(unsafe { readline::rl_done }, 0);
    assert_eq!(call("accept-line", 1, 0), 0);
    // SAFETY: done/state are exported.
    assert_eq!(unsafe { readline::rl_done }, 1);
    assert_ne!(unsafe { readline::rl_readline_state } & RL_STATE_DONE, 0);
}

#[test]
fn add_defun_resolves_the_same_real_body() {
    let _guard = lock();
    // SAFETY: the test lock serialises access to the global ABI state.
    unsafe { readline::rl_initialize() };

    let forward = command("forward-char");
    let alias = CString::new("ruline-forward-char").unwrap();
    // SAFETY: the alias name is valid for the call.
    assert_eq!(
        unsafe { readline::rl_add_defun(alias.as_ptr(), Some(forward), -1) },
        0
    );
    // SAFETY: the alias name is valid for the call.
    let resolved = unsafe { readline::rl_named_function(alias.as_ptr()) };
    assert!(resolved.is_some_and(|resolved| std::ptr::fn_addr_eq(resolved, forward)));

    set_line("abc");
    set_point(0);
    // SAFETY: `resolved` is the bindable forward-char command.
    assert_eq!(unsafe { resolved.unwrap()(1, 0) }, 0);
    assert_eq!(point(), 1);
}

#[test]
fn exported_commands_are_the_funmap_bodies() {
    let _guard = lock();
    // SAFETY: the test lock serialises access to the global ABI state.
    unsafe { readline::rl_initialize() };

    let pairs: [(&str, Command); 12] = [
        ("self-insert", readline::rl_insert),
        ("accept-line", readline::rl_newline),
        ("backward-delete-char", readline::rl_rubout),
        ("delete-char", readline::rl_delete),
        ("beginning-of-line", readline::rl_beg_of_line),
        ("end-of-line", readline::rl_end_of_line),
        ("forward-char", readline::rl_forward_char),
        ("backward-char", readline::rl_backward_char),
        ("forward-word", readline::rl_forward_word),
        ("kill-line", readline::rl_kill_line),
        ("yank", readline::rl_yank),
        ("transpose-chars", readline::rl_transpose_chars),
    ];
    for (name, exported) in pairs {
        let resolved = command(name);
        assert!(
            std::ptr::fn_addr_eq(resolved, exported),
            "{name} is not the exported symbol"
        );
    }
}

#[test]
fn direct_movement_insert_delete_abi() {
    let _guard = lock();
    // SAFETY: the test lock serialises access to the global ABI state.
    unsafe { readline::rl_initialize() };

    set_line("one two three");
    set_point(0);
    // SAFETY: exported commands take (count, key).
    assert_eq!(unsafe { readline::rl_forward_word(1, 0) }, 0);
    assert_eq!(point(), 3);
    assert_eq!(unsafe { readline::rl_forward(2, 0) }, 0);
    assert_eq!(point(), 5);
    assert_eq!(unsafe { readline::rl_backward(1, 0) }, 0);
    assert_eq!(point(), 4);
    assert_eq!(unsafe { readline::rl_beg_of_line(1, 0) }, 0);
    assert_eq!(point(), 0);
    assert_eq!(unsafe { readline::rl_end_of_line(1, 0) }, 0);
    assert_eq!(point(), 13);
    assert_eq!(unsafe { readline::rl_backward_byte(1, 0) }, 0);
    assert_eq!(point(), 12);
    assert_eq!(unsafe { readline::rl_forward_byte(1, 0) }, 0);
    assert_eq!(point(), 13);

    set_line("ac");
    set_point(1);
    // SAFETY: exported commands take (count, key).
    assert_eq!(unsafe { readline::rl_insert(1, b'b' as c_int) }, 0);
    assert_eq!((line(), point()), ("abc".to_string(), 2));
    assert_eq!(unsafe { readline::rl_tab_insert(1, 0) }, 0);
    assert_eq!(line(), "ab\tc");
    assert_eq!(unsafe { readline::rl_delete(1, 0) }, 0);
    assert_eq!((line(), point()), ("ab\t".to_string(), 3));
    assert_eq!(unsafe { readline::rl_rubout(1, 0) }, 0);
    assert_eq!((line(), point()), ("ab".to_string(), 2));
    assert_eq!(unsafe { readline::rl_rubout_or_delete(1, 0) }, 0);
    assert_eq!((line(), point()), ("a".to_string(), 1));
}

#[test]
fn direct_mark_region_and_predicates() {
    let _guard = lock();
    // SAFETY: the test lock serialises access to the global ABI state.
    unsafe { readline::rl_initialize() };

    // SAFETY: mark-active state is process-wide.
    unsafe { readline::rl_deactivate_mark() };
    assert_eq!(unsafe { readline::rl_mark_active_p() }, 0);
    // SAFETY: mark-active state is process-wide.
    unsafe { readline::rl_activate_mark() };
    assert_eq!(unsafe { readline::rl_mark_active_p() }, 1);
    // SAFETY: mark-active state is process-wide.
    unsafe { readline::rl_keep_mark_active() };
    assert_eq!(unsafe { readline::rl_mark_active_p() }, 1);
    unsafe { readline::rl_deactivate_mark() };
    assert_eq!(unsafe { readline::rl_mark_active_p() }, 0);

    set_line("hello");
    set_point(0);
    unsafe { readline::rl_mark = 3 };
    // Exchange activates the region in GNU's text.c.
    assert_eq!(unsafe { readline::rl_exchange_point_and_mark(1, 0) }, 0);
    assert_eq!((point(), mark()), (3, 0));
    assert_eq!(unsafe { readline::rl_mark_active_p() }, 1);

    // SAFETY: predicates do not touch the line.
    unsafe {
        assert_eq!(readline::rl_alphabetic(b'a' as c_int), 1);
        assert_eq!(readline::rl_alphabetic(b'0' as c_int), 1);
        assert_eq!(readline::rl_alphabetic(b' ' as c_int), 0);
        assert_eq!(readline::alphabetic(b'Z' as c_int), 1);
        assert_eq!(readline::ding(), 0);
    }

    let copy = unsafe { readline::rl_copy_text(0, 2) };
    assert!(!copy.is_null());
    // SAFETY: `copy` came from rl_copy_text's malloc and is handed back once.
    unsafe { readline::rl_free(copy as *mut c_void) };
}

#[test]
fn exported_kill_copy_and_yank_share_the_ring() {
    let _guard = lock();
    // SAFETY: the test lock serialises access to the global ABI state.
    unsafe { readline::rl_initialize() };
    // rl_initialize does not end a kill sequence (GNU neither), so a kill
    // left by whichever test ran before would make the first kill below
    // append to its ring entry; end it the way a non-kill command does.
    assert_eq!(unsafe { readline::rl_beg_of_line(1, 0) }, 0);

    set_line("one two");
    set_point(0);
    // SAFETY: exported commands take (count, key).
    assert_eq!(unsafe { readline::rl_kill_word(1, 0) }, 0);
    assert_eq!(line(), " two");
    // A non-kill command ends the kill sequence, matching dispatch.
    assert_eq!(unsafe { readline::rl_beg_of_line(1, 0) }, 0);

    set_line("three four");
    set_point(0);
    assert_eq!(unsafe { readline::rl_kill_word(1, 0) }, 0);
    assert_eq!(line(), " four");
    assert_eq!(unsafe { readline::rl_yank(1, 0) }, 0);
    assert_eq!(line(), "three four");
    assert_eq!(unsafe { readline::rl_yank_pop(1, 0) }, 0);
    assert_eq!(line(), "one four");
    assert_eq!(point(), 3);

    set_line("abcdef");
    set_point(1);
    unsafe { readline::rl_mark = 4 };
    assert_eq!(unsafe { readline::rl_copy_region_to_kill(1, 0) }, 0);
    assert_eq!((line(), point(), mark()), ("abcdef".to_string(), 1, 4));
    assert_eq!(unsafe { readline::rl_kill_region(1, 0) }, 0);
    assert_eq!((line(), point()), ("aef".to_string(), 1));

    set_line("abc");
    set_point(2);
    assert_eq!(unsafe { readline::rl_unix_line_discard(1, 0) }, 0);
    assert_eq!((line(), point()), ("c".to_string(), 0));

    set_line("src/foo.c");
    set_point(9);
    assert_eq!(unsafe { readline::rl_unix_filename_rubout(1, 0) }, 0);
    assert_eq!((line(), point()), ("src/".to_string(), 4));
}

#[test]
fn modes_newline_and_argument_discard_are_exported() {
    let _guard = lock();
    // SAFETY: the test lock serialises access to the global ABI state.
    unsafe { readline::rl_initialize() };

    let editing = CString::new("editing-mode").unwrap();
    let vi = CString::new("vi").unwrap();
    assert_eq!(
        unsafe { readline::rl_variable_bind(editing.as_ptr(), vi.as_ptr()) },
        0
    );
    assert_eq!(unsafe { readline::rl_emacs_editing_mode(1, 0) }, 0);
    assert_eq!(unsafe { readline::rl_insert_mode }, 1);
    let name = unsafe { readline::rl_get_keymap_name(readline::rl_get_keymap()) };
    assert_eq!(unsafe { CStr::from_ptr(name) }.to_bytes(), b"emacs");

    // SAFETY: numeric-argument globals are exported.
    unsafe {
        readline::rl_explicit_arg = 1;
        readline::rl_numeric_arg = 7;
    }
    assert_eq!(unsafe { readline::rl_discard_argument() }, 0);
    // SAFETY: plain scalar reads through raw pointers avoid borrowing the
    // mutable statics.
    unsafe {
        assert_eq!(std::ptr::addr_of!(readline::rl_explicit_arg).read(), 0);
        assert_eq!(std::ptr::addr_of!(readline::rl_numeric_arg).read(), 1);
    }
    assert_eq!(
        unsafe { readline::rl_do_lowercase_version(1, b'A' as c_int) },
        99999
    );

    unsafe extern "C" fn no_redisplay() {}
    // SAFETY: the test lock serialises process-wide hook state.
    let previous = unsafe { readline::rl_redisplay_function };
    unsafe { readline::rl_redisplay_function = Some(no_redisplay) };
    unsafe {
        readline::rl_activate_mark();
        readline::rl_done = 0;
        assert_eq!(readline::rl_newline(1, 0), 0);
        assert_eq!(std::ptr::addr_of!(readline::rl_done).read(), 1);
        assert_eq!(readline::rl_mark_active_p(), 0);
    }
    unsafe { readline::rl_redisplay_function = previous };
}
