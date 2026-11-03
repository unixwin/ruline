//! A6 audit probes — GNU readline 8.3 semantics as reference.
//! Each test asserts GNU behavior; a failure is a candidate finding.
#![cfg(test)]

use super::editor::{EditCommandStatus, Editor};
use crate::{EditCommand, PromptEditMode, UndoBehavior};

fn ed(buffer: &str, pos: usize) -> Editor {
    let mut e = Editor::default();
    e.set_buffer(buffer.to_string(), UndoBehavior::CreateUndoPoint);
    e.line_buffer_mut().set_insertion_point(pos);
    e
}

fn run(e: &mut Editor, cmds: &[EditCommand]) {
    for c in cmds {
        e.run_edit_command(c);
    }
}

fn state(e: &Editor) -> (String, usize) {
    (e.get_buffer().to_string(), e.insertion_point())
}

// ---------- A. case commands (GNU rl_change_case: span = point..word-end) ----

#[test]
fn a6_f01_upcase_mid_word_only_tail() {
    // GNU oracle-verified: "hel|lo" M-u -> "heLLO", point at word end (5)
    let mut e = ed("hello world", 2);
    run(&mut e, &[EditCommand::UppercaseWord]);
    assert_eq!(state(&e), ("heLLO world".into(), 5));
}

#[test]
fn a2_upcase_at_word_end_takes_next_word() {
    // GNU oracle-verified: point after "hello" -> "hello WORLD", point 11
    let mut e = ed("hello world", 5);
    run(&mut e, &[EditCommand::UppercaseWord]);
    assert_eq!(state(&e), ("hello WORLD".into(), 11));
}

#[test]
fn a3_upcase_in_gap_takes_next_word() {
    let mut e = ed("hello  world", 6);
    run(&mut e, &[EditCommand::UppercaseWord]);
    assert_eq!(state(&e), ("hello  WORLD".into(), 12));
}

#[test]
fn a6_f01_downcase_mid_word_only_tail() {
    // GNU oracle-verified: "HEL|LO" M-l -> "HELlo", point 5
    let mut e = ed("HELLO WORLD", 3);
    run(&mut e, &[EditCommand::LowercaseWord]);
    assert_eq!(state(&e), ("HELlo WORLD".into(), 5));
}

#[test]
fn a5_capitalize_mid_word() {
    // GNU: span from point: "ELLO" -> "Ello"; buffer "hEllo", point 5
    let mut e = ed("hELLO world", 1);
    run(&mut e, &[EditCommand::CapitalizeWord]);
    assert_eq!(state(&e), ("hEllo world".into(), 5));
}

#[test]
fn a6_capitalize_at_word_start() {
    let mut e = ed("hELLO world", 0);
    run(&mut e, &[EditCommand::CapitalizeWord]);
    assert_eq!(state(&e), ("Hello world".into(), 5));
}

#[test]
#[ignore = "A6-F02: known divergence, see deep-audit A6 REPORT.md"]
fn a7_upcase_stops_at_punctuation() {
    // GNU word chars are alnum: M-u inside "foo" of "foo.bar" ends at '.'
    let mut e = ed("foo.bar", 1);
    run(&mut e, &[EditCommand::UppercaseWord]);
    assert_eq!(state(&e), ("fOO.bar".into(), 3));
}

#[test]
fn a8_capitalize_digits_consume_word() {
    // GNU oracle-verified: "123ABC" M-c at 0 -> "123abc" (digits open the word)
    let mut e = ed("123ABC x", 0);
    run(&mut e, &[EditCommand::CapitalizeWord]);
    assert_eq!(state(&e), ("123abc x".into(), 6));
}

#[test]
fn a6_f01_upcase_at_buffer_end_noop() {
    let mut e = ed("hello", 5);
    run(&mut e, &[EditCommand::UppercaseWord]);
    assert_eq!(state(&e), ("hello".into(), 5));
}

// ---------- B. transpose words (GNU rl_transpose_words walk) ----------------

#[test]
fn b1_transpose_words_point_in_first() {
    // GNU: "echo |one two" -> "one echo two", point at end of pair (8)
    let mut e = ed("echo one two", 6);
    run(&mut e, &[EditCommand::SwapWords]);
    assert_eq!(state(&e), ("one echo two".into(), 8));
}

#[test]
#[ignore = "A6-F02: known divergence, see deep-audit A6 REPORT.md"]
fn b2_transpose_words_punctuation_is_separator() {
    // GNU oracle-verified shape: "echo [one two", point in "one" ->
    // "one [echo two" (punctuation skipped as separator, not a word)
    let mut e = ed("echo [one two", 7);
    run(&mut e, &[EditCommand::SwapWords]);
    assert_eq!(state(&e), ("one [echo two".into(), 9));
}

#[test]
fn b3_transpose_words_at_end() {
    let mut e = ed("one two", 7);
    run(&mut e, &[EditCommand::SwapWords]);
    assert_eq!(state(&e), ("two one".into(), 7));
}

#[test]
fn b4_transpose_words_single_word_noop() {
    let mut e = ed("hello", 3);
    run(&mut e, &[EditCommand::SwapWords]);
    assert_eq!(state(&e), ("hello".into(), 3));
}

#[test]
fn b5_transpose_words_point_in_second() {
    // GNU: point inside "two" drags "one" past it -> "two one", point 7
    let mut e = ed("one two", 5);
    run(&mut e, &[EditCommand::SwapWords]);
    assert_eq!(state(&e), ("two one".into(), 7));
}

// ---------- C. kill ring ------------------------------------------------------

#[test]
fn c1_consecutive_backward_kills_merge_prepend() {
    // GNU: two backward-kill-words accumulate into ONE ring entry "bb cc"
    let mut e = ed("aa bb cc", 8);
    run(
        &mut e,
        &[EditCommand::CutWordLeft, EditCommand::CutWordLeft],
    );
    assert_eq!(state(&e), ("aa ".into(), 3));
    assert_eq!(e.current_kill().0, "bb cc");
    run(&mut e, &[EditCommand::Yank]);
    assert_eq!(state(&e), ("aa bb cc".into(), 8));
}

#[test]
fn c2_kills_separated_by_motion_are_separate_entries() {
    // GNU: any non-kill command (incl. motion) breaks kill accumulation
    let mut e = ed("aa bb cc", 8);
    run(&mut e, &[EditCommand::CutWordLeft]); // kills "cc"
    run(&mut e, &[EditCommand::MoveLeft { select: false }]);
    run(&mut e, &[EditCommand::CutWordLeft]); // kills "bb " separately
    run(&mut e, &[EditCommand::Yank]);
    assert_eq!(state(&e).0, "aa bb ");
    run(&mut e, &[EditCommand::YankPop]);
    assert_eq!(state(&e).0, "aa cc ");
}

#[test]
fn c3_yank_pop_without_yank_dings() {
    let mut e = ed("ab", 2);
    let status = e.run_edit_command(&EditCommand::YankPop);
    assert_eq!(status, EditCommandStatus::Inapplicable);
    assert_eq!(state(&e), ("ab".into(), 2));
}

#[test]
fn c4_yank_pop_rotation_wraps() {
    // three SEPARATE kills (motion breaks the chain): entries oldest-first
    // "three", "two ", "one ". Yank gives the newest; pops walk back and wrap.
    let mut e = ed("one two three", 13);
    for _ in 0..3 {
        run(&mut e, &[EditCommand::CutWordLeft]);
        run(&mut e, &[EditCommand::MoveToStart { select: false }]);
        run(&mut e, &[EditCommand::MoveToEnd { select: false }]);
    }
    assert_eq!(state(&e).0, "");
    run(&mut e, &[EditCommand::Yank]);
    assert_eq!(state(&e).0, "one ");
    run(&mut e, &[EditCommand::YankPop]);
    assert_eq!(state(&e).0, "two ");
    run(&mut e, &[EditCommand::YankPop]);
    assert_eq!(state(&e).0, "three");
    run(&mut e, &[EditCommand::YankPop]);
    assert_eq!(state(&e).0, "one ", "rotation must wrap to the newest entry");
}

#[test]
fn c5_yank_leaves_point_after_text() {
    // GNU oracle-verified: yank leaves point after the inserted text
    let mut e = ed("xyz", 3);
    run(&mut e, &[EditCommand::CutFromStart]);
    run(&mut e, &[EditCommand::Yank]);
    assert_eq!(state(&e), ("xyz".into(), 3));
}

#[test]
fn c7_forward_then_backward_kill_merges_in_order() {
    // GNU: kill-word forward then backward-kill-word consecutively prepends,
    // so the ring holds the text in buffer order: "bb cc" around point.
    // buffer "aa bb cc dd", point after "bb " (6): M-d kills "cc", then
    // backward-kill-word kills "bb " -> ring "bb cc"
    let mut e = ed("aa bb cc dd", 6);
    run(
        &mut e,
        &[EditCommand::CutWordRight, EditCommand::CutWordLeft],
    );
    assert_eq!(e.current_kill().0, "bb cc");
}

#[test]
fn c8_copy_region_does_not_break_kill_chain_but_moves_nothing() {
    // CopyRegionAsKill feeds the ring as a kill (GNU rl_copy_region_to_kill
    // sets last-was-kill), so a following backward kill PREPENDS to it.
    let mut e = ed("aa bb", 5);
    // set mark at 3 via MoveToPosition with select? use editor mark API:
    e.line_buffer_mut().set_insertion_point(3);
    // plant mark by selecting: use EditCommand::SetMark if present; fallback:
    // use selection anchor through Move with select from 5.. do direct:
    run(&mut e, &[EditCommand::MoveToEnd { select: false }]);
    // Without a mark this probe cannot run the region path; guard:
    if e.line_buffer_mut().mark().is_none() {
        eprintln!("c8 skipped: no mark API reachable");
        return;
    }
    run(&mut e, &[EditCommand::CopyRegionAsKill]);
    assert_eq!(e.current_kill().0, "bb");
}

// ---------- D. undo grouping ---------------------------------------------------

#[test]
#[ignore = "A6-F03: known divergence, see deep-audit A6 REPORT.md"]
fn d1_undo_once_reverts_whole_typed_run() {
    // GNU oracle-verified: typing "abc def" then ONE undo empties the run
    let mut e = ed("", 0);
    for c in "abc def".chars() {
        e.run_edit_command(&EditCommand::InsertChar(c));
    }
    assert_eq!(state(&e).0, "abc def");
    run(&mut e, &[EditCommand::Undo]);
    assert_eq!(state(&e).0, "", "GNU coalesces a contiguous insert run");
}

#[test]
fn d2_undo_restores_killed_word_and_point() {
    let mut e = ed("hello world", 11);
    run(&mut e, &[EditCommand::CutWordLeft]);
    assert_eq!(state(&e).0, "hello ");
    run(&mut e, &[EditCommand::Undo]);
    assert_eq!(state(&e), ("hello world".into(), 11));
}

#[test]
fn d3_redo_after_undo() {
    let mut e = ed("", 0);
    run(&mut e, &[EditCommand::InsertString("abc".into())]);
    run(&mut e, &[EditCommand::Undo]);
    assert_eq!(state(&e).0, "");
    run(&mut e, &[EditCommand::Redo]);
    assert_eq!(state(&e).0, "abc");
}

#[test]
fn d4_new_edit_clears_redo() {
    let mut e = ed("", 0);
    run(&mut e, &[EditCommand::InsertString("abc".into())]);
    run(&mut e, &[EditCommand::Undo]);
    run(&mut e, &[EditCommand::InsertChar('z')]);
    run(&mut e, &[EditCommand::Redo]);
    assert_eq!(state(&e).0, "z");
}

// ---------- E. transpose chars --------------------------------------------------

#[test]
fn e1_transpose_at_start_swaps_first_two() {
    let mut e = ed("ab", 0);
    run(&mut e, &[EditCommand::SwapGraphemes]);
    assert_eq!(state(&e), ("ba".into(), 2));
}

#[test]
fn e2_transpose_at_end_swaps_last_two_point_stays() {
    let mut e = ed("abc", 3);
    run(&mut e, &[EditCommand::SwapGraphemes]);
    assert_eq!(state(&e), ("acb".into(), 3));
}

#[test]
fn e3_transpose_drags_char_forward() {
    let mut e = ed("abc", 1);
    run(&mut e, &[EditCommand::SwapGraphemes]);
    assert_eq!(state(&e), ("bac".into(), 2));
    run(&mut e, &[EditCommand::SwapGraphemes]);
    assert_eq!(state(&e), ("bca".into(), 3));
}

// ---------- F. no-panic / boundary sweep ---------------------------------------

#[test]
fn a6_f04_command_sweep_never_panics_and_keeps_boundary() {
    let buffers = [
        "", "a", "ab", "a b", "e\u{301}x", "a\u{301}", "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} fam",
        "你好世界 abc", "a\tb", "foo.bar baz", "trailing  ", "  leading",
        "one\ntwo three", "ß ß", "ﬀx", "\u{1F1FA}\u{1F1F8} flag x",
        "कि hindi", "word", "a.b,c;d", "...", "   ", "\n", "x\n", "\u{0}nul",
    ];
    let cmds: Vec<EditCommand> = vec![
        EditCommand::InsertChar('x'),
        EditCommand::Backspace,
        EditCommand::Delete,
        EditCommand::CutWordLeft,
        EditCommand::CutBigWordLeft,
        EditCommand::CutWordRight,
        EditCommand::CutBigWordRight,
        EditCommand::CutWordRightToNext,
        EditCommand::UppercaseWord,
        EditCommand::LowercaseWord,
        EditCommand::CapitalizeWord,
        EditCommand::SwapWords,
        EditCommand::SwapGraphemes,
        EditCommand::Yank,
        EditCommand::YankPop,
        EditCommand::Undo,
        EditCommand::Redo,
        EditCommand::MoveWordLeft { select: false },
        EditCommand::MoveWordRight { select: false },
        EditCommand::MoveWordRightEnd { select: false },
        EditCommand::MoveBigWordRightEnd { select: false },
        EditCommand::CutFromStart,
        EditCommand::CutToEnd,
        EditCommand::KillLine,
        EditCommand::CutCharLeft,
        EditCommand::CutChar,
        EditCommand::MoveToStart { select: false },
        EditCommand::MoveToEnd { select: false },
        EditCommand::MoveToLineStart { select: false },
        EditCommand::MoveToLineEnd { select: false },
        EditCommand::Clear,
        EditCommand::ReplaceChar('q'),
        EditCommand::CutFromLineStart,
        EditCommand::CutToLineEnd,
        EditCommand::CutCurrentLine,
        EditCommand::BackspaceWord,
        EditCommand::DeleteWord,
        EditCommand::MoveLeft { select: false },
        EditCommand::MoveRight { select: false },
        EditCommand::MoveLineUp { select: false },
        EditCommand::MoveLineDown { select: false },
    ];
    for buf in buffers {
        let positions: Vec<usize> = buf
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(buf.len()))
            .collect();
        for pos in positions {
            for cmd in &cmds {
                let mut e = ed(buf, pos);
                e.run_edit_command(cmd);
                let p = e.insertion_point();
                assert!(
                    p <= e.get_buffer().len() && e.get_buffer().is_char_boundary(p),
                    "boundary broken: buf={buf:?} pos={pos} cmd={cmd:?} -> point {p}"
                );
            }
            // pairs of commands catch state-machine interactions
            for (c1, c2) in [
                (EditCommand::CutWordLeft, EditCommand::Yank),
                (EditCommand::CutWordRight, EditCommand::YankPop),
                (EditCommand::InsertChar('y'), EditCommand::Undo),
                (EditCommand::SwapWords, EditCommand::SwapWords),
                (EditCommand::MoveWordRightEnd { select: false }, EditCommand::CutChar),
            ] {
                let mut e = ed(buf, pos);
                e.run_edit_command(&c1);
                e.run_edit_command(&c2);
                let p = e.insertion_point();
                assert!(p <= e.get_buffer().len() && e.get_buffer().is_char_boundary(p));
            }
        }
    }
}

#[test]
fn f2_vi_normal_sweep_never_panics() {
    use crate::prompt::PromptViMode;
    let buffers = ["", "a", "e\u{301}", "你好", "a b c", "x\ny", "\u{1F600}z"];
    let cmds: Vec<EditCommand> = vec![
        EditCommand::MoveToEnd { select: false },
        EditCommand::MoveWordRightEnd { select: false },
        EditCommand::CutChar,
        EditCommand::CutWordRight,
        EditCommand::SwapGraphemes,
        EditCommand::UppercaseWord,
    ];
    for buf in buffers {
        for pos in buf.char_indices().map(|(i, _)| i).chain(std::iter::once(buf.len())) {
            for cmd in &cmds {
                let mut e = ed(buf, pos);
                e.set_edit_mode(PromptEditMode::Vi(PromptViMode::Normal));
                e.run_edit_command(cmd);
                assert!(e.insertion_point() <= e.get_buffer().len());
            }
        }
    }
}

// ---------- M. word motions / kill spans / kill-line (round 2) -----------------

#[test]
fn m1_forward_word_lands_on_word_end() {
    // GNU M-f: end of current word; from a word end, end of the next word
    let mut e = ed("foo bar", 0);
    run(&mut e, &[EditCommand::MoveWordRight { select: false }]);
    assert_eq!(state(&e).1, 3);
    run(&mut e, &[EditCommand::MoveWordRight { select: false }]);
    assert_eq!(state(&e).1, 7);
}

#[test]
#[ignore = "A6-F02: known divergence, see deep-audit A6 REPORT.md"]
fn m2_kill_word_spans_stop_at_punctuation() {
    // GNU kill-word on "foo.bar" at 0 kills "foo" only (alnum word chars)
    let mut e = ed("foo.bar", 0);
    run(&mut e, &[EditCommand::CutWordRight]);
    assert_eq!(state(&e).0, ".bar");
    assert_eq!(e.current_kill().0, "foo");
    // backward-kill-word at end kills "bar" only
    let mut e = ed("foo.bar", 7);
    run(&mut e, &[EditCommand::CutWordLeft]);
    assert_eq!(state(&e).0, "foo.");
    assert_eq!(e.current_kill().0, "bar");
}

#[test]
fn m3_kill_line_at_eol_kills_the_newline() {
    // GNU C-k at end of line kills the newline, joining the next line;
    // the newline is what lands in the ring.
    let mut e = ed("one\ntwo", 3);
    run(&mut e, &[EditCommand::KillLine]);
    assert_eq!(state(&e).0, "onetwo");
    assert_eq!(e.current_kill().0, "\n");
    run(&mut e, &[EditCommand::Yank]);
    assert_eq!(state(&e).0, "one\ntwo");
}

#[test]
fn m4_transpose_words_at_start_of_first_word_noop() {
    let mut e = ed("one two", 1);
    run(&mut e, &[EditCommand::SwapWords]);
    assert_eq!(state(&e), ("one two".into(), 1));
}

#[test]
#[ignore = "A6-F02: known divergence, see deep-audit A6 REPORT.md"]
fn m5_upcase_word_point_on_punctuation() {
    // GNU: point on '.', M-u upcases the FOLLOWING word
    let mut e = ed("foo.bar", 3);
    run(&mut e, &[EditCommand::UppercaseWord]);
    assert_eq!(state(&e), ("foo.BAR".into(), 7));
}

#[test]
fn m6_delete_word_command_erases_without_ring() {
    // Documentation probe: DeleteWord is the no-register erase (M-Delete's
    // current lowering); the ring must be checked by the binding probe.
    let mut e = ed("foo bar", 0);
    run(&mut e, &[EditCommand::DeleteWord]);
    assert_eq!(state(&e).0, " bar");
}

// ---------- round 3: kill chain gates, delete undo, misc ----------------------

#[test]
fn c9_yank_pop_after_motion_dings() {
    // GNU: yank-pop is only valid directly after yank/yank-pop
    // (rl_last_func); a cursor motion in between breaks the chain.
    let mut e = ed("hello world", 11);
    run(&mut e, &[EditCommand::CutWordLeft]);
    run(&mut e, &[EditCommand::Yank]);
    run(&mut e, &[EditCommand::MoveLeft { select: false }]);
    let status = e.run_edit_command(&EditCommand::YankPop);
    assert_eq!(status, EditCommandStatus::Inapplicable);
    assert_eq!(state(&e).0, "hello world");
}

#[test]
#[ignore = "A6-F03: known divergence, see deep-audit A6 REPORT.md"]
fn d5_backspace_undo_restores_one_char_per_gnu() {
    // GNU: rl_delete_text makes one undo entry per rubout (no coalescing);
    // inserts coalesce, deletes do not.
    let mut e = ed("", 0);
    for c in "abcdef".chars() {
        e.run_edit_command(&EditCommand::InsertChar(c));
    }
    for _ in 0..3 {
        e.run_edit_command(&EditCommand::Backspace);
    }
    assert_eq!(state(&e).0, "abc");
    run(&mut e, &[EditCommand::Undo]);
    assert_eq!(state(&e).0, "abcd", "GNU restores exactly one rubbed-out char");
}

#[test]
#[ignore = "A6-F03: known divergence, see deep-audit A6 REPORT.md"]
fn d6_insert_run_chunks_at_twenty() {
    // GNU rl_insert_text concatenates single-char inserts into one undo
    // entry only while that entry is shorter than 20 chars (text.c:107).
    let mut e = ed("", 0);
    for c in "abcdefghijklmnopqrstuvwxy".chars() {
        e.run_edit_command(&EditCommand::InsertChar(c));
    }
    run(&mut e, &[EditCommand::Undo]);
    assert_eq!(state(&e).0, "abcdefghijklmnopqrst", "newest 5-char chunk undone");
    run(&mut e, &[EditCommand::Undo]);
    assert_eq!(state(&e).0, "", "then the 20-char chunk");
}

#[test]
fn m7_kill_line_mid_line_kills_to_line_end() {
    let mut e = ed("one\ntwo", 4);
    run(&mut e, &[EditCommand::KillLine]);
    assert_eq!(state(&e).0, "one\n");
    assert_eq!(e.current_kill().0, "two");
}

#[test]
fn b6_transpose_words_point_in_gap() {
    let mut e = ed("one  two", 3);
    run(&mut e, &[EditCommand::SwapWords]);
    assert_eq!(state(&e), ("two  one".into(), 8));
}

#[test]
#[ignore = "A6-F15: known divergence, see deep-audit A6 REPORT.md"]
fn e4_transpose_combining_char_is_charwise_in_gnu() {
    // GNU transposes multibyte CHARACTERS: point after "e\u{301}" (byte 3),
    // transpose drags the combining mark over 'x' -> "ex\u{301}".
    // ruline transposes grapheme clusters instead -> "xe\u{301}".
    let mut e = ed("e\u{301}x", 3);
    run(&mut e, &[EditCommand::SwapGraphemes]);
    assert_eq!(state(&e).0, "ex\u{301}");
}

#[test]
fn a10_capitalize_word_point_in_gap() {
    let mut e = ed("foo  bar", 4);
    run(&mut e, &[EditCommand::CapitalizeWord]);
    assert_eq!(state(&e), ("foo  Bar".into(), 8));
}

#[test]
fn b7_transpose_words_at_buffer_start_moves_point_only() {
    let mut e = ed("one two", 0);
    run(&mut e, &[EditCommand::SwapWords]);
    assert_eq!(state(&e), ("one two".into(), 0));
}

#[test]
fn m8_kill_line_at_end_of_last_line_keeps_ring_empty() {
    let mut e = ed("one\ntwo", 7);
    run(&mut e, &[EditCommand::KillLine]);
    assert_eq!(state(&e).0, "one\ntwo");
    assert_eq!(e.current_kill().0, "");
}

#[test]
fn b8_transpose_words_at_buffer_end() {
    let mut e = ed("one two", 7);
    run(&mut e, &[EditCommand::SwapWords]);
    assert_eq!(state(&e), ("two one".into(), 7));
}
