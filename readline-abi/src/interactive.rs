//! Minimal `readline()` entry point driven by ruline's `Reedline` engine.

use std::borrow::Cow;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

use reedline::{Prompt, PromptEditMode, PromptHistorySearch, Reedline, Signal};

use crate::completion;
use crate::line;
use crate::state;
use crate::terminal;

struct AbiPrompt {
    text: String,
}

impl Prompt for AbiPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.text.as_str())
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}

fn prompt_text() -> String {
    // GNU skips prompt output when the application has already painted it;
    // Reedline paints the prompt it is given, so hand it an empty prompt.
    if crate::display::already_prompted() != 0 {
        return String::new();
    }

    let prompt = state::prompt();
    if prompt.is_null() {
        String::new()
    } else {
        // SAFETY: rl_prompt is NUL-terminated and owned by this library.
        unsafe { CStr::from_ptr(prompt) }
            .to_string_lossy()
            .into_owned()
    }
}

fn into_c_string(value: String) -> *mut c_char {
    match CString::new(value) {
        Ok(value) => unsafe { libc::strdup(value.as_ptr()) },
        Err(_) => ptr::null_mut(),
    }
}

fn mark_done() {
    state::set_done(1);
    let current = state::readline_state();
    state::set_readline_state(current | line::RL_STATE_DONE);
}

fn mark_eof() {
    state::set_done(1);
    state::set_eof_found(1);
    let current = state::readline_state();
    state::set_readline_state(current | line::RL_STATE_DONE | line::RL_STATE_EOF);
}

/// Read one line with ruline and return a malloc-compatible C string.
///
/// A NULL return means EOF, an interrupt, or a terminal error, matching the
/// readline contract.  The final buffer is mirrored into the exported state
/// globals before returning.
#[no_mangle]
pub unsafe extern "C" fn readline(prompt: *const c_char) -> *mut c_char {
    line::set_prompt(prompt);
    line::rl_initialize();

    // GNU calls the startup hook before the first prompt and the pre-input
    // hook once the prompt is on screen.  Both run before any input is read.
    completion::run_startup_hook();

    if state::pending_input() == libc::EOF {
        state::set_pending_input(0);
        mark_eof();
        return ptr::null_mut();
    }

    let mut editor = Reedline::create();
    let prompt = AbiPrompt {
        text: prompt_text(),
    };

    // GNU readline puts the terminal in editing mode and installs its signal
    // handlers around readline_internal(); both are restored before returning.
    terminal::prep_for_input();
    // SAFETY: signal dispositions are restored below.
    unsafe { terminal::rl_set_signals() };

    completion::run_pre_input_hook();

    let result = match editor.read_line(&prompt) {
        Ok(Signal::Success(buffer)) => {
            line::set_line_bytes(buffer.as_bytes());
            mark_done();
            into_c_string(buffer)
        }
        Ok(Signal::HostCommand(buffer)) | Ok(Signal::ExternalBreak(buffer)) => {
            line::set_line_bytes(buffer.as_bytes());
            mark_done();
            into_c_string(buffer)
        }
        Ok(Signal::CtrlC) => {
            line::set_line_bytes(b"");
            mark_done();
            ptr::null_mut()
        }
        Ok(Signal::CtrlD) => {
            line::set_line_bytes(b"");
            mark_eof();
            ptr::null_mut()
        }
        // `Signal` is non-exhaustive; treat an unknown future signal like an
        // aborted read rather than pretending a line was accepted.
        Ok(_) => {
            line::set_line_bytes(b"");
            mark_done();
            ptr::null_mut()
        }
        Err(_) => ptr::null_mut(),
    };

    terminal::deprep_for_input();
    // SAFETY: restores the application's signal dispositions.
    unsafe { terminal::rl_clear_signals() };

    result
}
