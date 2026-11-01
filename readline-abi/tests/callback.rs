//! Behavioural tests for the callback / terminal / signal slice.
//!
//! These run in their own test process, so the ABI globals are not shared with
//! the unit tests.  Input is an in-memory byte stream exposed through
//! `rl_getc_function`; no PTY is needed.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use readline::*;

const RL_STATE_TERMPREPPED: std::os::raw::c_ulong = 0x0000_0004;
const RL_STATE_SIGHANDLER: std::os::raw::c_ulong = 0x0000_8000;
const RL_STATE_INPUTPENDING: std::os::raw::c_ulong = 0x0002_0000;
const RL_STATE_CALLBACK: std::os::raw::c_ulong = 0x0008_0000;
const RL_STATE_EOF: std::os::raw::c_ulong = 0x0800_0000;

static TEST_LOCK: Mutex<()> = Mutex::new(());

static INPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static COMPLETED: Mutex<Vec<Option<String>>> = Mutex::new(Vec::new());
static COMPLETION_STATE: Mutex<Vec<(bool, bool)>> = Mutex::new(Vec::new());

static REDISPLAY_CALLS: AtomicUsize = AtomicUsize::new(0);
static PREP_CALLS: AtomicUsize = AtomicUsize::new(0);
static DEPREP_CALLS: AtomicUsize = AtomicUsize::new(0);
static EVENT_CALLS: AtomicUsize = AtomicUsize::new(0);
static AVAILABLE_AFTER: AtomicUsize = AtomicUsize::new(0);
static APP_SIGINT_CALLS: AtomicUsize = AtomicUsize::new(0);
static APP_SAW_SIGHANDLER: AtomicBool = AtomicBool::new(false);

fn lock<'a>() -> std::sync::MutexGuard<'a, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn feed(bytes: &[u8]) {
    INPUT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .extend_from_slice(bytes);
}

unsafe extern "C" fn stream_getc(_stream: *mut libc::FILE) -> c_int {
    let mut input = INPUT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match input.first().copied() {
        Some(byte) => {
            input.remove(0);
            c_int::from(byte)
        }
        None => libc::EOF,
    }
}

unsafe extern "C" fn line_handler(line: *mut c_char) {
    let text = if line.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(line) }
                .to_string_lossy()
                .into_owned(),
        )
    };
    if !line.is_null() {
        // SAFETY: the callback API hands ownership to the application.
        unsafe { libc::free(line as *mut c_void) };
    }
    let state = unsafe { ptr::addr_of!(rl_readline_state).read() };
    COMPLETED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(text);
    COMPLETION_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((rl_done != 0, state & RL_STATE_EOF != 0));
}

unsafe extern "C" fn counting_redisplay() {
    REDISPLAY_CALLS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn counting_prep(_meta: c_int) {
    PREP_CALLS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn counting_deprep() {
    DEPREP_CALLS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn counting_event_hook() -> c_int {
    EVENT_CALLS.fetch_add(1, Ordering::SeqCst);
    0
}

unsafe extern "C" fn availability_hook() -> c_int {
    (EVENT_CALLS.load(Ordering::SeqCst) >= AVAILABLE_AFTER.load(Ordering::SeqCst)) as c_int
}

#[cfg(unix)]
unsafe extern "C" fn app_sigint_handler(_signal: c_int) {
    APP_SIGINT_CALLS.fetch_add(1, Ordering::SeqCst);
    let state = unsafe { ptr::addr_of!(rl_readline_state).read() };
    APP_SAW_SIGHANDLER.store(state & RL_STATE_SIGHANDLER != 0, Ordering::SeqCst);
}

fn completed_lines() -> Vec<Option<String>> {
    COMPLETED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn completion_states() -> Vec<(bool, bool)> {
    COMPLETION_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn line_text() -> String {
    let buffer = unsafe { ptr::addr_of!(rl_line_buffer).read() };
    if buffer.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(buffer) }
            .to_string_lossy()
            .into_owned()
    }
}

fn readline_state() -> std::os::raw::c_ulong {
    unsafe { ptr::addr_of!(rl_readline_state).read() }
}

/// Restore a clean callback/terminal environment before each test.
fn reset() {
    // SAFETY: called under TEST_LOCK, so no other test touches these globals.
    unsafe {
        rl_callback_handler_remove();
        ptr::addr_of_mut!(rl_getc_function).write(None);
        ptr::addr_of_mut!(rl_redisplay_function).write(Some(counting_redisplay));
        ptr::addr_of_mut!(rl_prep_term_function).write(Some(counting_prep));
        ptr::addr_of_mut!(rl_deprep_term_function).write(Some(counting_deprep));
        ptr::addr_of_mut!(rl_event_hook).write(None);
        ptr::addr_of_mut!(rl_input_available_hook).write(None);
        ptr::addr_of_mut!(rl_pending_input).write(0);
        ptr::addr_of_mut!(rl_done).write(0);
        ptr::addr_of_mut!(rl_point).write(0);
        ptr::addr_of_mut!(rl_end).write(0);
    }
    INPUT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    COMPLETED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    COMPLETION_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    REDISPLAY_CALLS.store(0, Ordering::SeqCst);
    PREP_CALLS.store(0, Ordering::SeqCst);
    DEPREP_CALLS.store(0, Ordering::SeqCst);
    EVENT_CALLS.store(0, Ordering::SeqCst);
    AVAILABLE_AFTER.store(0, Ordering::SeqCst);
    unsafe {
        ptr::addr_of_mut!(rl_getc_function).write(Some(stream_getc));
        rl_initialize();
    }
}

fn finish() {
    unsafe {
        rl_callback_handler_remove();
        rl_clear_signals();
        ptr::addr_of_mut!(rl_getc_function).write(None);
        ptr::addr_of_mut!(rl_redisplay_function).write(Some(rl_redisplay));
        ptr::addr_of_mut!(rl_prep_term_function).write(None);
        ptr::addr_of_mut!(rl_deprep_term_function).write(None);
        ptr::addr_of_mut!(rl_event_hook).write(None);
        ptr::addr_of_mut!(rl_input_available_hook).write(None);
    }
}

fn install(prompt: &[u8]) {
    let prompt = std::ffi::CString::new(prompt).unwrap();
    unsafe { rl_callback_handler_install(prompt.as_ptr(), Some(line_handler)) };
}

fn read_char() {
    unsafe { rl_callback_read_char() };
}

#[test]
fn callback_completes_a_line_from_bytes() {
    let _guard = lock();
    reset();

    feed(b"helX\x7flo\n");
    install(b"> ");
    assert!(readline_state() & RL_STATE_CALLBACK != 0);
    assert!(REDISPLAY_CALLS.load(Ordering::SeqCst) >= 1);
    assert_eq!(PREP_CALLS.load(Ordering::SeqCst), 1);

    for _ in 0..8 {
        read_char();
    }

    assert_eq!(completed_lines(), vec![Some(String::from("hello"))]);
    let states = completion_states();
    assert_eq!(states.len(), 1);
    assert!(states[0].0, "rl_done is set while the handler runs");
    assert!(!states[0].1, "an accepted line is not EOF");

    // The callback prints the next prompt and clears the line.
    assert_eq!(line_text(), "");
    assert_eq!(unsafe { ptr::addr_of!(rl_done).read() }, 0);
    assert!(readline_state() & RL_STATE_CALLBACK != 0);
    assert_eq!(PREP_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(DEPREP_CALLS.load(Ordering::SeqCst), 1);

    // Reinstalling replaces the handler and starts a new line.
    install(b"$ ");
    assert_eq!(completed_lines().len(), 1);
    feed(b"z\n");
    read_char();
    read_char();
    assert_eq!(
        completed_lines(),
        vec![Some(String::from("hello")), Some(String::from("z"))]
    );

    unsafe { rl_callback_handler_remove() };
    assert_eq!(readline_state() & RL_STATE_CALLBACK, 0);
    assert_eq!(
        PREP_CALLS.load(Ordering::SeqCst),
        DEPREP_CALLS.load(Ordering::SeqCst)
    );
    finish();
}

#[test]
fn ctrl_d_is_eof_and_ctrl_c_aborts() {
    let _guard = lock();
    reset();
    install(b"> ");

    feed(b"\x04");
    read_char();
    assert_eq!(completed_lines(), vec![None]);
    assert_eq!(completion_states()[0], (true, true));

    feed(b"\x03");
    read_char();
    assert_eq!(completed_lines(), vec![None, None]);
    assert_eq!(completion_states()[1], (true, false));
    assert_eq!(line_text(), "");

    // Ctrl-D on a non-empty line is delete-char, not EOF.
    feed(b"abc\x04\x04\n");
    for _ in 0..6 {
        read_char();
    }
    assert_eq!(
        completed_lines(),
        vec![None, None, Some(String::from("abc"))]
    );

    finish();
}

#[test]
fn pending_input_is_replayed() {
    let _guard = lock();
    reset();
    install(b"> ");

    unsafe {
        ptr::addr_of_mut!(rl_pending_input).write(b'x' as c_int);
        ptr::addr_of_mut!(rl_readline_state).write(readline_state() | RL_STATE_INPUTPENDING);
    }
    read_char();
    assert_eq!(line_text(), "x");
    assert_eq!(unsafe { ptr::addr_of!(rl_pending_input).read() }, 0);
    assert_eq!(readline_state() & RL_STATE_INPUTPENDING, 0);

    // Finish the pending-input line, then queue EOF on the fresh line.
    feed(b"\n");
    read_char();
    assert_eq!(completed_lines(), vec![Some(String::from("x"))]);

    unsafe { ptr::addr_of_mut!(rl_pending_input).write(libc::EOF) };
    read_char();
    assert_eq!(completed_lines(), vec![Some(String::from("x")), None]);
    assert_eq!(completion_states()[1], (true, true));

    finish();
}

#[test]
fn event_and_availability_hooks_are_polled() {
    let _guard = lock();
    reset();
    AVAILABLE_AFTER.store(2, Ordering::SeqCst);
    unsafe {
        ptr::addr_of_mut!(rl_event_hook).write(Some(counting_event_hook));
        ptr::addr_of_mut!(rl_input_available_hook).write(Some(availability_hook));
    }

    feed(b"e\n");
    install(b"> ");

    read_char();
    assert_eq!(EVENT_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(line_text(), "e");

    read_char();
    assert_eq!(EVENT_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(completed_lines(), vec![Some(String::from("e"))]);

    finish();
}

#[test]
fn terminal_prep_deprep_round_trip() {
    let _guard = lock();
    reset();
    unsafe {
        ptr::addr_of_mut!(rl_prep_term_function).write(None);
        ptr::addr_of_mut!(rl_deprep_term_function).write(None);
    }

    let mut rows = 0;
    let mut columns = 0;
    unsafe { rl_get_screen_size(&mut rows, &mut columns) };
    assert!(rows > 0 && columns > 0);

    unsafe { rl_set_screen_size(41, 123) };
    let mut pinned_rows = 0;
    let mut pinned_columns = 0;
    unsafe { rl_get_screen_size(&mut pinned_rows, &mut pinned_columns) };
    assert_eq!((pinned_rows, pinned_columns), (41, 123));
    unsafe { rl_reset_screen_size() };

    let is_tty = unsafe { libc::isatty(0) } == 1;
    unsafe {
        rl_prep_terminal(0);
        rl_prep_terminal(0);
    }
    assert_eq!(readline_state() & RL_STATE_TERMPREPPED != 0, is_tty);
    assert_eq!(unsafe { ptr::addr_of!(rl_echoing_p).read() }, 1);

    unsafe {
        rl_deprep_terminal();
        rl_deprep_terminal();
    }
    assert_eq!(readline_state() & RL_STATE_TERMPREPPED, 0);

    finish();
}

#[cfg(unix)]
#[test]
fn signal_handling_sets_state_bits_and_aborts_the_line() {
    let _guard = lock();
    reset();
    install(b"> ");

    // Install the application's SIGINT handler, then let rl_set_signals save
    // it and surface the library's state bits while it is called.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = app_sigint_handler as *const () as libc::sighandler_t;
    action.sa_flags = libc::SA_RESTART;
    let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        assert_eq!(libc::sigaction(libc::SIGINT, &action, &mut previous), 0);
        assert_eq!(rl_set_signals(), 0);
        assert_eq!(libc::raise(libc::SIGINT), 0);
    }

    read_char();
    assert_eq!(APP_SIGINT_CALLS.load(Ordering::SeqCst), 1);
    assert!(APP_SAW_SIGHANDLER.load(Ordering::SeqCst));
    assert_eq!(completed_lines(), vec![None]);
    assert_eq!(completion_states()[0], (true, false));

    // SIGWINCH refreshes the screen and repaints through the hook.
    REDISPLAY_CALLS.store(0, Ordering::SeqCst);
    unsafe {
        assert_eq!(rl_set_signals(), 0);
        assert_eq!(libc::raise(libc::SIGWINCH), 0);
        ptr::addr_of_mut!(rl_pending_input).write(b'w' as c_int);
    }
    read_char();
    assert!(REDISPLAY_CALLS.load(Ordering::SeqCst) >= 1);
    assert_eq!(line_text(), "w");

    unsafe {
        rl_clear_signals();
        libc::sigaction(libc::SIGINT, &previous, ptr::null_mut());
    }
    finish();
}
