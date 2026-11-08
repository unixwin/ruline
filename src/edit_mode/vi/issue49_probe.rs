//! Issue 49 — the #43 insert-session capture must fold the session's
//! word rubout, and must fold at all when the session ends by submitting.
//!
//! GNU does not log keystrokes: when a vi insert session ends,
//! `_rl_vi_done_inserting` (vi_mode.c:854-893) re-derives the recorded
//! insert from the text the session actually left in the buffer, so a
//! `Ctrl-W` rubout inside the session is already folded — the replay
//! inserts the *net* text. It runs on both ways out of the session:
//! `Esc` through `rl_vi_movement_mode` (vi_mode.c:905) and `Enter`
//! through `rl_newline` (text.c:1158). ruline's capture is a keystroke
//! log, so each rubout shape needs its fold here: `Backspace` pops one
//! char (the #43 fold), `BackspaceWord` (Ctrl-W) truncates at the same
//! word boundary the editor rubs out to, and `CutFromStart` (Ctrl-U)
//! clears it. A session ended by `Enter` folds exactly like one ended
//! by `Esc`.
#![cfg(test)]

use super::{Vi, ViMode};
use crate::core_editor::Editor;
use crate::prompt::PromptViMode;
use crate::{EditMode, PromptEditMode, ReedlineEvent, ReedlineRawEvent, UndoBehavior};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

struct Driver {
    vi: Vi,
    editor: Editor,
}

fn raw(code: KeyCode, mods: KeyModifiers) -> ReedlineRawEvent {
    ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, mods))).unwrap()
}

impl Driver {
    fn new(buffer: &str, pos: usize, mode: ViMode) -> Self {
        let mut editor = Editor::default();
        editor.set_buffer(buffer.to_string(), UndoBehavior::CreateUndoPoint);
        editor.line_buffer_mut().set_insertion_point(pos);
        let mut d = Driver {
            vi: Vi {
                mode,
                ..Default::default()
            },
            editor,
        };
        d.sync_mode();
        d
    }

    fn sync_mode(&mut self) {
        let m = match self.vi.mode {
            ViMode::Normal => PromptViMode::Normal,
            ViMode::Insert => PromptViMode::Insert,
            ViMode::Visual => PromptViMode::Visual,
        };
        self.editor.set_edit_mode(PromptEditMode::Vi(m));
    }

    fn apply(&mut self, ev: ReedlineEvent) {
        match ev {
            ReedlineEvent::Edit(cmds) => {
                for c in &cmds {
                    self.editor.run_edit_command(c);
                }
            }
            ReedlineEvent::Multiple(evs) => {
                for e in evs {
                    self.apply(e);
                }
            }
            ReedlineEvent::OverwriteMode(on) => {
                let next = on.unwrap_or(!self.editor.overwrite());
                self.editor.set_overwrite(next);
            }
            _ => {}
        }
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ev = self.vi.parse_event(raw(code, mods));
        self.apply(ev);
        self.sync_mode();
    }

    fn ctrl(&mut self, c: char) {
        self.key(KeyCode::Char(c), KeyModifiers::CONTROL);
    }

    fn esc(&mut self) {
        self.key(KeyCode::Esc, KeyModifiers::NONE);
    }

    fn keys(&mut self, s: &str) {
        for c in s.chars() {
            if c.is_ascii_uppercase() {
                self.key(KeyCode::Char(c), KeyModifiers::SHIFT);
            } else {
                self.key(KeyCode::Char(c), KeyModifiers::NONE);
            }
        }
    }

    /// The engine accepted the line (the `Enter` above): the next
    /// `read_line` starts on a fresh buffer while the mode machine —
    /// and its recorded `.` action — carries over.
    fn new_line(&mut self, buffer: &str, pos: usize) {
        self.editor
            .set_buffer(buffer.to_string(), UndoBehavior::CreateUndoPoint);
        self.editor.line_buffer_mut().set_insertion_point(pos);
        self.sync_mode();
    }

    fn state(&self) -> (String, usize) {
        (
            self.editor.get_buffer().to_string(),
            self.editor.insertion_point(),
        )
    }

    fn mode(&self) -> ViMode {
        self.vi.mode
    }
}

#[test]
fn issue49_dot_repeat_folds_a_ctrl_w_rubout_into_the_captured_text() {
    let mut d = Driver::new("one two three", 0, ViMode::Normal);
    d.keys("cw");
    d.keys("alpha beta");
    assert_eq!(d.state().0, "alpha beta two three");
    // Ctrl-W rubs the last word out of the buffer...
    d.ctrl('w');
    assert_eq!(d.state().0, "alpha  two three");
    // ...and a second Ctrl-W rubs through the gap into the first word.
    d.ctrl('w');
    assert_eq!(d.state().0, " two three");
    d.keys("Z");
    assert_eq!(d.state().0, "Z two three");
    d.esc();
    // The replay must insert the net text — just "Z" — not the log of
    // everything typed ("alpha betaZ").
    d.keys("w");
    d.keys(".");
    assert_eq!(d.state().0, "Z Z three");
}

#[test]
fn issue49_dot_repeat_folds_a_ctrl_u_discard_into_the_captured_text() {
    // Ctrl-U (unix-line-discard) kills everything the session typed:
    // the capture folds to empty, like the buffer does.
    let mut d = Driver::new("one two three", 0, ViMode::Normal);
    d.keys("cw");
    d.keys("junk");
    assert_eq!(d.state().0, "junk two three");
    d.ctrl('u');
    assert_eq!(d.state().0, " two three");
    d.keys("Z");
    d.esc();
    d.keys("w");
    d.keys(".");
    assert_eq!(d.state().0, "Z Z three");
}

#[test]
fn issue49_a_session_ended_by_enter_still_folds_into_the_dot_action() {
    let mut d = Driver::new("one two three", 0, ViMode::Normal);
    d.keys("cw");
    d.keys("X");
    assert_eq!(d.state().0, "X two three");
    // Enter submits the line instead of Esc ending the session; GNU
    // saves the insert on this path too (text.c:1158).
    d.key(KeyCode::Enter, KeyModifiers::NONE);
    d.new_line("one two three", 0);
    // The next line starts in insert, as a fresh vi line does.
    assert_eq!(d.mode(), ViMode::Insert);
    d.esc();
    assert_eq!(d.mode(), ViMode::Normal);
    // `.` replays the change *and* its text, even though the session
    // was submitted rather than escaped.
    d.keys(".");
    assert_eq!(d.state().0, "X two three");
}
