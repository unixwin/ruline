//! Deterministic Rust coverage for the display and saved-state ABI slice.
//!
//! These tests use one global line state, so every case takes the same lock.
//! Output is redirected to a temporary stream where a function would paint.

use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use readline::*;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn c_string(value: &str) -> CString {
    CString::new(value).expect("test strings do not contain NUL")
}

unsafe fn read_int(value: *const c_int) -> c_int {
    value.read()
}

unsafe fn read_ptr<T>(value: *const *mut T) -> *mut T {
    value.read()
}

unsafe fn state_line() -> String {
    CStr::from_ptr(read_ptr(ptr::addr_of!(rl_line_buffer)))
        .to_string_lossy()
        .into_owned()
}

unsafe fn state_display_prompt() -> String {
    CStr::from_ptr(read_ptr(ptr::addr_of!(rl_display_prompt)))
        .to_string_lossy()
        .into_owned()
}

/// Run `action` with `rl_outstream` pointed at a scratch file.
unsafe fn capture_output<T>(action: impl FnOnce() -> T) -> (T, Vec<u8>) {
    let saved = read_ptr(ptr::addr_of!(rl_outstream));
    let file = libc::tmpfile();
    assert!(!file.is_null(), "tmpfile failed");

    rl_outstream = file;
    let result = action();
    libc::fflush(file);
    libc::rewind(file);

    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = libc::fread(chunk.as_mut_ptr() as *mut c_void, 1, chunk.len(), file);
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }

    rl_outstream = saved;
    libc::fclose(file);
    (result, bytes)
}

#[test]
fn state_globals_have_gnu_defaults() {
    let _guard = lock();
    unsafe {
        assert_eq!(rl_initialize(), 0);
        assert_eq!(read_int(ptr::addr_of!(rl_gnu_readline_p)), 1);
        assert_eq!(read_int(ptr::addr_of!(rl_editing_mode)), 1);
        assert_eq!(read_int(ptr::addr_of!(rl_eof_found)), 0);
        assert_eq!(read_int(ptr::addr_of!(rl_dispatching)), 0);
        assert_eq!(read_int(ptr::addr_of!(rl_prefer_env_winsize)), 0);
        assert!(read_ptr(ptr::addr_of!(rl_executing_macro)).is_null());
        assert!(read_ptr(ptr::addr_of!(rl_undo_list)).is_null());
        assert_eq!(read_int(ptr::addr_of!(rl_erase_empty_line)), 0);
        assert_eq!(read_int(ptr::addr_of!(rl_already_prompted)), 0);
        assert_eq!(read_int(ptr::addr_of!(rl_num_chars_to_read)), 0);
        assert!(!read_ptr(ptr::addr_of!(rl_instream)).is_null());
        assert!(!read_ptr(ptr::addr_of!(rl_outstream)).is_null());

        if let Ok(term) = std::env::var("TERM") {
            assert_eq!(
                CStr::from_ptr(ptr::addr_of!(rl_terminal_name).read())
                    .to_str()
                    .unwrap(),
                term.as_str()
            );
        }
    }
}

#[test]
fn character_rendering_matches_gnu() {
    let _guard = lock();
    unsafe {
        assert_eq!(rl_initialize(), 0);
        let (length, bytes) = capture_output(|| rl_show_char(0x01));
        assert_eq!(length, 3);
        assert_eq!(bytes, b"C-A");

        let (length, bytes) = capture_output(|| rl_show_char(0x80));
        assert_eq!(length, 1);
        assert_eq!(bytes, [0x80]);

        let (length, bytes) = capture_output(|| rl_show_char(i32::from(b'\t')));
        assert_eq!(length, 1);
        assert_eq!(bytes, b"\t");

        assert_eq!(rl_character_len(i32::from(b'a'), 0), 1);
        assert_eq!(rl_character_len(0x01, 0), 2);
        assert_eq!(rl_character_len(0x7f, 0), 2);
        assert_eq!(rl_character_len(0x80, 0), 1);
        assert_eq!(rl_character_len(i32::from(b'\t'), 0), 8);
        assert_eq!(rl_character_len(i32::from(b'\t'), 3), 5);
    }
}

#[test]
fn message_round_trip_restores_prompt() {
    let _guard = lock();
    unsafe {
        assert_eq!(rl_initialize(), 0);
        assert_eq!(rl_set_prompt(c_string("$ ").as_ptr()), 0);

        let format = c_string("value=%d/%s");
        let text = c_string("text");
        let (status, _) = capture_output(|| rl_message(format.as_ptr(), 7 as c_int, text.as_ptr()));
        assert_eq!(status, 0);
        assert_eq!(state_display_prompt(), "value=7/text");

        let (status, _) = capture_output(|| rl_clear_message());
        assert_eq!(status, 0);
        assert_eq!(state_display_prompt(), "$ ");
    }
}

#[test]
fn prompt_and_screen_line_helpers() {
    let _guard = lock();
    unsafe {
        assert_eq!(rl_initialize(), 0);
        assert_eq!(rl_set_prompt(c_string("one\ntwo").as_ptr()), 0);

        assert_eq!(rl_on_new_line_with_prompt(), 0);
        assert_eq!(state_display_prompt(), "one\ntwo");

        let (_, _) = capture_output(|| rl_redraw_prompt_last_line());
        assert_eq!(state_display_prompt(), "one\ntwo");

        let (status, bytes) = capture_output(|| rl_clear_screen(0, 0));
        assert_eq!(status, 0);
        assert!(bytes.windows(3).any(|window| window == b"\x1b[H"));
        let (status, _) = capture_output(|| rl_clear_display(0, 0));
        assert_eq!(status, 0);

        rl_set_screen_size(24, 8);
        rl_replace_line(c_string("abcdefghij").as_ptr(), 0);
        rl_point = 0;
        assert_eq!(rl_next_screen_line(1, 0), 0);
        assert_eq!(read_int(ptr::addr_of!(rl_point)), 8);
        assert_eq!(rl_previous_screen_line(1, 0), 0);
        assert_eq!(read_int(ptr::addr_of!(rl_point)), 0);
        rl_reset_screen_size();
    }
}

#[test]
fn arrow_keys_use_pending_input() {
    let _guard = lock();
    unsafe {
        assert_eq!(rl_initialize(), 0);
        rl_replace_line(c_string("abcd").as_ptr(), 0);
        rl_point = 0;

        rl_pending_input = c_int::from(b'C');
        assert_eq!(rl_arrow_keys(1, c_int::from(b'C')), 0);
        assert_eq!(read_int(ptr::addr_of!(rl_point)), 1);

        rl_pending_input = c_int::from(b'D');
        assert_eq!(rl_arrow_keys(1, c_int::from(b'D')), 0);
        assert_eq!(read_int(ptr::addr_of!(rl_point)), 0);
    }
}

static CSI_BYTES: [u8; 3] = *b"12~";
static CSI_LENGTH: AtomicUsize = AtomicUsize::new(3);
static CSI_POSITION: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn csi_getc(_stream: *mut libc::FILE) -> c_int {
    let index = CSI_POSITION.fetch_add(1, Ordering::SeqCst);
    if index >= CSI_LENGTH.load(Ordering::SeqCst) {
        return -1;
    }
    c_int::from(CSI_BYTES[index])
}

#[test]
fn skip_csi_sequence_reads_until_a_final_byte() {
    let _guard = lock();
    unsafe {
        let saved = ptr::addr_of!(rl_getc_function).read();
        rl_getc_function = Some(csi_getc);

        CSI_LENGTH.store(3, Ordering::SeqCst);
        CSI_POSITION.store(0, Ordering::SeqCst);
        assert_eq!(rl_skip_csi_sequence(1, 0), 0);

        CSI_LENGTH.store(1, Ordering::SeqCst);
        CSI_POSITION.store(0, Ordering::SeqCst);
        assert_eq!(rl_skip_csi_sequence(1, 0), 1);

        rl_getc_function = saved;
    }
}

#[test]
fn save_and_restore_round_trip() {
    let _guard = lock();
    unsafe {
        assert_eq!(rl_initialize(), 0);
        assert_eq!(rl_set_prompt(c_string("$ ").as_ptr()), 0);
        rl_replace_line(c_string("snapshot").as_ptr(), 0);
        rl_point = 3;
        rl_done = 0;
        rl_mark = 2;

        let mut saved = std::mem::MaybeUninit::<ReadlineState>::zeroed();
        assert_eq!(rl_save_state(saved.as_mut_ptr()), 0);

        // GNU stores the buffer pointer, not a copy.  Swap in another buffer
        // so restore has to put the saved pointer (with its contents) back.
        let original = read_ptr(ptr::addr_of!(rl_line_buffer));
        let replacement = libc::strdup(c_string("changed").as_ptr());
        assert!(!replacement.is_null());
        rl_line_buffer = replacement;
        rl_end = 7;
        rl_point = 1;
        rl_done = 1;
        rl_mark = 0;

        assert_eq!(rl_restore_state(saved.as_mut_ptr()), 0);
        assert_eq!(read_ptr(ptr::addr_of!(rl_line_buffer)), original);
        assert_eq!(state_line(), "snapshot");
        assert_eq!(read_int(ptr::addr_of!(rl_point)), 3);
        assert_eq!(read_int(ptr::addr_of!(rl_mark)), 2);
        assert_eq!(read_int(ptr::addr_of!(rl_done)), 0);

        libc::free(replacement as *mut c_void);
    }
}
