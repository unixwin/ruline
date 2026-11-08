//! Issue 50 — dot replay of an `R` session must replace, not insert.
//!
//! GNU keeps the two replays apart in `rl_vi_redo` (vi_mode.c:249): a
//! recorded plain insert is stuffed back with `rl_insert_text`
//! (`_rl_vi_stuff_insert`), but a recorded `R` session goes through
//! `_rl_vi_replace_insert` (vi_mode.c:222-235) — `_rl_replace_text`
//! swaps the recorded text over the span under the point, atomically,
//! in command mode, with no overstrike left on afterwards. ruline's
//! fold turned every session's text into an `InsertString`, so the
//! replay inserted plainly and left the editor's overwrite flag on.
//!
//! The driver mirrors the engine's routing of `OverwriteMode` to
//! `Editor::set_overwrite`, like the a6 probes.
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
            // The engine routes this to `Editor::set_overwrite`
            // (engine.rs, `ReedlineEvent::OverwriteMode`); mirror it so
            // `R` sessions overstrike in the probe like in the product.
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

    /// The engine accepted the line: the next `read_line` starts on a
    /// fresh buffer while the mode machine carries over.
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
fn issue50_dot_repeat_of_an_r_session_replaces_instead_of_inserting() {
    let mut d = Driver::new("abcd", 0, ViMode::Normal);
    d.keys("R");
    d.keys("XY");
    assert_eq!(d.state().0, "XYcd");
    d.esc();
    assert!(!d.editor.overwrite());
    // Move onto the `c` and replay: the recorded text must overstrike
    // the `cd` span, not push it right.
    d.keys("l");
    d.keys(".");
    assert_eq!(d.state().0, "XYXY");
    // The replay is atomic in command mode (GNU `rl_vi_redo` never
    // enters insert for it) and leaves overstrike off.
    assert_eq!(d.mode(), ViMode::Normal);
    assert!(!d.editor.overwrite());
    // Chain: a plain insert session afterwards really inserts.
    d.keys("0");
    d.keys("i");
    d.keys("Z");
    assert_eq!(d.state().0, "ZXYXY");
}

#[test]
fn issue50_dot_repeat_of_an_r_session_at_the_line_tail() {
    let mut d = Driver::new("abcdef", 0, ViMode::Normal);
    d.keys("R");
    d.keys("ZZ");
    d.esc();
    assert_eq!(d.state().0, "ZZcdef");
    d.keys("lll");
    d.keys(".");
    assert_eq!(d.state().0, "ZZcdZZ");
    // On the very last grapheme the recorded text is longer than the
    // span left: the tail is replaced and the rest appended, as
    // `_rl_replace_text` clamps its span to the buffer end.
    d.keys("l");
    d.keys(".");
    assert_eq!(d.state().0, "ZZcdZZZ");
    assert!(!d.editor.overwrite());
}

#[test]
fn issue50_an_r_session_ended_by_enter_replays_as_a_replace() {
    // Composes with the Enter-ended session fold: GNU saves an `R`
    // session's text on submit too (`_rl_vi_save_replace` via
    // text.c:1158), and its redo is still the replace form.
    let mut d = Driver::new("abcd", 0, ViMode::Normal);
    d.keys("R");
    d.keys("XY");
    assert_eq!(d.state().0, "XYcd");
    d.key(KeyCode::Enter, KeyModifiers::NONE);
    d.new_line("wxyz", 0);
    assert!(!d.editor.overwrite());
    d.esc();
    d.keys(".");
    assert_eq!(d.state().0, "XYyz");
    assert!(!d.editor.overwrite());
}
