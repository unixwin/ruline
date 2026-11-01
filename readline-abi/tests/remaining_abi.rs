//! Round-trips for the remaining keymap/completion/terminal/signal ABI slice.
//!
//! Every test locks the process-wide readline state.  Run the file in its own
//! process (Cargo does) or with `--test-threads=1` like the C smoke test.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

type Command = unsafe extern "C" fn(c_int, c_int) -> c_int;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn c_string(value: &str) -> CString {
    CString::new(value).unwrap()
}

fn line() -> String {
    // SAFETY: the library owns a NUL-terminated line buffer after initialize.
    unsafe { CStr::from_ptr(readline::rl_line_buffer) }
        .to_string_lossy()
        .into_owned()
}

fn set_line(text: &str) {
    let text = c_string(text);
    // SAFETY: the string is NUL-terminated for the call.
    unsafe { readline::rl_replace_line(text.as_ptr(), 0) };
    unsafe { readline::rl_point = readline::rl_end };
}

fn command(name: &str) -> Command {
    let name = c_string(name);
    // SAFETY: default funmap names are NUL-terminated and stable.
    unsafe { readline::rl_named_function(name.as_ptr()) }.expect("command exists")
}

fn strdup(value: &[u8]) -> *mut c_char {
    // SAFETY: malloc-compatible allocation for an ABI string.
    unsafe {
        let pointer = libc::malloc(value.len() + 1) as *mut u8;
        if pointer.is_null() {
            return ptr::null_mut();
        }
        ptr::copy_nonoverlapping(value.as_ptr(), pointer, value.len());
        *pointer.add(value.len()) = 0;
        pointer as *mut c_char
    }
}

fn take_string(pointer: *mut c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: pointers handed to and from the ABI are NUL-terminated.
    let value = unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned();
    unsafe { libc::free(pointer as *mut libc::c_void) };
    value
}

// --- Completion fixtures --------------------------------------------------

static NAMES: &[&str] = &["alpha", "alpine", "altitude"];

extern "C" fn generator(text: *const c_char, state_value: c_int) -> *mut c_char {
    let text = unsafe { CStr::from_ptr(text) }.to_bytes();
    let mut matches: Vec<&str> = NAMES
        .iter()
        .copied()
        .filter(|name| name.as_bytes().starts_with(text))
        .collect();
    if state_value < 0 || state_value as usize >= matches.len() {
        return ptr::null_mut();
    }
    let name = matches.swap_remove(state_value as usize);
    strdup(name.as_bytes())
}

extern "C" fn filename_generator(text: *const c_char, state_value: c_int) -> *mut c_char {
    unsafe { readline::rl_filename_completion_desired = 1 };
    generator(text, state_value)
}

static REWRITE_CALLS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn rewrite_hook(input: *mut c_char, _length: c_int) -> *mut c_char {
    REWRITE_CALLS.fetch_add(1, Ordering::SeqCst);
    input
}

fn hook_directory() -> &'static Mutex<Option<CString>> {
    static DIRECTORY: OnceLock<Mutex<Option<CString>>> = OnceLock::new();
    DIRECTORY.get_or_init(|| Mutex::new(None))
}

extern "C" fn directory_completion_hook(dirname: *mut *mut c_char) -> c_int {
    let guard = hook_directory()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(replacement) = guard.as_ref() else {
        return 0;
    };
    if dirname.is_null() {
        return 0;
    }
    let replacement = replacement.clone();
    unsafe {
        if !(*dirname).is_null() {
            libc::free(*dirname as *mut libc::c_void);
        }
        *dirname = libc::strdup(replacement.as_ptr());
    }
    1
}

extern "C" fn ignore_long_matches(matches: *mut *mut c_char) -> c_int {
    if matches.is_null() {
        return 0;
    }
    // Keep slot 0 (the LCD) and the first match; drop the rest in the same
    // shifted-array form GNU's ignore hook produces.
    let mut index = 2usize;
    loop {
        let entry = unsafe { *matches.add(index) };
        if entry.is_null() {
            unsafe { *matches.add(2) = ptr::null_mut() };
            return 0;
        }
        unsafe { libc::free(entry as *mut libc::c_void) };
        index += 1;
    }
}

static MACRO_HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);
static MACRO_HOOK_TEXT: Mutex<String> = Mutex::new(String::new());

extern "C" fn macro_display(
    key: *const c_char,
    value: *const c_char,
    _readable: c_int,
    prefix: *const c_char,
) {
    MACRO_HOOK_CALLS.fetch_add(1, Ordering::SeqCst);
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy();
    let value = unsafe { CStr::from_ptr(value) }.to_string_lossy();
    let prefix = if prefix.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(prefix) }
            .to_string_lossy()
            .into_owned()
    };
    *MACRO_HOOK_TEXT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = format!("{prefix}|{key}|{value}");
}

extern "C" fn eof_getc(_stream: *mut libc::FILE) -> c_int {
    libc::EOF
}

extern "C" fn zero_hook() -> c_int {
    0
}

// --- Tests ----------------------------------------------------------------

#[test]
fn keymap_funmap_binding_and_argument_trimming() {
    let _guard = lock();
    // SAFETY: the test owns the process-wide ABI state.
    unsafe { readline::rl_initialize() };

    let funmap = unsafe { ptr::addr_of!(readline::funmap).read() };
    assert!(!funmap.is_null());
    let mut entries = 0usize;
    let self_insert = command("self-insert");
    let mut found_self_insert = false;
    loop {
        let entry = unsafe { *funmap.add(entries) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).name) }.to_string_lossy();
        if name == "self-insert" {
            found_self_insert = true;
            assert_eq!(
                unsafe { (*entry).function }.map(|function| function as usize),
                Some(self_insert as usize)
            );
        }
        entries += 1;
    }
    assert!(entries >= 145);
    assert!(found_self_insert);

    let map = unsafe { readline::rl_make_bare_keymap() };
    assert!(!map.is_null());
    let probe = command("beginning-of-line");
    let single = c_string("Q");
    assert_eq!(
        unsafe { readline::rl_set_key(single.as_ptr(), Some(probe), map) },
        0
    );
    assert_eq!(
        unsafe { readline::rl_binding_keymap },
        map,
        "a single-key binding leaves the root map as the binding keymap"
    );
    let keyseq = c_string("\\C-xy");
    assert_eq!(
        unsafe { readline::rl_set_key(keyseq.as_ptr(), Some(probe), map) },
        0
    );
    assert!(
        !unsafe { readline::rl_binding_keymap }.is_null(),
        "a multi-key binding records the keymap holding the final key"
    );

    // u 5 x: the numeric prefix occupies the first two bytes.  Bind the
    // prefix command explicitly so the map does not depend on the default
    // GNU emacs bindings (where C-u is unix-line-discard).
    let universal = command("universal-argument");
    assert_eq!(
        unsafe { readline::rl_bind_key_in_map('u' as c_int, Some(universal), map) },
        0
    );
    let trimmed = [b'u', b'5', b'x'];
    let result = unsafe {
        readline::rl_trim_arg_from_keyseq(trimmed.as_ptr() as *const c_char, trimmed.len(), map)
    };
    assert_eq!(result, 2);
    let plain = b"x";
    assert_eq!(
        unsafe {
            readline::rl_trim_arg_from_keyseq(plain.as_ptr() as *const c_char, plain.len(), map)
        },
        0
    );
    // A key sequence that is only a numeric argument is not trimmed.
    let only_argument = [b'u', b'5'];
    assert_eq!(
        unsafe {
            readline::rl_trim_arg_from_keyseq(
                only_argument.as_ptr() as *const c_char,
                only_argument.len(),
                map,
            )
        },
        -1
    );

    // The callback-style dispatch seam records the map/key pair.
    assert_eq!(unsafe { readline::rl_executing_key }, 0);
    assert!(unsafe { readline::rl_executing_keymap }.is_null());
    unsafe { readline::rl_free_keymap(map) };
}

#[test]
fn completion_legacy_aliases_hooks_menu_and_export() {
    let _guard = lock();
    // SAFETY: the test owns the process-wide ABI state.
    unsafe { readline::rl_initialize() };
    unsafe {
        readline::rl_completion_entry_function = Some(generator);
        readline::rl_menu_completion_entry_function = None;
        readline::rl_attempted_completion_function = None;
        readline::rl_directory_rewrite_hook = None;
        readline::rl_directory_completion_hook = None;
        readline::rl_completion_rewrite_hook = None;
        readline::rl_ignore_some_completions_function = None;
        readline::rl_filename_rewrite_hook = None;
        readline::rl_last_func = None;
    }

    // Legacy completion_matches spelling.
    let text = c_string("al");
    let matches =
        unsafe { readline::completion_matches(text.as_ptr() as *mut c_char, Some(generator)) };
    assert!(!matches.is_null());
    let mut names = Vec::new();
    let mut index = 0usize;
    loop {
        let item = unsafe { *matches.add(index) };
        if item.is_null() {
            break;
        }
        names.push(
            unsafe { CStr::from_ptr(item) }
                .to_string_lossy()
                .into_owned(),
        );
        unsafe { libc::free(item as *mut libc::c_void) };
        index += 1;
    }
    unsafe { libc::free(matches as *mut libc::c_void) };
    assert!(names.iter().any(|name| name == "alpha"));
    assert!(names.iter().any(|name| name == "alpine"));

    // Legacy username/filename spellings delegate to the real generators.
    let missing = c_string("~__ruline_no_such_user");
    assert!(unsafe { readline::username_completion_function(missing.as_ptr(), 0) }.is_null());

    let dir = std::env::temp_dir().join(format!("ruline-remaining-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("alpha.txt"), b"a").unwrap();
    std::fs::write(dir.join("alpine.txt"), b"b").unwrap();
    let old_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let prefix = c_string("al");
    let mut found = Vec::new();
    for state in 0.. {
        let item = unsafe { readline::filename_completion_function(prefix.as_ptr(), state) };
        if item.is_null() {
            break;
        }
        found.push(take_string(item));
    }
    assert!(found.iter().any(|name| name.ends_with("alpha.txt")));

    // rl_completion_rewrite_hook runs over the non-empty filename component.
    REWRITE_CALLS.store(0, Ordering::SeqCst);
    unsafe { readline::rl_completion_rewrite_hook = Some(rewrite_hook) };
    let mut rewritten = Vec::new();
    for state in 0.. {
        let item = unsafe { readline::rl_filename_completion_function(prefix.as_ptr(), state) };
        if item.is_null() {
            break;
        }
        rewritten.push(take_string(item));
    }
    assert!(REWRITE_CALLS.load(Ordering::SeqCst) > 0);
    assert!(rewritten.iter().any(|name| name.ends_with("alpha.txt")));
    unsafe { readline::rl_completion_rewrite_hook = None };

    // rl_directory_completion_hook replaces the directory and is reflected in
    // the returned paths.
    *hook_directory()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(c_string(&dir.to_string_lossy()));
    unsafe { readline::rl_directory_completion_hook = Some(directory_completion_hook) };
    let empty = c_string("");
    let mut hooked = Vec::new();
    for state in 0.. {
        let item = unsafe { readline::rl_filename_completion_function(empty.as_ptr(), state) };
        if item.is_null() {
            break;
        }
        hooked.push(take_string(item));
    }
    assert!(hooked.iter().any(|name| name.contains("alpha.txt")));
    unsafe { readline::rl_directory_completion_hook = None };
    std::env::set_current_dir(&old_cwd).unwrap();

    // Old-style menu completion cycles through the generated names.
    unsafe {
        readline::rl_menu_completion_entry_function = Some(generator);
        readline::rl_last_func = None;
    }
    set_line("al");
    assert_eq!(unsafe { readline::rl_old_menu_complete(1, 0) }, 0);
    assert!(line().starts_with("alpha"), "first menu entry: {}", line());
    unsafe { readline::rl_last_func = Some(readline::rl_old_menu_complete) };
    assert_eq!(unsafe { readline::rl_old_menu_complete(1, 0) }, 0);
    assert!(
        line().starts_with("alpine"),
        "second menu entry: {}",
        line()
    );
    assert_eq!(unsafe { readline::rl_old_menu_complete(-1, 0) }, 0);
    assert!(
        line().starts_with("alpha"),
        "wrapped menu entry: {}",
        line()
    );
    unsafe { readline::rl_last_func = None };

    // rl_export_completions writes the count/text/range and matches, and the
    // ignore hook removes entries before export.
    unsafe {
        readline::rl_completion_entry_function = Some(filename_generator);
        readline::rl_ignore_some_completions_function = Some(ignore_long_matches);
    }
    set_line("al");
    let capture = unsafe { libc::tmpfile() };
    assert!(!capture.is_null());
    let saved_stream = unsafe { readline::rl_outstream };
    unsafe { readline::rl_outstream = capture };
    assert_eq!(unsafe { readline::rl_export_completions(1, 0) }, 0);
    unsafe { readline::rl_outstream = saved_stream };
    unsafe { libc::fflush(capture) };
    unsafe { libc::rewind(capture) };
    let mut output = vec![0u8; 512];
    let read = unsafe {
        libc::fread(
            output.as_mut_ptr() as *mut libc::c_void,
            1,
            output.len(),
            capture,
        )
    };
    unsafe { libc::fclose(capture) };
    output.truncate(read);
    let output = String::from_utf8_lossy(&output).into_owned();
    assert!(output.contains("al\n"), "export text: {output:?}");
    assert!(output.contains("0:2"), "export range: {output:?}");
    assert!(output.contains("alpha"), "export match: {output:?}");
    assert!(
        !output.contains("altitude"),
        "ignore hook removed a match: {output:?}"
    );
    unsafe { readline::rl_ignore_some_completions_function = None };

    // Export clears the line with count > 1.
    set_line("al");
    let capture = unsafe { libc::tmpfile() };
    unsafe { readline::rl_outstream = capture };
    assert_eq!(unsafe { readline::rl_export_completions(2, 0) }, 0);
    unsafe { readline::rl_outstream = saved_stream };
    unsafe { libc::fclose(capture) };
    assert_eq!(line(), "");

    // rl_vi_complete completes the current word and enters insert mode.
    unsafe {
        readline::rl_completion_entry_function = Some(generator);
        readline::rl_completion_append_character = b' ' as c_int;
        readline::rl_emacs_editing_mode(1, 0);
    }
    set_line("a");
    unsafe { readline::rl_point = 0 };
    assert_eq!(unsafe { readline::rl_vi_complete(1, b'\\' as c_int) }, 0);
    assert!(line().starts_with("al"), "vi completion: {}", line());
    assert_eq!(
        unsafe { readline::rl_get_keymap() },
        ptr::addr_of_mut!(readline::vi_insertion_keymap).cast()
    );
    unsafe { readline::rl_emacs_editing_mode(1, 0) };
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn terminal_input_timeout_and_capability_surface() {
    let _guard = lock();
    // SAFETY: the test owns the process-wide ABI state.
    unsafe { readline::rl_initialize() };

    assert_eq!(unsafe { readline::rl_stuff_char('x' as c_int) }, 1);
    assert_eq!(unsafe { readline::rl_read_key() }, 'x' as c_int);

    assert_eq!(unsafe { readline::rl_execute_next('y' as c_int) }, 0);
    assert_eq!(unsafe { readline::rl_pending_input }, 'y' as c_int);
    assert_eq!(unsafe { readline::rl_read_key() }, 'y' as c_int);
    assert_eq!(unsafe { readline::rl_pending_input }, 0);

    // EOF is stuffed as an EOF pending key followed by a newline, as in GNU.
    assert_eq!(unsafe { readline::rl_stuff_char(libc::EOF) }, 1);
    assert_eq!(unsafe { readline::rl_read_key() }, libc::EOF);
    assert_eq!(unsafe { readline::rl_read_key() }, '\n' as c_int);

    // rl_getc reads from an arbitrary FILE; the timeout is only started by a
    // read and reports -1 before that.
    let file = unsafe { libc::tmpfile() };
    assert!(!file.is_null());
    unsafe {
        libc::fputc('a' as c_int, file);
        libc::fputc('b' as c_int, file);
        libc::rewind(file);
    }
    assert_eq!(unsafe { readline::rl_getc(file) }, 'a' as c_int);
    assert_eq!(unsafe { readline::rl_getc(file) }, 'b' as c_int);
    assert_eq!(unsafe { readline::rl_getc(file) }, libc::EOF);
    unsafe { libc::fclose(file) };

    let mut secs = 0u32;
    let mut usecs = 0u32;
    assert_eq!(unsafe { readline::rl_set_timeout(1, 0) }, 0);
    assert_eq!(
        unsafe { readline::rl_timeout_remaining(&mut secs, &mut usecs) },
        -1
    );

    let saved_getc = unsafe { readline::rl_getc_function };
    unsafe { readline::rl_getc_function = Some(eof_getc) };
    assert_eq!(unsafe { readline::rl_read_key() }, libc::EOF);
    assert_eq!(
        unsafe { readline::rl_timeout_remaining(&mut secs, &mut usecs) },
        1
    );
    unsafe { readline::rl_getc_function = saved_getc };
    assert_eq!(unsafe { readline::rl_set_timeout(0, 0) }, 0);
    assert_eq!(
        unsafe { readline::rl_timeout_remaining(&mut secs, &mut usecs) },
        -1
    );

    let old_echo = unsafe { readline::rl_tty_set_echoing(0) };
    assert_eq!(unsafe { readline::rl_echoing_p }, 0);
    unsafe { readline::rl_tty_set_echoing(old_echo) };

    let old_blink = unsafe { readline::rl_set_paren_blink_timeout(1234) };
    assert_eq!(old_blink, 500_000);
    assert_eq!(unsafe { readline::rl_set_paren_blink_timeout(0) }, 1234);

    // Unknown capabilities are NULL; an ANSI description provides the common
    // key sequences.  Reset to TERM afterwards for the other tests.
    let unknown = c_string("zz");
    assert!(unsafe { readline::rl_get_termcap(unknown.as_ptr()) }.is_null());
    let xterm = c_string("xterm");
    assert_eq!(unsafe { readline::rl_reset_terminal(xterm.as_ptr()) }, 0);
    let ku = c_string("ku");
    let cap = unsafe { readline::rl_get_termcap(ku.as_ptr()) };
    assert!(!cap.is_null());
    assert_eq!(unsafe { *cap }, 0x1b);
    assert_eq!(unsafe { readline::rl_reset_terminal(ptr::null()) }, 0);

    let standard = ptr::addr_of_mut!(readline::emacs_standard_keymap).cast();
    unsafe { readline::rl_tty_set_default_bindings(standard) };
    unsafe { readline::rl_tty_unset_default_bindings(standard) };
    unsafe { readline::rl_echo_signal_char(libc::SIGINT) };
    unsafe { readline::rl_reparse_colors() };
    unsafe { readline::rl_reparse_colors() };
}

#[test]
fn signal_toggles_and_hooks_are_real_state() {
    let _guard = lock();
    // SAFETY: the test owns the process-wide ABI state.
    unsafe { readline::rl_initialize() };

    unsafe {
        readline::rl_catch_signals = 1;
        readline::rl_catch_sigwinch = 1;
        readline::rl_change_environment = 1;
        readline::rl_persistent_signal_handlers = 0;
    }
    assert_eq!(unsafe { readline::rl_pending_signal() }, 0);
    unsafe { readline::rl_check_signals() };

    let mut saved: readline::ReadlineState = unsafe { std::mem::zeroed() };
    unsafe {
        readline::rl_catch_signals = 0;
        readline::rl_catch_sigwinch = 0;
    }
    assert_eq!(unsafe { readline::rl_save_state(&mut saved) }, 0);
    assert_eq!(saved.catchsigs, 0);
    assert_eq!(saved.catchsigwinch, 0);
    unsafe {
        readline::rl_catch_signals = 1;
        readline::rl_catch_sigwinch = 1;
    }
    assert_eq!(unsafe { readline::rl_restore_state(&mut saved) }, 0);
    assert_eq!(unsafe { readline::rl_catch_signals }, 0);
    assert_eq!(unsafe { readline::rl_catch_sigwinch }, 0);

    // Catch toggles are honoured by the install/restore entry points.
    unsafe {
        readline::rl_catch_signals = 0;
        readline::rl_catch_sigwinch = 0;
    }
    assert_eq!(unsafe { readline::rl_set_signals() }, 0);
    assert_eq!(unsafe { readline::rl_clear_signals() }, 0);
    unsafe {
        readline::rl_catch_signals = 1;
        readline::rl_catch_sigwinch = 1;
    }

    assert!(unsafe { readline::rl_macro_display_hook }.is_none());
    assert!(unsafe { readline::rl_signal_event_hook }.is_none());
    assert!(unsafe { readline::rl_timeout_event_hook }.is_none());

    unsafe {
        readline::rl_signal_event_hook = Some(zero_hook);
        readline::rl_timeout_event_hook = Some(zero_hook);
    }
    assert!(unsafe { readline::rl_signal_event_hook }.is_some());
    assert!(unsafe { readline::rl_timeout_event_hook }.is_some());
    unsafe {
        readline::rl_signal_event_hook = None;
        readline::rl_timeout_event_hook = None;
    }

    // The macro display hook replaces rl_macro_dumper's default output.
    MACRO_HOOK_CALLS.store(0, Ordering::SeqCst);
    let keyseq = c_string("\\C-xz");
    let value = c_string("hi");
    unsafe {
        readline::rl_macro_bind(keyseq.as_ptr(), value.as_ptr(), ptr::null_mut());
        readline::rl_macro_display_hook = Some(macro_display);
        readline::rl_macro_dumper(1);
        readline::rl_macro_display_hook = None;
    }
    assert!(MACRO_HOOK_CALLS.load(Ordering::SeqCst) >= 1);
    assert!(MACRO_HOOK_TEXT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains("hi"));
}
