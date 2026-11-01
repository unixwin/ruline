//! Rust round-trips for the standalone editing commands.
//!
//! These tests go through the public ABI (`rl_named_function`, the exported
//! state globals, `rl_replace_line`) rather than internal helpers.  The C
//! smoke test covers the same surface from a C caller; this file keeps the
//! behaviour checked by `cargo test` without needing `cc`.

use std::ffi::{CStr, CString};
use std::os::raw::c_int;
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
