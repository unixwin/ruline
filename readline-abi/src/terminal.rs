//! Terminal preparation, screen-size queries, and signal handling.
//!
//! This module is the clean-room counterpart of GNU readline's `rltty.c`,
//! `terminal.c`, and `signals.c` for the slice this crate implements.  The
//! vendored GNU tree is the behavioural oracle:
//!
//! * `rl_prep_terminal` puts the terminal into character-at-a-time mode,
//!   keeping `ISIG` enabled so control characters can still generate signals.
//! * `rl_deprep_terminal` restores the exact settings saved by the matching
//!   prepare call and is idempotent.
//! * `rl_get_screen_size`/`rl_set_screen_size` expose the cached dimensions;
//!   the cache is filled from `TIOCGWINSZ` (falling back to `COLUMNS`/`LINES`
//!   and finally 80x24) and refreshed on `SIGWINCH`.
//! * `rl_set_signals`/`rl_clear_signals` install and restore the `SIGINT` and
//!   `SIGWINCH` dispositions.  The other signals GNU catches (SIGTERM, SIGHUP,
//!   SIGQUIT, SIGALRM, SIGTSTP, SIGTTIN, SIGTTOU) are not installed by this
//!   slice yet; that is tracked as remaining work, not silently ignored.
//!
//! # Platform fallback
//!
//! Only Unix has `termios` and POSIX signal dispositions.  On non-Unix builds
//! `rl_prep_terminal` records that echoing is on and leaves the state bit
//! clear (there is no terminal mode to change), the screen-size source is the
//! `COLUMNS`/`LINES` environment plus the 80x24 default, and
//! `rl_set_signals`/`rl_clear_signals` are documented no-ops that return 0.
//! This mirrors the shared parts of GNU's `NO_TTY_DRIVER` build.

use std::collections::VecDeque;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint, c_ulong};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::completion::{RlHookFunc, RlVintFunc, RlVoidFunc};
use crate::state;

// --- RL_STATE bits --------------------------------------------------------

// Values are fixed by the GNU ABI (`readline.h`).  They live here because
// this module owns the phases that are only visible during terminal/signal
// processing; `line.rs` keeps the lifecycle bits it already exported.
pub(crate) const RL_STATE_TERMPREPPED: c_ulong = 0x0000_0004;
pub(crate) const RL_STATE_READCMD: c_ulong = 0x0000_0008;
pub(crate) const RL_STATE_DISPATCHING: c_ulong = 0x0000_0020;
pub(crate) const RL_STATE_MOREINPUT: c_ulong = 0x0000_0040;
pub(crate) const RL_STATE_SIGHANDLER: c_ulong = 0x0000_8000;
pub(crate) const RL_STATE_INPUTPENDING: c_ulong = 0x0002_0000;
pub(crate) const RL_STATE_TTYCSAVED: c_ulong = 0x0004_0000;
pub(crate) const RL_STATE_CALLBACK: c_ulong = 0x0008_0000;
pub(crate) const RL_STATE_MULTIKEY: c_ulong = 0x0020_0000;
pub(crate) const RL_STATE_DONE: c_ulong = 0x0200_0000;
pub(crate) const RL_STATE_TIMEOUT: c_ulong = 0x0400_0000;
pub(crate) const RL_STATE_EOF: c_ulong = 0x0800_0000;

/// Add bits to `rl_readline_state`.
pub(crate) fn add_state(bits: c_ulong) {
    state::set_readline_state(state::readline_state() | bits);
}

/// Remove bits from `rl_readline_state`.
pub(crate) fn remove_state(bits: c_ulong) {
    state::set_readline_state(state::readline_state() & !bits);
}

pub(crate) fn state_is_set(bits: c_ulong) -> bool {
    state::readline_state() & bits != 0
}

// --- Signal and environment policy ---------------------------------------

/// GNU installs its signal handlers by default.
#[no_mangle]
pub static mut rl_catch_signals: c_int = 1;
/// GNU installs a SIGWINCH handler by default where the signal exists.
#[no_mangle]
pub static mut rl_catch_sigwinch: c_int = 1;
/// Update LINES/COLUMNS in the environment when the screen size changes.
#[no_mangle]
pub static mut rl_change_environment: c_int = 1;
/// Keep readline's handlers installed across callback reads.
#[no_mangle]
pub static mut rl_persistent_signal_handlers: c_int = 0;

fn catch_signals() -> c_int {
    unsafe { ptr::addr_of!(rl_catch_signals).read() }
}

fn catch_sigwinch() -> c_int {
    unsafe { ptr::addr_of!(rl_catch_sigwinch).read() }
}

pub(crate) fn persistent_signal_handlers() -> bool {
    unsafe { ptr::addr_of!(rl_persistent_signal_handlers).read() != 0 }
}

fn change_environment() -> bool {
    unsafe { ptr::addr_of!(rl_change_environment).read() != 0 }
}

fn signal_event_hook() -> RlHookFunc {
    unsafe { ptr::addr_of!(crate::completion::rl_signal_event_hook).read() }
}

fn timeout_event_hook() -> RlHookFunc {
    unsafe { ptr::addr_of!(crate::completion::rl_timeout_event_hook).read() }
}

fn getc_function() -> Option<unsafe extern "C" fn(*mut libc::FILE) -> c_int> {
    unsafe { ptr::addr_of!(crate::completion::rl_getc_function).read() }
}

// --- Stuffed input ---------------------------------------------------------

/// GNU's typeahead ring holds 511 bytes (`sizeof (ibuffer) - 1`).
const IBUFFER_LEN: usize = 511;

static STUFFED: Mutex<VecDeque<c_int>> = Mutex::new(VecDeque::new());

pub(crate) fn pop_stuffed() -> Option<c_int> {
    STUFFED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .pop_front()
}

pub(crate) fn stuffed_available() -> bool {
    !STUFFED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty()
}

/// Queue KEY for the next `rl_read_key`.  Returns 1 when queued, 0 when the
/// ring is full.  EOF queues a newline and leaves EOF in `rl_pending_input`,
/// exactly as GNU's `rl_stuff_char`.
#[no_mangle]
pub unsafe extern "C" fn rl_stuff_char(key: c_int) -> c_int {
    let mut queue = STUFFED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if queue.len() >= IBUFFER_LEN {
        return 0;
    }

    let mut key = key;
    if key == libc::EOF {
        key = b'\n' as c_int;
        state::set_pending_input(libc::EOF);
        add_state(RL_STATE_INPUTPENDING);
    }
    if key == 0 {
        // A NUL key is stored as a literal byte; it is not the end marker.
        key = 0;
    }
    queue.push_back(key);
    1
}

/// Make `key` the next command, through the `rl_pending_input` path.
#[no_mangle]
pub unsafe extern "C" fn rl_execute_next(key: c_int) -> c_int {
    state::set_pending_input(key);
    add_state(RL_STATE_INPUTPENDING);
    0
}

// --- Timeout state ---------------------------------------------------------

static TIMEOUT_DURATION: Mutex<Option<Duration>> = Mutex::new(None);
static TIMEOUT_DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);

/// Set the timeout for the next read.  `(0, 0)` clears it.  The deadline is
/// started by `rl_read_key`/`rl_getc`, mirroring GNU's `_rl_timeout_init`.
#[no_mangle]
pub unsafe extern "C" fn rl_set_timeout(secs: c_uint, usecs: c_uint) -> c_int {
    let secs = u64::from(secs) + u64::from(usecs) / 1_000_000;
    let nanos = u64::from(usecs % 1_000_000) * 1_000;
    let mut duration = TIMEOUT_DURATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut deadline = TIMEOUT_DEADLINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if secs == 0 && nanos == 0 {
        *duration = None;
        *deadline = None;
    } else {
        *duration = Some(Duration::new(secs, nanos as u32));
        *deadline = None;
    }
    0
}

fn arm_timeout() {
    let duration = *TIMEOUT_DURATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(duration) = duration {
        let mut deadline = TIMEOUT_DEADLINE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if deadline.is_none() {
            *deadline = Some(Instant::now() + duration);
        }
    }
}

fn timeout_expired() -> bool {
    let deadline = *TIMEOUT_DEADLINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

/// Remaining time until the armed timeout.  Returns -1 when no timeout has
/// been started, 0 when it has expired, and 1 with the remaining time
/// otherwise.  The output parameters are only written when both are non-NULL,
/// matching GNU.
#[no_mangle]
pub unsafe extern "C" fn rl_timeout_remaining(secs: *mut c_uint, usecs: *mut c_uint) -> c_int {
    let deadline = *TIMEOUT_DEADLINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(deadline) = deadline else {
        return -1;
    };
    let now = Instant::now();
    if now >= deadline {
        return 0;
    }
    if !secs.is_null() && !usecs.is_null() {
        let remaining = deadline - now;
        *secs = remaining.as_secs() as c_uint;
        *usecs = remaining.subsec_micros();
    }
    1
}

#[cfg(unix)]
fn timeout_timeval() -> Option<libc::timeval> {
    let keyboard = state::keyboard_timeout();
    let keyboard_usecs = if keyboard > 0 {
        Some(keyboard as u64)
    } else {
        None
    };

    let deadline = *TIMEOUT_DEADLINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let micros = match deadline {
        Some(deadline) => {
            let now = Instant::now();
            if now >= deadline {
                return Some(libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                });
            }
            let remaining = (deadline - now).as_micros() as u64;
            Some(keyboard_usecs.map_or(remaining, |keyboard| keyboard.min(remaining)))
        }
        None => keyboard_usecs,
    };

    micros.map(|micros| libc::timeval {
        tv_sec: (micros / 1_000_000) as libc::time_t,
        tv_usec: (micros % 1_000_000) as libc::suseconds_t,
    })
}

fn abort_after_timeout() {
    if let Some(hook) = timeout_event_hook() {
        // SAFETY: the application owns this callback and promised the ABI.
        unsafe { hook() };
    }
    add_state(RL_STATE_TIMEOUT);
    // GNU's `_rl_abort_internal` clears transient input state.  This adapter
    // has no longjmp target, so the read returns EOF after the cleanup; the
    // caller treats that as the aborted line (documented dispatcher seam).
    unsafe { rl_clear_pending_input() };
    state::reset_argument();
    crate::macros::kill_kbd_macro();
    remove_state(RL_STATE_MULTIKEY);
    state::set_last_func(None);
}

// --- Character input -------------------------------------------------------

/// GNU's `READERR` return from `rl_getc` while a command is being read.
const READERR: c_int = -2;

/// Read one key, honouring queued input and macros before the input stream.
///
/// Order follows GNU `rl_read_key`: `rl_pending_input`, then macro input, then
/// the typeahead ring (`rl_stuff_char`), then `rl_getc_function` or `rl_getc`.
/// The timeout configured by `rl_set_timeout` is started here.
#[no_mangle]
pub unsafe extern "C" fn rl_read_key() -> c_int {
    let pending = state::pending_input();
    if pending != 0 {
        state::set_pending_input(0);
        remove_state(RL_STATE_INPUTPENDING);
        return pending;
    }

    let macro_key = crate::macros::next_macro_key();
    if macro_key != 0 {
        return c_int::from(macro_key as u8);
    }

    if let Some(key) = pop_stuffed() {
        return key;
    }

    arm_timeout();

    if let Some(getc) = getc_function() {
        let stream = state::effective_instream();
        // SAFETY: the application installed this callback for our FILE stream.
        let key = getc(stream);
        check_signals();
        return key;
    }

    let stream = state::effective_instream();
    let key = rl_getc(stream);
    check_signals();
    key
}

/// Read one byte from STREAM, handling signals and the armed timeout.
///
/// This is GNU's default `rl_getc_function`.  A read timeout calls
/// `rl_timeout_event_hook`, sets `RL_STATE_TIMEOUT`, and returns EOF after
/// clearing transient input (the adapter has no longjmp target; see the
/// timeout seam documented on `abort_after_timeout`).
#[no_mangle]
pub unsafe extern "C" fn rl_getc(stream: *mut libc::FILE) -> c_int {
    #[cfg(unix)]
    {
        if stream.is_null() {
            return libc::EOF;
        }
        let fd = libc::fileno(stream);
        if fd < 0 {
            return libc::fgetc(stream);
        }

        let mut byte: u8 = 0;
        loop {
            check_signals();

            let mut readfds: libc::fd_set = std::mem::zeroed();
            libc::FD_ZERO(&mut readfds);
            libc::FD_SET(fd, &mut readfds);
            let mut timeout = timeout_timeval();
            let timeout_ptr = timeout
                .as_mut()
                .map_or(ptr::null_mut(), |value| value as *mut libc::timeval);
            let ready = libc::select(
                fd + 1,
                &mut readfds,
                ptr::null_mut(),
                ptr::null_mut(),
                timeout_ptr,
            );

            if ready == 0 {
                if timeout_expired() {
                    abort_after_timeout();
                    return libc::EOF;
                }
                // Keyboard-timeout tick: re-check signals and wait again.
                continue;
            }

            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    check_signals();
                    if let Some(hook) = signal_event_hook() {
                        // SAFETY: application hook, called after SIGINT et al.
                        unsafe { hook() };
                    }
                    if timeout_expired() {
                        abort_after_timeout();
                        return libc::EOF;
                    }
                    continue;
                }
                return if state_is_set(RL_STATE_READCMD) {
                    READERR
                } else {
                    libc::EOF
                };
            }

            let count = libc::read(fd, &mut byte as *mut u8 as *mut libc::c_void, 1);
            if count == 1 {
                return c_int::from(byte);
            }
            if count == 0 {
                return libc::EOF;
            }

            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                check_signals();
                if let Some(hook) = signal_event_hook() {
                    // SAFETY: application hook after an interrupted read.
                    unsafe { hook() };
                }
                continue;
            }
            if matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK
            ) {
                continue;
            }
            return if state_is_set(RL_STATE_READCMD) {
                READERR
            } else {
                libc::EOF
            };
        }
    }

    #[cfg(not(unix))]
    {
        // Windows/other hosts read through the C stream; the console event
        // loop belongs to the host integration.
        if stream.is_null() {
            libc::EOF
        } else {
            libc::fgetc(stream)
        }
    }
}

// --- Hook indirection -----------------------------------------------------

fn prep_term_function() -> RlVintFunc {
    // SAFETY: the global is a plain function pointer written by the
    // application through the C ABI.
    unsafe { ptr::addr_of!(crate::completion::rl_prep_term_function).read() }
}

fn deprep_term_function() -> RlVoidFunc {
    // SAFETY: as above.
    unsafe { ptr::addr_of!(crate::completion::rl_deprep_term_function).read() }
}

/// Prepare the terminal for line editing, honouring an application-installed
/// `rl_prep_term_function`.  GNU calls the hook from the same places
/// (`readline()`, `_rl_callback_newline`, `rl_reset_after_signal`); when the
/// hook is NULL the library's own `rl_prep_terminal` is the default.
pub(crate) fn prep_for_input() {
    match prep_term_function() {
        // SAFETY: the application owns the callback and promised this ABI.
        Some(hook) => unsafe { hook(0) },
        // SAFETY: default preparation only touches terminal state.
        None => unsafe { rl_prep_terminal(0) },
    }
}

/// Restore the terminal after editing, honouring `rl_deprep_term_function`.
pub(crate) fn deprep_for_input() {
    match deprep_term_function() {
        // SAFETY: the application owns the callback and promised this ABI.
        Some(hook) => unsafe { hook() },
        // SAFETY: default restoration only touches terminal state.
        None => unsafe { rl_deprep_terminal() },
    }
}

// --- Terminal preparation -------------------------------------------------

#[cfg(unix)]
struct SavedTermios {
    fd: c_int,
    original: libc::termios,
}

#[cfg(unix)]
static SAVED_TERMIOS: Mutex<Option<SavedTermios>> = Mutex::new(None);

/// GNU uses `rl_instream`, which defaults to stdin.  This adapter reads stdin.
#[cfg(unix)]
const TERMINAL_FD: c_int = 0;

#[cfg(unix)]
const DISABLE_IFLAG: libc::tcflag_t = libc::IXON
    | libc::ICRNL
    | libc::INLCR
    // IXANY is not present on a few strict POSIX targets; the cfg below keeps
    // those builds honest instead of guessing a value.
    | IXANY_IFLAG;

#[cfg(all(
    unix,
    any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )
))]
const IXANY_IFLAG: libc::tcflag_t = libc::IXANY;

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))
))]
const IXANY_IFLAG: libc::tcflag_t = 0;

/// Terminal control characters saved by readline's terminal preparation.
#[cfg(unix)]
#[derive(Clone, Copy)]
struct TtyChars {
    erase: u8,
    kill: u8,
    lnext: u8,
    werase: u8,
}

#[cfg(unix)]
static TTY_CHARS: Mutex<Option<TtyChars>> = Mutex::new(None);

#[cfg(unix)]
static ECHOCTL_ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static INTR_CHAR: AtomicI32 = AtomicI32::new(0x03);
#[cfg(unix)]
static QUIT_CHAR: AtomicI32 = AtomicI32::new(0x1c);
#[cfg(unix)]
static SUSP_CHAR: AtomicI32 = AtomicI32::new(0x1a);

#[cfg(unix)]
fn save_tty_chars(tio: &libc::termios) {
    let chars = TtyChars {
        erase: tio.c_cc[libc::VERASE],
        kill: tio.c_cc[libc::VKILL],
        lnext: tio.c_cc[libc::VLNEXT],
        werase: tio.c_cc[libc::VWERASE],
    };
    *TTY_CHARS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(chars);
    INTR_CHAR.store(c_int::from(tio.c_cc[libc::VINTR]), Ordering::Relaxed);
    QUIT_CHAR.store(c_int::from(tio.c_cc[libc::VQUIT]), Ordering::Relaxed);
    SUSP_CHAR.store(c_int::from(tio.c_cc[libc::VSUSP]), Ordering::Relaxed);
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    {
        ECHOCTL_ENABLED.store(tio.c_lflag & libc::ECHOCTL != 0, Ordering::Relaxed);
    }
}

/// Put the terminal into readline's character-at-a-time mode.
///
/// Mirrors GNU `rltty.c:rl_prep_terminal`: `ICANON` and `ECHO` are cleared,
/// `ISIG` is deliberately left enabled, CR/NL translation and XON/XOFF are
/// disabled, and `VMIN`/`VTIME` request one byte at a time.  The original
/// settings are stored for `rl_deprep_terminal`.
///
/// A non-terminal input (a pipe, a test harness, a non-interactive shell)
/// makes `tcgetattr` fail.  GNU treats that as "echoing is on" and does not
/// set `RL_STATE_TERMPREPPED`; we do the same.
#[no_mangle]
pub unsafe extern "C" fn rl_prep_terminal(_meta_flag: c_int) {
    #[cfg(unix)]
    {
        if state_is_set(RL_STATE_TERMPREPPED) {
            return;
        }

        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(TERMINAL_FD, &mut tio) != 0 {
            state::set_echoing(1);
            return;
        }

        let original = tio;
        save_tty_chars(&original);
        add_state(RL_STATE_TTYCSAVED);
        let mut requested = tio;

        requested.c_lflag &= !(libc::ICANON | libc::ECHO);
        requested.c_iflag &= !DISABLE_IFLAG;

        // Only drop 8-bit stripping when the terminal is actually 8-bit, as
        // GNU does (`(c_cflag & CSIZE) == CS8 || meta_flag`).
        if (requested.c_cflag & libc::CSIZE) == libc::CS8 || _meta_flag != 0 {
            requested.c_iflag &= !(libc::ISTRIP | libc::INPCK);
        }

        requested.c_cc[libc::VMIN] = 1;
        requested.c_cc[libc::VTIME] = 0;

        if libc::tcsetattr(TERMINAL_FD, libc::TCSADRAIN, &requested) != 0 {
            return;
        }

        let mut saved = SAVED_TERMIOS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *saved = Some(SavedTermios {
            fd: TERMINAL_FD,
            original,
        });
        drop(saved);

        state::set_echoing(if original.c_lflag & libc::ECHO != 0 {
            1
        } else {
            0
        });
        add_state(RL_STATE_TERMPREPPED);
    }

    #[cfg(not(unix))]
    {
        // No termios on this platform.  A ConPTY/console host owns the mode;
        // readline cannot change it from the standalone library.
        let _ = _meta_flag;
        state::set_echoing(1);
    }
}

/// Restore the settings saved by `rl_prep_terminal`.  Safe to call when the
/// terminal was never prepared, matching GNU's early return.
#[no_mangle]
pub unsafe extern "C" fn rl_deprep_terminal() {
    #[cfg(unix)]
    {
        if !state_is_set(RL_STATE_TERMPREPPED) {
            return;
        }

        let saved = SAVED_TERMIOS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(saved) = saved {
            let _ = libc::tcsetattr(saved.fd, libc::TCSADRAIN, &saved.original);
        }
        remove_state(RL_STATE_TERMPREPPED);
    }

    #[cfg(not(unix))]
    {
        // Nothing was changed by rl_prep_terminal, so nothing to restore.
        remove_state(RL_STATE_TERMPREPPED);
    }
}

// --- Screen size ----------------------------------------------------------

static SCREEN_ROWS: AtomicU32 = AtomicU32::new(0);
static SCREEN_COLUMNS: AtomicU32 = AtomicU32::new(0);
static SCREEN_OVERRIDE: AtomicBool = AtomicBool::new(false);

const DEFAULT_ROWS: u32 = 24;
const DEFAULT_COLUMNS: u32 = 80;

fn env_dimension(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value > 0)
}

#[cfg(unix)]
fn kernel_dimensions() -> Option<(u32, u32)> {
    for fd in [0, 1, 2] {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ writes a winsize through the supplied pointer.
        let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
        if result == 0 && size.ws_row > 0 && size.ws_col > 0 {
            return Some((u32::from(size.ws_row), u32::from(size.ws_col)));
        }
    }
    None
}

#[cfg(not(unix))]
fn kernel_dimensions() -> Option<(u32, u32)> {
    // Windows ConPTY exposes the console size through the console API, not a
    // winsize ioctl.  Without a console handle the shared fallback (`LINES`,
    // `COLUMNS`, then 80x24) is used; a host integration can call
    // rl_set_screen_size once it knows the real size.
    None
}

fn load_dimensions() -> (u32, u32) {
    let kernel = kernel_dimensions();
    let environment = (
        env_dimension("LINES").filter(|value| *value > 0),
        env_dimension("COLUMNS").filter(|value| *value > 0),
    );

    // GNU's `rl_prefer_env_winsize` makes LINES/COLUMNS win over the kernel.
    let (rows, columns) = if state::prefer_env_winsize() != 0 {
        (
            environment.0.or_else(|| kernel.map(|(rows, _)| rows)),
            environment.1.or_else(|| kernel.map(|(_, columns)| columns)),
        )
    } else {
        (
            kernel.map(|(rows, _)| rows).or(environment.0),
            kernel.map(|(_, columns)| columns).or(environment.1),
        )
    };

    (
        rows.filter(|value| *value > 0).unwrap_or(DEFAULT_ROWS),
        columns
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_COLUMNS),
    )
}

fn ensure_dimensions() {
    if SCREEN_ROWS.load(Ordering::Relaxed) != 0 && SCREEN_COLUMNS.load(Ordering::Relaxed) != 0 {
        return;
    }
    let (rows, columns) = load_dimensions();
    SCREEN_ROWS.store(rows, Ordering::Relaxed);
    SCREEN_COLUMNS.store(columns, Ordering::Relaxed);
}

fn update_environment_dimensions() {
    if !change_environment() {
        return;
    }
    let rows = SCREEN_ROWS.load(Ordering::Relaxed);
    let columns = SCREEN_COLUMNS.load(Ordering::Relaxed);
    if rows > 0 {
        std::env::set_var("LINES", rows.to_string());
    }
    if columns > 0 {
        std::env::set_var("COLUMNS", columns.to_string());
    }
}

/// Re-read the terminal size after `SIGWINCH`, unless the application pinned
/// an explicit size with `rl_set_screen_size`.
pub(crate) fn refresh_screen_size() {
    if SCREEN_OVERRIDE.load(Ordering::Relaxed) {
        return;
    }
    if let Some((rows, columns)) = kernel_dimensions() {
        SCREEN_ROWS.store(rows, Ordering::Relaxed);
        SCREEN_COLUMNS.store(columns, Ordering::Relaxed);
    } else if SCREEN_ROWS.load(Ordering::Relaxed) == 0 {
        let (rows, columns) = load_dimensions();
        SCREEN_ROWS.store(rows, Ordering::Relaxed);
        SCREEN_COLUMNS.store(columns, Ordering::Relaxed);
    }
    update_environment_dimensions();
}

/// Return the cached screen dimensions.  The cache is initialised from the
/// kernel (or the environment) on first use and refreshed on resize.
#[no_mangle]
pub unsafe extern "C" fn rl_get_screen_size(rows: *mut c_int, columns: *mut c_int) {
    ensure_dimensions();
    if !rows.is_null() {
        *rows = SCREEN_ROWS.load(Ordering::Relaxed) as c_int;
    }
    if !columns.is_null() {
        *columns = SCREEN_COLUMNS.load(Ordering::Relaxed) as c_int;
    }
}

/// Cached terminal width for screen-line commands.
pub(crate) fn screen_width() -> c_int {
    ensure_dimensions();
    let columns = SCREEN_COLUMNS.load(Ordering::Relaxed) as c_int;
    if columns > 0 {
        columns
    } else {
        DEFAULT_COLUMNS as c_int
    }
}

/// Pin the cached screen dimensions.  Non-positive values leave the
/// corresponding dimension unchanged, matching GNU's `_rl_set_screen_size`.
#[no_mangle]
pub unsafe extern "C" fn rl_set_screen_size(rows: c_int, columns: c_int) {
    if rows > 0 {
        SCREEN_ROWS.store(rows as u32, Ordering::Relaxed);
    }
    if columns > 0 {
        SCREEN_COLUMNS.store(columns as u32, Ordering::Relaxed);
    }
    if rows > 0 || columns > 0 {
        SCREEN_OVERRIDE.store(true, Ordering::Relaxed);
        update_environment_dimensions();
    }
}

/// Drop a pinned size and re-read the terminal (and environment) dimensions.
#[no_mangle]
pub unsafe extern "C" fn rl_reset_screen_size() {
    SCREEN_OVERRIDE.store(false, Ordering::Relaxed);
    let (rows, columns) = load_dimensions();
    SCREEN_ROWS.store(rows, Ordering::Relaxed);
    SCREEN_COLUMNS.store(columns, Ordering::Relaxed);
    update_environment_dimensions();
}

// --- Pending input --------------------------------------------------------

/// Discard a key queued through `rl_pending_input`.  GNU clears the value and
/// the `RL_STATE_INPUTPENDING` bookkeeping bit.
#[no_mangle]
pub unsafe extern "C" fn rl_clear_pending_input() -> c_int {
    state::set_pending_input(0);
    remove_state(RL_STATE_INPUTPENDING);
    0
}

// --- Terminal defaults and capabilities -----------------------------------

fn bind_tty_special(map: crate::keymap::Keymap, character: u8, name: &str) {
    if map.is_null() || character == 0 {
        return;
    }
    let Ok(name) = CString::new(name) else {
        return;
    };
    // SAFETY: the name is NUL-terminated and the lookup only reads the
    // keymap/funmap store.
    let function = unsafe { crate::keymap::rl_named_function(name.as_ptr()) };
    let Some(function) = function else {
        return;
    };
    let entry = unsafe { &mut *map.add(character as usize) };
    if entry.r#type == crate::keymap::ISFUNC {
        // SAFETY: the function pointer is a bindable readline command.
        entry.function = function as *const () as *mut std::os::raw::c_void;
    }
}

/// Bind the terminal's erase/kill/word-erase/literal-next characters to their
/// readline commands in MAP, matching GNU `_rl_bind_tty_special_chars`.
#[no_mangle]
pub unsafe extern "C" fn rl_tty_set_default_bindings(map: crate::keymap::Keymap) {
    #[cfg(unix)]
    {
        let stream = state::effective_instream();
        let fd = if stream.is_null() {
            TERMINAL_FD
        } else {
            libc::fileno(stream)
        };
        let mut tio: libc::termios = std::mem::zeroed();
        if fd < 0 || libc::tcgetattr(fd, &mut tio) != 0 {
            return;
        }
        bind_tty_special(map, tio.c_cc[libc::VERASE], "backward-delete-char");
        bind_tty_special(map, tio.c_cc[libc::VKILL], "unix-line-discard");
        bind_tty_special(map, tio.c_cc[libc::VLNEXT], "quoted-insert");
        if state::editing_mode() == 0 {
            bind_tty_special(map, tio.c_cc[libc::VWERASE], "vi-unix-word-rubout");
        } else {
            bind_tty_special(map, tio.c_cc[libc::VWERASE], "unix-word-rubout");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = map;
    }
}

/// Restore the terminal special-character bindings saved by terminal
/// preparation to `self-insert`; a no-op before `rl_prep_terminal` has saved
/// them (GNU checks `RL_STATE_TTYCSAVED`).
#[no_mangle]
pub unsafe extern "C" fn rl_tty_unset_default_bindings(map: crate::keymap::Keymap) {
    #[cfg(unix)]
    {
        if !state_is_set(RL_STATE_TTYCSAVED) || map.is_null() {
            return;
        }
        let chars = *TTY_CHARS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(chars) = chars else {
            return;
        };
        for character in [chars.erase, chars.kill, chars.lnext, chars.werase] {
            bind_tty_special(map, character, "self-insert");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = map;
    }
}

/// Set readline's idea of whether it is echoing output; returns the old value.
#[no_mangle]
pub unsafe extern "C" fn rl_tty_set_echoing(echoing: c_int) -> c_int {
    let previous = state::echoing();
    state::set_echoing(echoing);
    previous
}

/// Echo the control character corresponding to SIG to the output stream when
/// the terminal has ECHOCTL and `echo-control-characters` is enabled.
#[no_mangle]
pub unsafe extern "C" fn rl_echo_signal_char(signal: c_int) {
    #[cfg(unix)]
    {
        if !ECHOCTL_ENABLED.load(Ordering::Relaxed) || !crate::keymap::echo_control_characters() {
            return;
        }

        let character = match signal {
            libc::SIGINT => INTR_CHAR.load(Ordering::Relaxed),
            libc::SIGQUIT => QUIT_CHAR.load(Ordering::Relaxed),
            libc::SIGTSTP => SUSP_CHAR.load(Ordering::Relaxed),
            _ => return,
        };

        let bytes = if (0..0x20).contains(&character) || character == 0x7f {
            let mut bytes = [b'^', 0u8];
            bytes[1] = if character == 0x7f {
                b'?'
            } else {
                ((character as u8) ^ 0x40).to_ascii_uppercase()
            };
            bytes.to_vec()
        } else if (0..=0xff).contains(&character) {
            vec![character as u8]
        } else {
            return;
        };

        let stream = state::effective_outstream();
        if !stream.is_null() {
            // SAFETY: one fwrite to the application output stream.
            libc::fwrite(
                bytes.as_ptr() as *const libc::c_void,
                1,
                bytes.len(),
                stream,
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal;
    }
}

// Built-in ANSI capabilities for the terminal families this adapter can
// identify.  GNU reads these from the termcap/terminfo database; the ABI and
// formatting are identical, but the value table is the documented seam for
// terminals whose private capabilities are not covered here.
static TCAP_KEY_UP: &[u8] = b"\x1b[A\0";
static TCAP_KEY_DOWN: &[u8] = b"\x1b[B\0";
static TCAP_KEY_RIGHT: &[u8] = b"\x1b[C\0";
static TCAP_KEY_LEFT: &[u8] = b"\x1b[D\0";
static TCAP_KEY_HOME: &[u8] = b"\x1b[H\0";
static TCAP_KEY_END: &[u8] = b"\x1b[4~\0";
static TCAP_KEY_DELETE: &[u8] = b"\x1b[3~\0";
static TCAP_KEY_INSERT: &[u8] = b"\x1b[2~\0";
static TCAP_KEY_PAGE_UP: &[u8] = b"\x1b[5~\0";
static TCAP_KEY_PAGE_DOWN: &[u8] = b"\x1b[6~\0";
static TCAP_CLEAR_EOL: &[u8] = b"\x1b[K\0";
static TCAP_CLEAR_PAGE: &[u8] = b"\x1b[H\x1b[2J\0";
static TCAP_CARRIAGE_RETURN: &[u8] = b"\r\0";
static TCAP_DELETE_CHAR: &[u8] = b"\x1b[P\0";
static TCAP_INSERT_CHAR: &[u8] = b"\x1b[@\0";
static TCAP_HOME: &[u8] = b"\x1b[H\0";
static TCAP_BACKSPACE: &[u8] = b"\x08\0";
static TCAP_FORWARD_CHAR: &[u8] = b"\x1b[C\0";
static TCAP_STANDOUT: &[u8] = b"\x1b[0;7m\0";
static TCAP_STANDOUT_END: &[u8] = b"\x1b[0m\0";
static TCAP_VISIBLE_BELL: &[u8] = b"\x1b[?5h\x1b[?5l\0";
static TCAP_CURSOR_NORMAL: &[u8] = b"\x1b[?12l\x1b[?25h\0";
static TCAP_CURSOR_INVISIBLE: &[u8] = b"\x1b[?25l\0";
static TCAP_KEYPAD_START: &[u8] = b"\x1b[?1h\x1b=\0";
static TCAP_KEYPAD_END: &[u8] = b"\x1b[?1l\x1b>\0";

fn ansi_capabilities() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("@7", TCAP_KEY_END),
        ("ce", TCAP_CLEAR_EOL),
        ("cl", TCAP_CLEAR_PAGE),
        ("cr", TCAP_CARRIAGE_RETURN),
        ("dc", TCAP_DELETE_CHAR),
        ("ho", TCAP_HOME),
        ("ic", TCAP_INSERT_CHAR),
        ("kD", TCAP_KEY_DELETE),
        ("kI", TCAP_KEY_INSERT),
        ("kN", TCAP_KEY_PAGE_DOWN),
        ("kP", TCAP_KEY_PAGE_UP),
        ("kd", TCAP_KEY_DOWN),
        ("ke", TCAP_KEYPAD_END),
        ("kh", TCAP_KEY_HOME),
        ("kl", TCAP_KEY_LEFT),
        ("kr", TCAP_KEY_RIGHT),
        ("ks", TCAP_KEYPAD_START),
        ("ku", TCAP_KEY_UP),
        ("le", TCAP_BACKSPACE),
        ("nd", TCAP_FORWARD_CHAR),
        ("se", TCAP_STANDOUT_END),
        ("so", TCAP_STANDOUT),
        ("up", TCAP_KEY_UP),
        ("vb", TCAP_VISIBLE_BELL),
        ("ve", TCAP_CURSOR_NORMAL),
        ("vs", TCAP_CURSOR_INVISIBLE),
    ]
}

fn ansi_terminal(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "xterm",
        "screen",
        "tmux",
        "rxvt",
        "ansi",
        "linux",
        "vt100",
        "vt220",
        "cygwin",
        "alacritty",
        "kitty",
        "wezterm",
        "foot",
        "contour",
        "st",
        "eterm",
    ];
    let name = name.to_ascii_lowercase();
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

type TermcapEntry = (&'static str, &'static [u8]);

static TERMCAP: Mutex<Option<Vec<TermcapEntry>>> = Mutex::new(None);

fn initialize_termcap(name: Option<&str>) {
    let term = name
        .map(str::to_owned)
        .or_else(|| std::env::var("TERM").ok())
        .unwrap_or_else(|| "dumb".to_owned());
    let capabilities = if ansi_terminal(&term) {
        ansi_capabilities()
    } else {
        Vec::new()
    };
    *TERMCAP
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(capabilities);
}

/// Look up a terminal capability by its termcap name.  Returns NULL for an
/// unknown capability or a terminal without a recognized ANSI description.
#[no_mangle]
pub unsafe extern "C" fn rl_get_termcap(capability: *const c_char) -> *mut c_char {
    if capability.is_null() || !state_is_set(crate::line::RL_STATE_INITIALIZED) {
        return ptr::null_mut();
    }
    let name = std::ffi::CStr::from_ptr(capability)
        .to_string_lossy()
        .into_owned();
    let borrowed = name.as_str();
    let mut table = TERMCAP
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if table.is_none() {
        drop(table);
        initialize_termcap(None);
        table = TERMCAP
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    table
        .as_ref()
        .and_then(|entries| {
            entries
                .iter()
                .find(|(candidate, _)| *candidate == borrowed)
                .map(|(_, value)| *value)
        })
        .map(|value| value.as_ptr() as *mut c_char)
        .unwrap_or(ptr::null_mut())
}

/// Re-read the terminal description and reset the cached screen size.
#[no_mangle]
pub unsafe extern "C" fn rl_reset_terminal(terminal: *const c_char) -> c_int {
    SCREEN_ROWS.store(0, Ordering::Relaxed);
    SCREEN_COLUMNS.store(0, Ordering::Relaxed);
    let name = if terminal.is_null() {
        None
    } else {
        Some(
            std::ffi::CStr::from_ptr(terminal)
                .to_string_lossy()
                .into_owned(),
        )
    };
    initialize_termcap(name.as_deref());
    0
}

static LAST_LS_COLORS: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// Re-read `LS_COLORS`.  This adapter does not maintain GNU's colored-stats
/// extension table; the hook records the current value so existing state is
/// unchanged and a future color implementation has a single entry point.
#[no_mangle]
pub unsafe extern "C" fn rl_reparse_colors() {
    let value = std::env::var_os("LS_COLORS")
        .map(|value| value.to_string_lossy().into_owned().into_bytes());
    let mut last = LAST_LS_COLORS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *last == value {
        return;
    }
    *last = value;
}

static PAREN_BLINK_USEC: AtomicI32 = AtomicI32::new(500_000);

/// Set the parenthesis-blink delay in microseconds; returns the old value.
/// Non-positive values leave the current delay unchanged.
#[no_mangle]
pub unsafe extern "C" fn rl_set_paren_blink_timeout(timeout: c_int) -> c_int {
    let previous = PAREN_BLINK_USEC.load(Ordering::Relaxed);
    if timeout > 0 {
        PAREN_BLINK_USEC.store(timeout, Ordering::Relaxed);
    }
    previous
}

// --- Signals (Unix) -------------------------------------------------------

#[cfg(unix)]
static OLD_SIGINT: Mutex<Option<libc::sigaction>> = Mutex::new(None);
#[cfg(unix)]
static OLD_SIGWINCH: Mutex<Option<libc::sigaction>> = Mutex::new(None);
#[cfg(unix)]
static SIGINT_SET: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static SIGWINCH_SET: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static SIGINT_WAS_CUSTOM: AtomicBool = AtomicBool::new(false);

/// Set by the async-signal-safe handler, consumed by `check_signals`.
#[cfg(unix)]
static CAUGHT_SIGINT: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static CAUGHT_SIGWINCH: AtomicBool = AtomicBool::new(false);
/// Last signal number recorded by a handler (GNU `_rl_caught_signal`).
#[cfg(unix)]
static CAUGHT_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Async-signal-safe `SIGINT` handler: only record the delivery.  GNU likewise
/// defers all real work to `_rl_signal_handler`/`RL_CHECK_SIGNALS`.
#[cfg(unix)]
extern "C" fn rl_signal_handler(_signal: c_int) {
    CAUGHT_SIGINT.store(true, Ordering::SeqCst);
    CAUGHT_SIGNAL.store(_signal, Ordering::SeqCst);
}

/// Async-signal-safe `SIGWINCH` handler.
#[cfg(unix)]
extern "C" fn rl_sigwinch_handler(_signal: c_int) {
    CAUGHT_SIGWINCH.store(true, Ordering::SeqCst);
    CAUGHT_SIGNAL.store(_signal, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_handler(signal: c_int, handler: extern "C" fn(c_int)) -> Option<libc::sigaction> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler as libc::sighandler_t;
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: the mask is owned by this stack value.
    unsafe { libc::sigemptyset(&mut action.sa_mask) };

    let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers refer to valid sigaction values for the call.
    let result = unsafe { libc::sigaction(signal, &action, &mut previous) };
    if result == 0 {
        Some(previous)
    } else {
        None
    }
}

#[cfg(unix)]
fn current_disposition(signal: c_int) -> Option<libc::sigaction> {
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: a NULL new action queries without changing the disposition.
    let result = unsafe { libc::sigaction(signal, ptr::null(), &mut current) };
    if result == 0 {
        Some(current)
    } else {
        None
    }
}

/// Install readline's `SIGINT` and `SIGWINCH` handlers, saving the previous
/// dispositions.  Returns 0 like GNU.  Idempotent: a second call while the
/// handlers are installed does not overwrite the saved dispositions.
#[no_mangle]
pub unsafe extern "C" fn rl_set_signals() -> c_int {
    #[cfg(unix)]
    {
        if catch_signals() != 0 && !SIGINT_SET.swap(true, Ordering::SeqCst) {
            let ignored = current_disposition(libc::SIGINT)
                .map(|current| current.sa_sigaction == libc::SIG_IGN)
                .unwrap_or(false);

            let mut saved = OLD_SIGINT
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if ignored {
                // GNU does not replace an ignored signal.
                *saved = current_disposition(libc::SIGINT);
                SIGINT_WAS_CUSTOM.store(false, Ordering::SeqCst);
            } else if let Some(previous) = install_handler(libc::SIGINT, rl_signal_handler) {
                SIGINT_WAS_CUSTOM.store(
                    previous.sa_sigaction != libc::SIG_DFL
                        && previous.sa_sigaction != libc::SIG_IGN,
                    Ordering::SeqCst,
                );
                *saved = Some(previous);
            } else {
                SIGINT_SET.store(false, Ordering::SeqCst);
            }
        }

        if catch_sigwinch() != 0 && !SIGWINCH_SET.swap(true, Ordering::SeqCst) {
            let ignored = current_disposition(libc::SIGWINCH)
                .map(|current| current.sa_sigaction == libc::SIG_IGN)
                .unwrap_or(false);

            let mut saved = OLD_SIGWINCH
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if ignored {
                *saved = current_disposition(libc::SIGWINCH);
            } else if let Some(previous) = install_handler(libc::SIGWINCH, rl_sigwinch_handler) {
                *saved = Some(previous);
            } else {
                SIGWINCH_SET.store(false, Ordering::SeqCst);
            }
        }
    }

    // Non-Unix: there are no POSIX dispositions to install.  The host event
    // loop owns Ctrl-C/Win32 console events.
    0
}

/// Restore the `SIGINT`/`SIGWINCH` dispositions saved by `rl_set_signals`.
#[no_mangle]
pub unsafe extern "C" fn rl_clear_signals() -> c_int {
    #[cfg(unix)]
    {
        if catch_signals() != 0 && SIGINT_SET.swap(false, Ordering::SeqCst) {
            let saved = OLD_SIGINT
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(previous) = saved {
                let _ = libc::sigaction(libc::SIGINT, &previous, ptr::null_mut());
            }
            SIGINT_WAS_CUSTOM.store(false, Ordering::SeqCst);
        }

        if catch_sigwinch() != 0 && SIGWINCH_SET.swap(false, Ordering::SeqCst) {
            let saved = OLD_SIGWINCH
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(previous) = saved {
                let _ = libc::sigaction(libc::SIGWINCH, &previous, ptr::null_mut());
            }
        }
    }

    0
}

/// GNU `rl_cleanup_after_signal`: leave terminal mode, drop pending input, and
/// restore the application's signal dispositions.  The application calls this
/// from its own signal handling before it returns to its event loop.
#[no_mangle]
pub unsafe extern "C" fn rl_cleanup_after_signal() {
    deprep_for_input();
    rl_clear_pending_input();
    rl_clear_signals();
}

/// GNU `rl_reset_after_signal`: re-enter terminal mode and reinstall the
/// library's handlers after the application handled a signal.
#[no_mangle]
pub unsafe extern "C" fn rl_reset_after_signal() {
    prep_for_input();
    rl_set_signals();
}

/// Consume signals recorded by the async handlers.
///
/// `SIGWINCH` refreshes the cached size and repaints.  `SIGINT` performs GNU's
/// cleanup order: free line state, run callback signal cleanup, restore the
/// terminal, note the interrupt for the callback loop, deliver the signal to
/// the application's previous handler, then re-enter terminal mode.
///
/// Divergence (documented): when no application handler was installed
/// (the previous disposition was `SIG_DFL`) GNU re-raises the signal and the
/// process terminates.  A standalone ABI adapter used inside an event-loop
/// host must not take the host down for a readline-internal Ctrl-C, so this
/// path aborts the current line instead.  With an application handler the
/// signal is re-raised exactly like GNU, after the terminal is restored.
pub(crate) fn check_signals() {
    #[cfg(unix)]
    {
        if CAUGHT_SIGWINCH.swap(false, Ordering::SeqCst) {
            add_state(RL_STATE_SIGHANDLER);
            refresh_screen_size();
            // SAFETY: GNU's SIGWINCH path repaints through rl_resize_terminal;
            // it tolerates an uninitialised library.
            unsafe { crate::completion::rl_resize_terminal() };
            remove_state(RL_STATE_SIGHANDLER);
        }

        if CAUGHT_SIGINT.swap(false, Ordering::SeqCst) {
            add_state(RL_STATE_SIGHANDLER);

            // Capture the disposition before rl_cleanup_after_signal restores
            // the application's handler and resets the bookkeeping flag.
            let was_custom = SIGINT_WAS_CUSTOM.load(Ordering::SeqCst);

            // SAFETY: these are the documented GNU cleanup entry points.
            unsafe {
                crate::line::rl_free_line_state();
                crate::callback::rl_callback_sigcleanup();
                rl_cleanup_after_signal();
            }
            crate::callback::note_interrupt();
            if was_custom {
                // SAFETY: the previous handler is installed again after the
                // cleanup above, so raise() reaches the application's handler.
                unsafe { libc::raise(libc::SIGINT) };
            }
            // SAFETY: re-enter terminal mode for the remainder of the line.
            unsafe { rl_reset_after_signal() };

            remove_state(RL_STATE_SIGHANDLER);
        }

        CAUGHT_SIGNAL.store(0, Ordering::SeqCst);
    }
}

/// Return the signal recorded by the last handler, or 0 when none is pending.
/// GNU's `_rl_caught_signal` is cleared once `rl_check_signals` handles it.
#[no_mangle]
pub unsafe extern "C" fn rl_pending_signal() -> c_int {
    #[cfg(unix)]
    {
        CAUGHT_SIGNAL.load(Ordering::SeqCst)
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Public entry point for the signal post-processing that the input loop runs
/// after an interrupted read (GNU's `RL_CHECK_SIGNALS`).
#[no_mangle]
pub unsafe extern "C" fn rl_check_signals() {
    check_signals();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_size_values_are_positive() {
        // Pure cache defaults; does not depend on the test terminal.
        let (rows, columns) = load_dimensions();
        assert!(rows > 0);
        assert!(columns > 0);
    }

    #[test]
    fn pending_input_clear_drops_the_state_bit() {
        // SAFETY: single test process global; no concurrent readers here.
        unsafe { rl_clear_pending_input() };
        state::set_pending_input('x' as c_int);
        add_state(RL_STATE_INPUTPENDING);
        // SAFETY: as above.
        unsafe { rl_clear_pending_input() };
        assert_eq!(state::pending_input(), 0);
        assert!(!state_is_set(RL_STATE_INPUTPENDING));
    }
}
