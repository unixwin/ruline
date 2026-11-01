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

use std::os::raw::{c_int, c_ulong};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
#[cfg(unix)]
use std::sync::Mutex;

use crate::completion::{RlVintFunc, RlVoidFunc};
use crate::state;

// --- RL_STATE bits --------------------------------------------------------

// Values are fixed by the GNU ABI (`readline.h`).  They live here because
// this module owns the phases that are only visible during terminal/signal
// processing; `line.rs` keeps the lifecycle bits it already exported.
pub(crate) const RL_STATE_TERMPREPPED: c_ulong = 0x0000_0004;
pub(crate) const RL_STATE_READCMD: c_ulong = 0x0000_0008;
pub(crate) const RL_STATE_DISPATCHING: c_ulong = 0x0000_0020;
pub(crate) const RL_STATE_SIGHANDLER: c_ulong = 0x0000_8000;
pub(crate) const RL_STATE_INPUTPENDING: c_ulong = 0x0002_0000;
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
    let rows = kernel
        .map(|(rows, _)| rows)
        .or_else(|| env_dimension("LINES"))
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ROWS);
    let columns = kernel
        .map(|(_, columns)| columns)
        .or_else(|| env_dimension("COLUMNS"))
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_COLUMNS);
    (rows, columns)
}

fn ensure_dimensions() {
    if SCREEN_ROWS.load(Ordering::Relaxed) != 0 && SCREEN_COLUMNS.load(Ordering::Relaxed) != 0 {
        return;
    }
    let (rows, columns) = load_dimensions();
    SCREEN_ROWS.store(rows, Ordering::Relaxed);
    SCREEN_COLUMNS.store(columns, Ordering::Relaxed);
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
    }
}

/// Drop a pinned size and re-read the terminal (and environment) dimensions.
#[no_mangle]
pub unsafe extern "C" fn rl_reset_screen_size() {
    SCREEN_OVERRIDE.store(false, Ordering::Relaxed);
    let (rows, columns) = load_dimensions();
    SCREEN_ROWS.store(rows, Ordering::Relaxed);
    SCREEN_COLUMNS.store(columns, Ordering::Relaxed);
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

/// Async-signal-safe `SIGINT` handler: only record the delivery.  GNU likewise
/// defers all real work to `_rl_signal_handler`/`RL_CHECK_SIGNALS`.
#[cfg(unix)]
extern "C" fn rl_signal_handler(_signal: c_int) {
    CAUGHT_SIGINT.store(true, Ordering::SeqCst);
}

/// Async-signal-safe `SIGWINCH` handler.
#[cfg(unix)]
extern "C" fn rl_sigwinch_handler(_signal: c_int) {
    CAUGHT_SIGWINCH.store(true, Ordering::SeqCst);
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
        if !SIGINT_SET.swap(true, Ordering::SeqCst) {
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

        if !SIGWINCH_SET.swap(true, Ordering::SeqCst) {
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
        if SIGINT_SET.swap(false, Ordering::SeqCst) {
            let saved = OLD_SIGINT
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(previous) = saved {
                let _ = libc::sigaction(libc::SIGINT, &previous, ptr::null_mut());
            }
            SIGINT_WAS_CUSTOM.store(false, Ordering::SeqCst);
        }

        if SIGWINCH_SET.swap(false, Ordering::SeqCst) {
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
    }
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
