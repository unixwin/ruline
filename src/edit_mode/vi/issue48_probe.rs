//! Issue 48 — `cj`/`ck` at the buffer edge must not enter insert.
//!
//! GNU: a line motion that cannot leave the current line fails
//! (`rl_vi_next_line`/`rl_vi_prev_line` return MOVE_FAILED), `rl_vi_domove`
//! reports the failure (vi_mode.c:1385-1396), and the operator is cancelled
//! wholesale — `rl_vi_change_to` dings and never reaches
//! `vi_change_dispatch`, so `rl_vi_start_inserting` never runs. ruline's
//! #33 fix made the editor skip the dead operator edit; these pins cover
//! the parse-layer half: the mode must stay Normal, no insert session may
//! open, and the cancelled operator must not become the `.` action.
//!
//! The driver mirrors the engine's event round trip: an `Edit` event's
//! commands run through `Editor::run_edit_command`, their aggregate
//! status is computed the way `apply_edit_commands` computes it (applied
//! if any applied), and the outcome is reported back to the mode machine
//! through `EditMode::note_edit_outcome`, as `handle_editor_event` does.
#![cfg(test)]

use super::{Vi, ViMode};
use crate::core_editor::{EditCommandStatus, Editor};
use crate::enums::EventStatus;
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
                let mut status = EditCommandStatus::Inapplicable;
                for c in &cmds {
                    if self.editor.run_edit_command(c) == EditCommandStatus::Applied {
                        status = EditCommandStatus::Applied;
                    }
                }
                // The engine reports the executed batch's outcome back to
                // the mode machine (see `EditMode::note_edit_outcome`).
                self.vi.note_edit_outcome(
                    &cmds,
                    match status {
                        EditCommandStatus::Applied => EventStatus::Handled,
                        EditCommandStatus::Inapplicable => EventStatus::Inapplicable,
                    },
                );
            }
            ReedlineEvent::Multiple(evs) => {
                for e in evs {
                    self.apply(e);
                }
            }
            // See issue50_probe: the cursor keys arrive as
            // `UntilFound` combos; with no menu/hint state in the
            // probe, applying the sequence mirrors the engine.
            ReedlineEvent::UntilFound(evs) => {
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

    fn keys(&mut self, s: &str) {
        for c in s.chars() {
            if c.is_ascii_uppercase() {
                self.key(KeyCode::Char(c), KeyModifiers::SHIFT);
            } else {
                self.key(KeyCode::Char(c), KeyModifiers::NONE);
            }
        }
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
fn issue48_cj_on_the_last_line_is_cancelled_and_stays_in_normal() {
    let mut d = Driver::new("one\ntwo", 5, ViMode::Normal);
    d.keys("cj");
    // The motion failed, so the operator was cancelled: no insert
    // session opened and the buffer is untouched.
    assert_eq!(d.mode(), ViMode::Normal);
    assert_eq!(d.state().0, "one\ntwo");
    // Chain: the next key is read as a normal-mode command, not as
    // insert text — `x` deletes the grapheme under the caret.
    d.keys("x");
    assert_eq!(d.mode(), ViMode::Normal);
    assert_eq!(d.state().0, "one\nto");
}

#[test]
fn issue48_ck_on_the_first_line_is_cancelled_and_stays_in_normal() {
    let mut d = Driver::new("one\ntwo", 1, ViMode::Normal);
    d.keys("ck");
    assert_eq!(d.mode(), ViMode::Normal);
    assert_eq!(d.state().0, "one\ntwo");
    // Chain: `x` still deletes as a command.
    d.keys("x");
    assert_eq!(d.mode(), ViMode::Normal);
    assert_eq!(d.state().0, "oe\ntwo");
}

#[test]
fn issue48_a_cancelled_change_does_not_become_the_dot_action() {
    let mut d = Driver::new("one two\nthree four", 8, ViMode::Normal);
    d.keys("dw");
    assert_eq!(d.state().0, "one two\nfour");
    // `cj` on the last line fails and is cancelled...
    d.keys("cj");
    assert_eq!(d.mode(), ViMode::Normal);
    assert_eq!(d.state().0, "one two\nfour");
    // ...so `.` still repeats the `dw`, not the cancelled change.
    d.keys(".");
    assert_eq!(d.state().0, "one two\n");
    assert_eq!(d.mode(), ViMode::Normal);
}

#[test]
fn issue48_cj_with_a_live_line_below_still_enters_insert() {
    // Control: the cancellation is keyed on the motion failing, not on
    // the operator — with a line below, `cj` changes both lines and the
    // insert session opens on the blank line that remains.
    let mut d = Driver::new("one\ntwo", 0, ViMode::Normal);
    d.keys("cj");
    assert_eq!(d.mode(), ViMode::Insert);
    assert_eq!(d.state().0, "");
    d.keys("X");
    assert_eq!(d.state().0, "X");
    d.key(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(d.mode(), ViMode::Normal);
    assert_eq!(d.state().0, "X");
}

#[test]
fn issue48_a_cancelled_delete_does_not_become_the_dot_action() {
    // The cancellation is the operator layer's, not `c`'s alone: a
    // failed `dj` dispatches nothing in GNU, so it is not recorded
    // either — `.` keeps repeating the last real change.
    let mut d = Driver::new("one two\nthree four", 8, ViMode::Normal);
    d.keys("dw");
    assert_eq!(d.state().0, "one two\nfour");
    d.keys("dj");
    assert_eq!(d.state().0, "one two\nfour");
    d.keys(".");
    assert_eq!(d.state().0, "one two\n");
}
