//! Rust round-trips for the standalone vi command ABI.
//!
//! The expectations here are the GNU 8.3 bodies in `vi_mode.c`, `kill.c`, and
//! `misc.c`; each test enters vi command mode first so direct command calls
//! see the same keymap GNU's dispatcher would have selected.

use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::sync::{Mutex, MutexGuard};

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn line() -> String {
    // SAFETY: rl_initialize installs a NUL-terminated line buffer.
    unsafe { CStr::from_ptr(readline::rl_line_buffer) }
        .to_string_lossy()
        .into_owned()
}

fn set_line(text: &str) {
    let text = CString::new(text).unwrap();
    // SAFETY: `text` is valid for the call.
    unsafe { readline::rl_replace_line(text.as_ptr(), 0) };
}

fn set_point(value: c_int) {
    // SAFETY: plain scalar write of exported state.
    unsafe { readline::rl_point = value };
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

fn keymap_name() -> Option<String> {
    // SAFETY: the keymap accessors return borrowed static/cached names.
    unsafe {
        let map = readline::rl_get_keymap();
        let name = readline::rl_get_keymap_name(map);
        if name.is_null() {
            None
        } else {
            Some(CStr::from_ptr(name).to_string_lossy().into_owned())
        }
    }
}

fn pending(key: u8) {
    // SAFETY: plain scalar write of exported input state.
    unsafe { readline::rl_pending_input = key as c_int };
}

fn vi_mode() {
    // SAFETY: the test lock serialises access to global ABI state.
    unsafe {
        readline::rl_initialize();
        readline::rl_vi_editing_mode(1, 0);
        readline::rl_vi_movement_mode(1, 0);
    }
}

#[test]
fn modes_and_check_follow_gnu_keymaps() {
    let _guard = lock();
    // SAFETY: the test lock serialises global state.
    unsafe {
        readline::rl_initialize();
        assert_eq!(readline::rl_vi_editing_mode(1, 0), 0);
        assert_eq!(keymap_name().as_deref(), Some("vi-insert"));
        assert_eq!(std::ptr::addr_of!(readline::rl_insert_mode).read(), 1);

        set_line("abc");
        set_point(3);
        assert_eq!(readline::rl_vi_movement_mode(1, 0), 0);
        assert_eq!(keymap_name().as_deref(), Some("vi"));
        assert_eq!(point(), 2);

        set_point(3);
        assert_eq!(readline::rl_vi_check(), 0);
        assert_eq!(point(), 2);
    }
}

#[test]
fn word_motions_match_the_oracle() {
    let _guard = lock();
    vi_mode();

    set_line("one two three");
    set_point(0);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_fword(1, b'w' as c_int), 0);
        assert_eq!(point(), 4);
        assert_eq!(readline::rl_vi_bword(1, b'b' as c_int), 0);
        assert_eq!(point(), 0);
        assert_eq!(readline::rl_vi_eword(1, b'e' as c_int), 0);
        assert_eq!(point(), 2);
        assert_eq!(readline::rl_vi_next_word(1, b'w' as c_int), 0);
        assert_eq!(point(), 4);
        assert_eq!(readline::rl_vi_end_word(1, b'e' as c_int), 0);
        assert_eq!(point(), 6);
        assert_eq!(readline::rl_vi_prev_word(1, b'b' as c_int), 0);
        // From the end of "two" (index 6), `b` lands at the start of
        // that same word (4), not at the start of the line.
        assert_eq!(point(), 4);
    }

    set_line("a.b c");
    set_point(0);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_fWord(1, b'W' as c_int), 0);
        assert_eq!(point(), 4);
        assert_eq!(readline::rl_vi_bWord(1, b'B' as c_int), 0);
        assert_eq!(point(), 0);
        assert_eq!(readline::rl_vi_eWord(1, b'E' as c_int), 0);
        assert_eq!(point(), 2);
    }

    set_line("   abc");
    set_point(0);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_first_print(1, b'^' as c_int), 0);
        assert_eq!(point(), 3);
        assert_eq!(readline::rl_vi_back_to_indent(1, b'^' as c_int), 0);
        assert_eq!(point(), 3);
    }

    set_line("one two");
    set_point(0);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_column(3, b'|' as c_int), 0);
        assert_eq!(point(), 2);
        set_point(7);
        assert_eq!(readline::rl_vi_column(9, b'|' as c_int), 0);
        assert_eq!(point(), 7);
    }
}

#[test]
fn delete_operators_use_vi_ranges() {
    let _guard = lock();
    vi_mode();

    set_line("one two three");
    set_point(0);
    pending(b'w');
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_delete_to(1, b'd' as c_int), 0);
    }
    assert_eq!(line(), "two three");
    assert_eq!((point(), mark()), (0, 4));

    set_line("one two three");
    set_point(0);
    pending(b'e');
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_delete_to(1, b'd' as c_int), 0);
    }
    assert_eq!(line(), " two three");
    assert_eq!((point(), mark()), (0, 3));

    set_line("one two three");
    set_point(4);
    pending(b'b');
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_delete_to(1, b'd' as c_int), 0);
    }
    assert_eq!(line(), "two three");
    assert_eq!((point(), mark()), (0, 4));

    set_line("abcdef");
    set_point(2);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_delete(1, b'x' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("abdef", 2));
    // GNU's undo restores the text and leaves point after it.
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_undo(1, b'u' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("abcdef", 3));
}

#[test]
fn change_insert_and_undo_form_one_unit() {
    let _guard = lock();
    vi_mode();

    set_line("one two");
    set_point(0);
    pending(b'w');
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_change_to(1, b'c' as c_int), 0);
    }
    assert_eq!(line(), " two");
    assert_eq!(keymap_name().as_deref(), Some("vi-insert"));

    // SAFETY: self-insert operates on the exported line buffer.
    unsafe {
        assert_eq!(readline::rl_insert(1, b'O' as c_int), 0);
        assert_eq!(readline::rl_insert(1, b'N' as c_int), 0);
        assert_eq!(readline::rl_insert(1, b'E' as c_int), 0);
        assert_eq!(readline::rl_vi_movement_mode(1, 0), 0);
    }
    assert_eq!((line().as_str(), point()), ("ONE two", 2));

    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_undo(1, b'u' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("one two", 3));
}

#[test]
fn put_yank_and_yank_pop_share_the_vi_kill_ring() {
    let _guard = lock();
    vi_mode();

    set_line("abc");
    set_point(1);
    // SAFETY: kill_text records the kill ring the same way GNU does.
    unsafe {
        readline::rl_kill_text(1, 2);
        readline::rl_vi_movement_mode(1, 0);
    }
    set_line("zz");
    set_point(1);
    // SAFETY: second kill starts a new slot in vi mode.
    unsafe {
        readline::rl_kill_text(1, 2);
        readline::rl_vi_movement_mode(1, 0);
    }

    set_line("q");
    set_point(1);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_put(1, b'p' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("qz", 1));

    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_yank_pop(1, b'p' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("qb", 1));
}

#[test]
fn char_search_and_marks_use_the_pending_key_seam() {
    let _guard = lock();
    vi_mode();

    set_line("a,b,c");
    set_point(0);
    pending(b',');
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_char_search(1, b'f' as c_int), 0);
    }
    assert_eq!(point(), 1);

    // SAFETY: `;` repeats the saved search direction.
    unsafe {
        assert_eq!(readline::rl_vi_char_search(1, b';' as c_int), 0);
    }
    assert_eq!(point(), 3);

    // SAFETY: `,` repeats it in the opposite direction.
    unsafe {
        assert_eq!(readline::rl_vi_char_search(1, b',' as c_int), 0);
    }
    assert_eq!(point(), 1);

    set_line("abcdef");
    set_point(2);
    pending(b'a');
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_set_mark(1, b'm' as c_int), 0);
    }
    set_point(5);
    pending(b'a');
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_goto_mark(1, b'`' as c_int), 0);
    }
    assert_eq!(point(), 2);
}

#[test]
fn change_char_case_and_subst_match_gnu() {
    let _guard = lock();
    vi_mode();

    set_line("abcdef");
    set_point(1);
    pending(b'X');
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_change_char(2, b'r' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("aXXdef", 2));

    set_line("aBc def");
    set_point(0);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_change_case(2, b'~' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("Abc def", 2));

    set_line("one two");
    set_point(0);
    // SAFETY: `s` stuffs an `l` motion through the pending-key seam.
    unsafe {
        assert_eq!(readline::rl_vi_subst(1, b's' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("ne two", 0));

    // SAFETY: return to command mode before the next case.
    unsafe {
        readline::rl_vi_movement_mode(1, 0);
    }
    set_line("one two");
    set_point(4);
    // SAFETY: `S` is `cc`.
    unsafe {
        assert_eq!(readline::rl_vi_subst(1, b'S' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("", 0));
    assert_eq!(keymap_name().as_deref(), Some("vi-insert"));
}

#[test]
fn overstrike_and_replace_build_the_replace_mode_state() {
    let _guard = lock();
    vi_mode();

    set_line("abcde");
    set_point(1);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_overstrike(1, b'X' as c_int), 0);
        assert_eq!(readline::rl_vi_overstrike(1, b'Y' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("aXYde", 3));
    // SAFETY: leaving insert mode backs up over the last replacement.
    unsafe {
        assert_eq!(readline::rl_vi_movement_mode(1, 0), 0);
    }
    assert_eq!((line().as_str(), point()), ("aXYde", 2));

    set_line("abcde");
    set_point(1);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_replace(1, b'R' as c_int), 0);
        assert_eq!(std::ptr::addr_of!(readline::rl_insert_mode).read(), 1);
        assert_eq!(keymap_name(), None);
        assert_eq!(readline::rl_insert(1, b'X' as c_int), 0);
        assert_eq!(readline::rl_insert(1, b'Y' as c_int), 0);
        assert_eq!(readline::rl_vi_movement_mode(1, 0), 0);
    }
    assert_eq!((line().as_str(), point()), ("aXYde", 2));
}

#[test]
fn fetch_history_places_vi_point_and_mark() {
    let _guard = lock();
    vi_mode();

    // SAFETY: the history store is process-global.
    unsafe {
        readline::clear_history();
        readline::add_history(CString::new("cmd one two").unwrap().as_ptr());
        readline::add_history(CString::new("echo alpha beta").unwrap().as_ptr());
        readline::using_history();
    }

    set_line("");
    set_point(0);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        readline::rl_explicit_arg = 1;
        readline::rl_numeric_arg = 1;
        assert_eq!(readline::rl_vi_fetch_history(1, b'G' as c_int), 0);
        readline::rl_explicit_arg = 0;
        readline::rl_numeric_arg = 1;
    }
    assert_eq!(line(), "cmd one two");
    assert_eq!((point(), mark(), end()), (0, 11, 11));
}

#[test]
fn yank_arg_reads_the_previous_history_entry() {
    let _guard = lock();
    vi_mode();

    // SAFETY: the history store is process-global.
    unsafe {
        readline::clear_history();
        readline::add_history(CString::new("cmd one two").unwrap().as_ptr());
        readline::add_history(CString::new("echo alpha beta").unwrap().as_ptr());
        readline::using_history();
    }

    set_line("x");
    set_point(0);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_yank_arg(1, b':' as c_int), 0);
    }
    assert_eq!(line(), "x beta");
    assert_eq!(point(), 6);
    assert_eq!(keymap_name().as_deref(), Some("vi-insert"));
}

#[test]
fn search_consumes_a_pending_pattern_and_repeats_it() {
    let _guard = lock();
    vi_mode();

    // SAFETY: the history store is process-global.
    unsafe {
        readline::clear_history();
        readline::add_history(CString::new("cmd one two").unwrap().as_ptr());
        readline::add_history(CString::new("echo alpha beta").unwrap().as_ptr());
        readline::using_history();
    }

    set_line("");
    pending(b'a');
    // SAFETY: `/` searches older history entries using the pending key as the
    // standalone pattern seam.
    unsafe {
        assert_eq!(readline::rl_vi_search(1, b'/' as c_int), 0);
    }
    assert_eq!(line(), "echo alpha beta");
    assert_eq!(point(), 0);

    // SAFETY: `n` repeats the saved pattern in the reverse direction.
    unsafe {
        assert_eq!(readline::rl_vi_search_again(1, b'n' as c_int), 0);
    }
    // "cmd one two" does not contain `a`, so the failed search leaves the line
    // alone and resets point like GNU's non-incremental search.
    assert_eq!(line(), "echo alpha beta");
    assert_eq!(point(), 0);
}

#[test]
fn tilde_expand_replaces_the_word_and_enters_insert_mode() {
    let _guard = lock();
    vi_mode();

    let previous = std::env::var_os("HOME");
    // SAFETY: the test lock serialises environment mutation in this process.
    std::env::set_var("HOME", "/tmp");
    set_line("~/x");
    set_point(3);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_tilde_expand(1, b'&' as c_int), 0);
    }
    assert_eq!(line(), "/tmp/x");
    assert_eq!(point(), 6);
    assert_eq!(keymap_name().as_deref(), Some("vi-insert"));

    if let Some(previous) = previous {
        std::env::set_var("HOME", previous);
    } else {
        std::env::remove_var("HOME");
    }
}

#[test]
fn redo_replays_the_last_text_modification() {
    let _guard = lock();
    vi_mode();

    set_line("abcdef");
    set_point(0);
    // SAFETY: exported vi commands take (count, key).
    unsafe {
        assert_eq!(readline::rl_vi_delete(1, b'x' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("bcdef", 0));

    // SAFETY: `.` replays the recorded delete command.
    unsafe {
        assert_eq!(readline::rl_vi_redo(1, b'.' as c_int), 0);
    }
    assert_eq!((line().as_str(), point()), ("cdef", 0));
}
