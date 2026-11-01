//! Rust round-trips for the history navigation and search commands.
//!
//! The commands touch process-wide history and saved-line state, so this file
//! drives one ordered scenario in a single test.  Run in its own process by
//! Cargo, it needs no lock against the other integration tests.

use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::sync::{Mutex, MutexGuard};

type Command = unsafe extern "C" fn(c_int, c_int) -> c_int;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn command(name: &str) -> Command {
    let name = CString::new(name).unwrap();
    // SAFETY: the name is a NUL-terminated default funmap entry.
    unsafe { readline::rl_named_function(name.as_ptr()) }.expect("command is in the default funmap")
}

fn call(name: &str, count: c_int) -> c_int {
    // SAFETY: every command in the default funmap has the bindable signature.
    unsafe { (command(name))(count, 0) }
}

fn set_line(text: &str) {
    let text = CString::new(text).unwrap();
    // SAFETY: `text` is a valid NUL-terminated string.
    unsafe { readline::rl_replace_line(text.as_ptr(), 0) };
    // rl_replace_line only clamps point; these commands start from the end.
    unsafe { readline::rl_point = readline::rl_end };
}

fn line() -> String {
    // SAFETY: rl_initialize installs a NUL-terminated line buffer.
    unsafe { CStr::from_ptr(readline::rl_line_buffer) }
        .to_string_lossy()
        .into_owned()
}

fn add(line: &str) {
    let line = CString::new(line).unwrap();
    // SAFETY: the string is valid for the call.
    unsafe { readline::add_history(line.as_ptr()) };
}

fn history_line(number: c_int) -> String {
    // SAFETY: history_get returns NULL or a valid entry.
    let entry = unsafe { readline::history_get(number) };
    assert!(!entry.is_null(), "history entry {number} is missing");
    // SAFETY: entry->line is NUL-terminated while history is unchanged.
    unsafe { CStr::from_ptr((*entry).line) }
        .to_string_lossy()
        .into_owned()
}

fn queue_one(key: c_int) {
    // SAFETY: rl_pending_input is the documented next-key seam.
    unsafe { readline::rl_pending_input = key };
}

/// Feed printable keys one at a time.  Each invocation consumes the one key
/// in `rl_pending_input`; with no key left the search stays active.
fn feed(name: &str, keys: &[u8]) {
    for &key in keys {
        queue_one(key as c_int);
        assert_eq!(call(name, 1), 0);
    }
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

#[test]
fn history_navigation_and_search_commands() {
    let _guard = lock();
    // SAFETY: this test owns the process-wide ABI state.
    unsafe { readline::rl_initialize() };

    // --- previous/next history, saved line, and edit write-back ----------
    unsafe { readline::clear_history() };
    add("one");
    add("two");
    add("three");
    unsafe { readline::using_history() };

    set_line("draft");
    assert_eq!(call("previous-history", 1), 0);
    assert_eq!(line(), "three");
    assert_eq!((point(), end(), mark()), (5, 5, 0));

    set_line("THREE");
    assert_eq!(call("next-history", 1), 0);
    assert_eq!(line(), "draft");
    assert_eq!(point(), 5);
    assert_eq!(history_line(3), "THREE");

    assert_eq!(call("previous-history", 1), 0);
    assert_eq!(line(), "THREE");
    assert_eq!(call("beginning-of-history", 1), 0);
    assert_eq!(line(), "one");
    assert_eq!(call("end-of-history", 1), 0);
    assert_eq!(line(), "draft");

    // --- fetch-history -----------------------------------------------------
    set_line("draft");
    // SAFETY: explicit argument is exported command state.
    unsafe { readline::rl_explicit_arg = 1 };
    assert_eq!(call("fetch-history", 2), 0);
    assert_eq!(line(), "two");
    assert_eq!(call("end-of-history", 1), 0);
    assert_eq!(line(), "draft");
    assert_eq!(call("fetch-history", 1), 0);
    assert_eq!(line(), "one");
    assert_eq!(call("end-of-history", 1), 0);

    // --- history-search-backward/forward and substring variants -----------
    unsafe { readline::clear_history() };
    add("alpha one");
    add("beta two");
    add("alphabet");
    add("gamma alpha");
    unsafe { readline::using_history() };

    set_line("al");
    assert_eq!(call("history-search-backward", 1), 0);
    assert_eq!(line(), "alphabet");
    assert_eq!((point(), mark()), (2, 8));
    assert_eq!(call("history-search-backward", 1), 0);
    assert_eq!(line(), "alpha one");
    assert_eq!(call("history-search-forward", 1), 0);
    assert_eq!(line(), "alphabet");

    unsafe { readline::using_history() };
    set_line("alpha");
    assert_eq!(call("history-substring-search-backward", 1), 0);
    assert_eq!(line(), "gamma alpha");
    assert_eq!((point(), mark()), (6, 11));

    // --- non-incremental search driven through rl_pending_input -----------
    unsafe { readline::clear_history() };
    add("alpha one");
    add("beta two");
    add("alphabet");
    add("gamma alpha");
    unsafe { readline::using_history() };

    set_line("draft");
    feed("non-incremental-reverse-search-history", b"alphabet");
    queue_one(b'\n' as c_int);
    assert_eq!(call("non-incremental-reverse-search-history", 1), 0);
    assert_eq!(line(), "alphabet");
    assert_eq!((point(), mark()), (0, 8));

    // The next older line does not contain the full search string.
    assert_eq!(call("non-incremental-reverse-search-history-again", 1), 1);
    assert_eq!(line(), "alphabet");
    assert_eq!(point(), 0);

    unsafe { readline::using_history() };
    unsafe { readline::history_set_pos(0) };
    set_line("draft");
    feed("non-incremental-forward-search-history", b"gamma");
    queue_one(b'\n' as c_int);
    assert_eq!(call("non-incremental-forward-search-history", 1), 0);
    assert_eq!(line(), "gamma alpha");

    // --- incremental reverse search ---------------------------------------
    set_line("draft");
    unsafe { readline::using_history() };
    assert_eq!(call("reverse-search-history", 1), 0);
    assert_eq!(line(), "");
    feed("reverse-search-history", b"alph");
    queue_one(b'\n' as c_int);
    assert_eq!(call("reverse-search-history", 1), 0);
    assert_eq!(line(), "gamma alpha");
    assert_eq!(point(), 6);

    // C-g aborts and restores the line where the search started.
    unsafe { readline::using_history() };
    set_line("draft");
    assert_eq!(call("reverse-search-history", 1), 0);
    queue_one(7);
    assert_eq!(call("reverse-search-history", 1), 1);
    assert_eq!(line(), "draft");

    // --- operate-and-get-next accepts the line and arms the startup hook ---
    unsafe { readline::clear_history() };
    add("one");
    add("two");
    add("three");
    unsafe { readline::using_history() };
    set_line("three");
    assert_eq!(call("operate-and-get-next", 1), 0);
    // SAFETY: plain scalar read of exported state.
    assert_eq!(unsafe { readline::rl_done }, 1);

    // --- rl_clear_history preserves history_base ---------------------------
    unsafe { readline::clear_history() };
    let base = unsafe { readline::history_base };
    unsafe { readline::rl_clear_history() };
    assert_eq!(unsafe { readline::history_length }, 0);
    assert_eq!(unsafe { readline::history_base }, base);
}

#[test]
fn clear_history_is_not_a_funmap_name() {
    let _guard = lock();
    let name = CString::new("clear-history").unwrap();
    // SAFETY: the name is NUL-terminated.
    assert!(unsafe { readline::rl_named_function(name.as_ptr()) }.is_none());
}
