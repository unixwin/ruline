//! A6 audit probes — vi mode, GNU readline vi (vi_mode.c) / POSIX vi semantics.
#![cfg(test)]

use super::{Vi, ViMode};
use crate::core_editor::Editor;
use crate::prompt::PromptViMode;
use crate::{EditCommand, EditMode, PromptEditMode, ReedlineEvent, ReedlineRawEvent, UndoBehavior};
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

    fn ctrl(&mut self, c: char) {
        self.key(KeyCode::Char(c), KeyModifiers::CONTROL);
    }

    fn esc(&mut self) {
        self.key(KeyCode::Esc, KeyModifiers::NONE);
    }

    fn keys(&mut self, s: &str) {
        // crossterm reports a shifted letter as Char('X') + SHIFT
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

// ---------------- operators with word motions ----------------

#[test]
fn v01_dw_deletes_word_and_space() {
    let mut d = Driver::new("hello world", 0, ViMode::Normal);
    d.keys("dw");
    assert_eq!(d.state(), ("world".into(), 0));
}

#[test]
fn v02_de_deletes_to_word_end() {
    let mut d = Driver::new("hello world", 0, ViMode::Normal);
    d.keys("de");
    assert_eq!(d.state(), (" world".into(), 0));
}

#[test]
fn v03_dw_on_last_word() {
    let mut d = Driver::new("hello world", 6, ViMode::Normal);
    d.keys("dw");
    assert_eq!(d.state(), ("hello ".into(), 5));
}

#[test]
fn v04_cw_is_like_ce_not_dw() {
    // classic vi rule (and GNU rl_vi): cw acts on the word end only
    let mut d = Driver::new("hello world", 0, ViMode::Normal);
    d.keys("cw");
    assert_eq!(d.state(), (" world".into(), 0));
    assert_eq!(d.mode(), ViMode::Insert);
    d.keys("X");
    d.esc();
    assert_eq!(d.state().0, "X world");
}

#[test]
fn v05_cb_changes_back_to_word_start() {
    let mut d = Driver::new("hello world", 8, ViMode::Normal);
    d.keys("cb");
    assert_eq!(d.state(), ("hello rld".into(), 6));
    assert_eq!(d.mode(), ViMode::Insert);
}

#[test]
fn v06_d_dollar_and_d_zero() {
    let mut d = Driver::new("hello world", 2, ViMode::Normal);
    d.keys("d$");
    assert_eq!(d.state(), ("he".into(), 1));
    let mut d = Driver::new("hello world", 8, ViMode::Normal);
    d.keys("d0");
    assert_eq!(d.state(), ("rld".into(), 0));
}

#[test]
fn v07_dd_removes_whole_line() {
    let mut d = Driver::new("one\ntwo\nthree", 5, ViMode::Normal);
    d.keys("dd");
    assert_eq!(d.state().0, "one\nthree");
}

#[test]
fn v08_df_deletes_through_char() {
    let mut d = Driver::new("a,b,c", 0, ViMode::Normal);
    d.keys("df,");
    assert_eq!(d.state(), ("b,c".into(), 0));
}

#[test]
fn v09_dt_deletes_up_to_char() {
    let mut d = Driver::new("a,b,c", 0, ViMode::Normal);
    d.keys("dt,");
    assert_eq!(d.state(), (",b,c".into(), 0));
}

#[test]
fn v10_counts_multiply_and_prefix() {
    let mut d = Driver::new("one two three four", 0, ViMode::Normal);
    d.keys("d2w");
    assert_eq!(d.state(), ("three four".into(), 0));
    let mut d = Driver::new("one two three four five six", 0, ViMode::Normal);
    d.keys("2d2w");
    assert_eq!(d.state(), ("five six".into(), 0));
}

#[test]
fn v11_x_with_count() {
    let mut d = Driver::new("abcdef", 0, ViMode::Normal);
    d.keys("3x");
    assert_eq!(d.state(), ("def".into(), 0));
}

#[test]
fn v12_w_walks_punctuation_as_words() {
    let mut d = Driver::new("foo.bar baz", 0, ViMode::Normal);
    d.keys("w");
    assert_eq!(d.state().1, 3);
    d.keys("w");
    assert_eq!(d.state().1, 4);
    d.keys("w");
    assert_eq!(d.state().1, 8);
}

#[test]
fn v13_e_walks_word_ends() {
    let mut d = Driver::new("foo.bar baz", 0, ViMode::Normal);
    d.keys("e");
    assert_eq!(d.state().1, 2);
    d.keys("e");
    assert_eq!(d.state().1, 3);
    d.keys("e");
    assert_eq!(d.state().1, 6);
}

#[test]
fn v14_semicolon_repeats_find_comma_reverses() {
    let mut d = Driver::new("a,b,c,d", 0, ViMode::Normal);
    d.keys("f,");
    assert_eq!(d.state().1, 1);
    d.keys(";");
    assert_eq!(d.state().1, 3);
    d.keys(";");
    assert_eq!(d.state().1, 5);
    d.keys(",");
    assert_eq!(d.state().1, 3);
}

#[test]
fn v15_r_replaces_char() {
    let mut d = Driver::new("abc", 0, ViMode::Normal);
    d.keys("rx");
    assert_eq!(d.state(), ("xbc".into(), 0));
    assert_eq!(d.mode(), ViMode::Normal);
}

#[test]
fn v16_tilde_toggles_and_advances() {
    let mut d = Driver::new("aBc", 0, ViMode::Normal);
    d.keys("~");
    assert_eq!(d.state(), ("ABc".into(), 1));
}

#[test]
fn v18_s_substitutes_char() {
    let mut d = Driver::new("abc", 0, ViMode::Normal);
    d.keys("s");
    assert_eq!(d.mode(), ViMode::Insert);
    d.keys("Z");
    d.esc();
    assert_eq!(d.state().0, "Zbc");
}

#[test]
fn v19_yy_and_p_linewise() {
    let mut d = Driver::new("one\ntwo", 0, ViMode::Normal);
    d.keys("yy");
    d.keys("p");
    assert_eq!(d.state().0, "one\none\ntwo");
}

#[test]
fn v20_charwise_paste_after_and_before() {
    let mut d = Driver::new("hello world", 0, ViMode::Normal);
    d.keys("dw");
    assert_eq!(d.state().0, "world");
    d.keys("$");
    d.keys("p");
    assert_eq!(d.state().0, "worldhello ");
    let mut d = Driver::new("hello world", 0, ViMode::Normal);
    d.keys("dw");
    d.keys("P");
    assert_eq!(d.state().0, "hello world");
}

#[test]
fn v21_dd_then_p_moves_line_down() {
    let mut d = Driver::new("one\ntwo", 0, ViMode::Normal);
    d.keys("dd");
    assert_eq!(d.state().0, "two");
    d.keys("p");
    assert_eq!(d.state().0, "two\none");
}

#[test]
fn v22_percent_matches_bracket() {
    let mut d = Driver::new("a(b)c", 1, ViMode::Normal);
    d.keys("%");
    assert_eq!(d.state().1, 3);
    d.keys("%");
    assert_eq!(d.state().1, 1);
}

#[test]
fn v23_caret_first_nonblank() {
    let mut d = Driver::new("  indented", 0, ViMode::Normal);
    d.keys("^");
    assert_eq!(d.state().1, 2);
}

#[test]
fn v24_visual_delete() {
    let mut d = Driver::new("hello world", 0, ViMode::Normal);
    d.keys("v");
    d.keys("lll");
    d.keys("d");
    assert_eq!(d.state(), ("o world".into(), 0));
    assert_eq!(d.mode(), ViMode::Normal);
}

#[test]
fn v25_a_enters_insert_after_char() {
    let mut d = Driver::new("ab", 0, ViMode::Normal);
    d.keys("a");
    assert_eq!(d.mode(), ViMode::Insert);
    d.keys("X");
    assert_eq!(d.state().0, "aXb");
}

#[test]
fn v27_dot_repeats_last_change() {
    // GNU readline vi: '.' is rl_vi_redo
    let mut d = Driver::new("one two three", 0, ViMode::Normal);
    d.keys("dw");
    assert_eq!(d.state().0, "two three");
    d.keys(".");
    assert_eq!(d.state().0, "three");
}

#[test]
fn v28_u_undoes_dw() {
    let mut d = Driver::new("hello world", 0, ViMode::Normal);
    d.keys("dw");
    d.keys("u");
    assert_eq!(d.state().0, "hello world");
}

#[test]
fn v29_insert_mode_ctrl_w_rubs_out_word() {
    // GNU vi insert keymap: C-w = unix-word-rubout
    let mut d = Driver::new("", 0, ViMode::Insert);
    d.keys("hello world");
    d.ctrl('w');
    assert_eq!(d.state().0, "hello ");
}

#[test]
fn a6_f09_insert_mode_ctrl_u_discards_line() {
    // GNU vi insert keymap: C-u = unix-line-discard
    let mut d = Driver::new("", 0, ViMode::Insert);
    d.keys("hello world");
    d.ctrl('u');
    assert_eq!(d.state().0, "");
}

#[test]
fn v31_o_is_unbound_in_gnu_vi_command_mode() {
    // GNU readline's vi-command keymap has no `o`/`O` (that is vim's
    // open-line; `bind -m vi-command -p` shows no such row): pressing it
    // must neither edit nor change mode.
    let mut d = Driver::new("one\ntwo", 0, ViMode::Normal);
    d.keys("o");
    assert_eq!(d.mode(), ViMode::Normal);
    assert_eq!(d.state().0, "one\ntwo");
}

#[test]
fn v32_gg_and_g() {
    let mut d = Driver::new("one\ntwo\nthree", 9, ViMode::Normal);
    d.keys("gg");
    assert_eq!(d.state().1, 0);
    d.keys("G");
    assert_eq!(d.state().0, "one\ntwo\nthree");
    assert!(d.state().1 >= 8, "G lands on the last line, got {}", d.state().1);
}

#[test]
fn v33_db_from_word_middle() {
    let mut d = Driver::new("hello world", 8, ViMode::Normal);
    d.keys("db");
    assert_eq!(d.state(), ("hello rld".into(), 6));
}

#[test]
fn v35_yank_word_then_paste_charwise() {
    let mut d = Driver::new("one two", 0, ViMode::Normal);
    d.keys("yw");
    assert_eq!(d.state(), ("one two".into(), 0));
    d.keys("$");
    d.keys("p");
    assert_eq!(d.state().0, "one twoone ");
}

#[test]
fn v36_dj_deletes_two_lines() {
    let mut d = Driver::new("one\ntwo\nthree", 0, ViMode::Normal);
    d.keys("dj");
    assert_eq!(d.state().0, "three");
}

#[test]
fn v37_x_at_line_end_stays() {
    let mut d = Driver::new("ab", 1, ViMode::Normal);
    d.keys("x");
    assert_eq!(d.state(), ("a".into(), 0));
}

#[test]
fn v38_b_from_start_of_second_word() {
    let mut d = Driver::new("foo bar", 4, ViMode::Normal);
    d.keys("b");
    assert_eq!(d.state().1, 0);
}

#[test]
fn v39_find_char_no_match_is_noop() {
    let mut d = Driver::new("abc", 0, ViMode::Normal);
    d.keys("fz");
    assert_eq!(d.state(), ("abc".into(), 0));
}

#[test]
fn v40_d_with_failed_motion_deletes_nothing() {
    let mut d = Driver::new("abc", 0, ViMode::Normal);
    d.keys("dfz");
    assert_eq!(d.state(), ("abc".into(), 0));
}

// ---------------- round 3 ----------------

#[test]
fn v42_big_r_enters_replace_mode() {
    // GNU vi-command: R = rl_vi_replace (vi_keymap.c:126) — overwrite
    // until Esc.
    let mut d = Driver::new("abc", 0, ViMode::Normal);
    d.keys("R");
    assert_eq!(d.mode(), ViMode::Insert);
    d.keys("XY");
    d.esc();
    assert_eq!(d.state().0, "XYc");
}

#[test]
fn v43_big_x_deletes_char_before_cursor() {
    let mut d = Driver::new("abc", 2, ViMode::Normal);
    d.keys("X");
    assert_eq!(d.state(), ("ac".into(), 1));
}

#[test]
fn v44_big_s_rewrites_line() {
    let mut d = Driver::new("hello world", 2, ViMode::Normal);
    d.keys("S");
    assert_eq!(d.mode(), ViMode::Insert);
    d.keys("Z");
    d.esc();
    assert_eq!(d.state().0, "Z");
}

#[test]
fn v45_text_object_inner_and_around_word() {
    let mut d = Driver::new("foo bar baz", 5, ViMode::Normal);
    d.keys("diw");
    assert_eq!(d.state(), ("foo  baz".into(), 4));
    let mut d = Driver::new("foo bar baz", 5, ViMode::Normal);
    d.keys("daw");
    assert_eq!(d.state(), ("foo baz".into(), 4));
}

#[test]
fn v46_change_inside_quotes() {
    let mut d = Driver::new("say \"hi\" there", 6, ViMode::Normal);
    d.keys("ci\"");
    assert_eq!(d.mode(), ViMode::Insert);
    d.keys("YO");
    d.esc();
    assert_eq!(d.state().0, "say \"YO\" there");
}

#[test]
fn v47_backward_find_and_repeat_directions() {
    let mut d = Driver::new("a,b,c", 4, ViMode::Normal);
    d.keys("F,");
    assert_eq!(d.state().1, 3);
    d.keys(";");
    assert_eq!(d.state().1, 1);
    d.keys(",");
    assert_eq!(d.state().1, 3);
}

#[test]
fn v48_paste_with_count() {
    let mut d = Driver::new("one\ntwo", 0, ViMode::Normal);
    d.keys("yy");
    d.keys("2p");
    assert_eq!(d.state().0, "one\none\none\ntwo");
}

#[test]
fn v49_motions_at_buffer_edges_stay() {
    let mut d = Driver::new("ab cd", 4, ViMode::Normal);
    d.keys("e");
    assert_eq!(d.state().1, 4);
    let mut d = Driver::new("ab cd", 3, ViMode::Normal);
    d.keys("w");
    assert_eq!(d.state().1, 4);
}

#[test]
fn v50_cc_changes_whole_line_keeping_indent_slot() {
    let mut d = Driver::new("one\ntwo\nthree", 5, ViMode::Normal);
    d.keys("cc");
    assert_eq!(d.mode(), ViMode::Insert);
    assert_eq!(d.state().0, "one\n\nthree");
}

#[test]
fn v51_dd_only_line_and_last_line() {
    let mut d = Driver::new("solo", 0, ViMode::Normal);
    d.keys("dd");
    assert_eq!(d.state().0, "");
    let mut d = Driver::new("one\ntwo", 4, ViMode::Normal);
    d.keys("dd");
    assert_eq!(d.state().0, "one");
    let mut d = Driver::new("one\ntwo", 0, ViMode::Normal);
    d.keys("dd");
    assert_eq!(d.state().0, "two");
}

#[test]
fn issue42_cw_on_a_wide_gap_changes_a_single_space() {
    // GNU's domove cleanup (vi_mode.c:1250-1272) collapses the `cw`
    // span onto the mark plus one char, whatever the gap width.
    let mut d = Driver::new("one  two", 3, ViMode::Normal);
    d.keys("cw");
    d.keys("X");
    d.esc();
    assert_eq!(d.state().0, "oneX two");
    // Same for `cW` (the LongWord flavor of the same rule).
    let mut d = Driver::new("one  two", 3, ViMode::Normal);
    d.keys("cW");
    d.keys("X");
    d.esc();
    assert_eq!(d.state().0, "oneX two");
}

#[test]
fn v52_dj_on_last_line_does_nothing() {
    let mut d = Driver::new("one\ntwo", 4, ViMode::Normal);
    d.keys("dj");
    assert_eq!(d.state().0, "one\ntwo");
}

#[test]
#[ignore = "premise refuted 2026-11-04 (Wave A ruline lane): GNU _rl_char_search_internal \
            steps one char past point before checking (text.c:1720), so a repeat from \
            the t/T landing spot re-matches the same char — GNU is stuck too (PTY-probed); \
            the advance expectation below is vim semantics. Kept ignored pending captain \
            adjudication of the pin itself"]
fn v53_operator_semicolon_repeats_till() {
    let mut d = Driver::new("ab,cd,ef", 0, ViMode::Normal);
    d.keys("t,");
    assert_eq!(d.state().1, 1);
    d.keys(";");
    assert_eq!(d.state().1, 4);
    let mut d = Driver::new("ab,cd,ef", 0, ViMode::Normal);
    d.keys("t,");
    d.keys("d;");
    assert_eq!(d.state(), ("a,ef".into(), 1));
}

#[test]
fn v59_dk_on_first_line_does_nothing() {
    let mut d = Driver::new("one\ntwo", 0, ViMode::Normal);
    d.keys("dk");
    assert_eq!(d.state().0, "one\ntwo");
}

#[test]
fn v60_dj_on_first_line_deletes_both_lines() {
    let mut d = Driver::new("one\ntwo", 0, ViMode::Normal);
    d.keys("dj");
    assert_eq!(d.state().0, "");
}

#[test]
fn v54_counted_find() {
    let mut d = Driver::new("a,b,c,d", 0, ViMode::Normal);
    d.keys("2f,");
    assert_eq!(d.state().1, 3);
    let mut d = Driver::new("a,b,c,d", 0, ViMode::Normal);
    d.keys("d2f,");
    assert_eq!(d.state().0, "c,d");
}

#[test]
fn v57_change_inner_word_on_whitespace() {
    let mut d = Driver::new("foo  bar", 4, ViMode::Normal);
    d.keys("ciw");
    assert_eq!(d.mode(), ViMode::Insert);
    assert_eq!(d.state().0, "foobar");
}

#[test]
fn v58_counted_charwise_paste() {
    let mut d = Driver::new("ab", 0, ViMode::Normal);
    d.keys("x");
    assert_eq!(d.state().0, "b");
    d.keys("2p");
    assert_eq!(d.state().0, "baa");
}

#[test]
fn v61_tilde_with_count() {
    let mut d = Driver::new("ab", 0, ViMode::Normal);
    d.keys("2~");
    assert_eq!(d.state().0, "AB");
}

#[test]
fn v62_dot_repeat_replays_change_and_its_text() {
    let mut d = Driver::new("one two three", 0, ViMode::Normal);
    d.keys("cw");
    d.keys("X");
    d.esc();
    assert_eq!(d.state().0, "X two three");
    d.keys("w");
    d.keys(".");
    assert_eq!(d.state().0, "X X three");
}

#[test]
fn issue43_dot_repeat_folds_rubouts_into_the_captured_text() {
    // Type "XY", rub out the "Y": the replay must insert only "X"
    // (GNU folds the session's rubouts into `_rl_vi_last_insert`).
    let mut d = Driver::new("one two three", 0, ViMode::Normal);
    d.keys("cw");
    d.keys("XY");
    d.key(KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(d.state().0, "X two three");
    d.esc();
    d.keys("w");
    d.keys(".");
    assert_eq!(d.state().0, "X X three");
}

#[test]
fn issue43_dot_repeat_after_an_empty_insert_session_replays_the_cut_only() {
    // `cw` straight to Esc typed nothing: `.` must not gain a phantom
    // insert from the empty capture.
    let mut d = Driver::new("one two three", 0, ViMode::Normal);
    d.keys("cw");
    d.esc();
    assert_eq!(d.state().0, " two three");
    d.keys("w");
    d.keys(".");
    assert_eq!(d.state().0, " three");
}

#[test]
fn issue36_replace_mode_turns_overstrike_back_off_at_esc() {
    let mut d = Driver::new("abc", 0, ViMode::Normal);
    d.keys("R");
    d.keys("X");
    assert_eq!(d.state().0, "Xbc");
    assert!(d.editor.overwrite());
    d.esc();
    assert!(!d.editor.overwrite());
    // A following plain insert session inserts, not overstrikes.
    d.keys("i");
    d.keys("Y");
    assert_eq!(d.state().0, "YXbc");
}

#[test]
fn v63_dot_repeat_replays_plain_insert() {
    let mut d = Driver::new("ab", 0, ViMode::Normal);
    d.keys("i");
    d.keys("Z");
    d.esc();
    assert_eq!(d.state().0, "Zab");
    d.keys("l");
    d.keys(".");
    assert_eq!(d.state().0, "ZZab");
}
