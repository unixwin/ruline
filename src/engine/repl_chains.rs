//! Chain-style editor behaviour tests for the interactive REPL surface.
//!
//! Each test is a *chain*: a sequence of keystrokes where the intermediate
//! state (buffer text, insertion point in bytes and graphemes, kill-ring
//! slot/count, input mode) is asserted after **every** step, not just at
//! the end. A regression that leaves the final state correct but corrupts
//! a mid-chain state (wrong ring merge order, cursor landing inside a
//! grapheme cluster, a paste that submits line-by-line) is exactly what
//! these tests exist to catch.
//!
//! Expected behaviour follows GNU readline semantics (the engine's stated
//! contract); where a chain pins a GNU behaviour the implementation does
//! not yet have, the test is `#[ignore]`d and the divergence is recorded
//! in the lane report instead of bending the expectation.
//!
//! The interactive-test duty for this surface comes from
//! unixwin/niubash#827 (PTY/interactive gate); these are the headless
//! unit-level chains underneath it.

use super::*;
use crate::{DefaultPrompt, HistoryItem};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------

/// An Emacs-mode engine anchored the way the engine test-suite anchors
/// it, so repaint state never leaks into editing state.
fn new_rl() -> Reedline {
    let mut rl = Reedline::create().with_edit_mode(Box::<Emacs>::default());
    rl.painter.force_prompt_anchored_for_test(0);
    rl
}

fn new_rl_with_pairs(pairs: &[(char, char)]) -> Reedline {
    let mut rl = Reedline::create()
        .with_edit_mode(Box::<Emacs>::default())
        .with_auto_pairs(AutoPairs::new(pairs.iter().copied()));
    rl.painter.force_prompt_anchored_for_test(0);
    rl
}

fn drive(rl: &mut Reedline, keys: &[KeyEvent]) -> ControlFlow<Signal> {
    let prompt = DefaultPrompt::default();
    let events: Vec<Event> = keys.iter().copied().map(Event::Key).collect();
    rl.process_input_batch(&prompt, events).expect("batch ok")
}

/// One key per input batch, the way real keystrokes arrive.
fn press(rl: &mut Reedline, k: KeyEvent) {
    let _ = drive(rl, &[k]);
}

/// Like [`press`], but returns the batch flow so a chain can assert
/// whether the line submitted.
fn press_flow(rl: &mut Reedline, k: KeyEvent) -> ControlFlow<Signal> {
    drive(rl, &[k])
}

/// Type a string one char per batch.
fn type_str(rl: &mut Reedline, s: &str) {
    for c in s.chars() {
        press(rl, ch(c));
    }
}

fn paste(rl: &mut Reedline, text: &str) {
    let _ = paste_flow(rl, text);
}

fn paste_flow(rl: &mut Reedline, text: &str) -> ControlFlow<Signal> {
    let prompt = DefaultPrompt::default();
    rl.process_input_batch(&prompt, vec![Event::Paste(text.into())])
        .expect("paste batch ok")
}

fn seed_history(rl: &mut Reedline, entries: &[&str]) {
    for entry in entries {
        rl.history
            .save(HistoryItem::from_command_line(*entry))
            .expect("failed to save history");
    }
}

fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn alt_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
}

// ---------------------------------------------------------------------
// State probes
// ---------------------------------------------------------------------

fn buf(rl: &Reedline) -> &str {
    rl.editor.get_buffer()
}

/// Byte offset of the insertion point.
fn pos(rl: &Reedline) -> usize {
    rl.editor.insertion_point()
}

/// Grapheme index of the insertion point (number of whole grapheme
/// clusters before it). Only meaningful when the point sits on a
/// grapheme boundary — which the wide-char chains assert separately.
fn gpos(rl: &Reedline) -> usize {
    let b = buf(rl);
    let p = pos(rl);
    UnicodeSegmentation::grapheme_indices(b, true)
        .take_while(|(start, _)| *start < p)
        .count()
}

/// Display column of the point within its buffer line (segment after
/// the last `\n`), in terminal cells.
fn col(rl: &Reedline) -> usize {
    let b = buf(rl);
    let p = pos(rl);
    let seg_start = b[..p].rfind('\n').map(|i| i + 1).unwrap_or(0);
    UnicodeWidthStr::width(&b[seg_start..p])
}

/// (current kill-ring slot content, live slot count)
fn ring(rl: &Reedline) -> (String, usize) {
    rl.editor.current_kill()
}

/// The line the incremental history search is currently displaying —
/// the match the user sees while searching. During a search the
/// editor buffer itself keeps the pre-search line untouched (that is
/// what makes abort exact), so the match lives on the history cursor.
fn search_line(rl: &Reedline) -> Option<String> {
    rl.history_cursor.string_at_cursor()
}

/// Assert buffer + byte point in one breath, with the step named so a
/// failure says *which* link of the chain broke.
fn expect(rl: &Reedline, step: &str, want_buf: &str, want_pos: usize) {
    assert_eq!(buf(rl), want_buf, "step `{step}`: buffer");
    assert_eq!(pos(rl), want_pos, "step `{step}`: insertion point");
}

fn expect_ring(rl: &Reedline, step: &str, want_slot: &str, want_len: usize) {
    assert_eq!(
        ring(rl),
        (want_slot.to_string(), want_len),
        "step `{step}`: kill ring (current slot, live slots)"
    );
}

/// Send one standalone [`ReedlineEvent`] through the engine's event
/// entry point, for events no default keybinding produces (a host may
/// still send them — e.g. the standalone `ToStart`/`ToEnd` aliases).
fn send_event(rl: &mut Reedline, event: ReedlineEvent) -> EventStatus {
    let prompt = DefaultPrompt::default();
    rl.handle_event(&prompt, event).expect("event ok")
}

// ---------------------------------------------------------------------
// A. Basic editing chains
// ---------------------------------------------------------------------

#[test]
fn chain_basic_insert_move_delete() {
    let mut rl = new_rl();

    type_str(&mut rl, "helo");
    expect(&rl, "type helo", "helo", 4);

    // Insert the missing letter in the middle.
    press(&mut rl, key(KeyCode::Left));
    expect(&rl, "left", "helo", 3);
    press(&mut rl, ch('l'));
    expect(&rl, "insert l", "hello", 4);

    // C-a / C-e are absolute for a single-line buffer.
    press(&mut rl, ctrl('a'));
    expect(&rl, "C-a", "hello", 0);
    press(&mut rl, ctrl('e'));
    expect(&rl, "C-e", "hello", 5);

    // C-b / C-f step one grapheme and stop at the edges.
    press(&mut rl, ctrl('b'));
    expect(&rl, "C-b", "hello", 4);
    press(&mut rl, ctrl('b'));
    expect(&rl, "C-b again", "hello", 3);
    press(&mut rl, ctrl('f'));
    expect(&rl, "C-f", "hello", 4);
    press(&mut rl, ctrl('a'));
    press(&mut rl, ctrl('b'));
    expect(&rl, "C-b at start is a no-op", "hello", 0);
    press(&mut rl, ctrl('e'));
    press(&mut rl, ctrl('f'));
    expect(&rl, "C-f at end is a no-op", "hello", 5);

    // Backspace deletes before the point; Delete deletes at the point.
    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "backspace", "hell", 4);
    press(&mut rl, ctrl('a'));
    press(&mut rl, key(KeyCode::Delete));
    expect(&rl, "delete at start", "ell", 0);
    press(&mut rl, key(KeyCode::Delete));
    expect(&rl, "delete again", "ll", 0);

    // Home/End keys agree with C-a/C-e.
    press(&mut rl, key(KeyCode::End));
    expect(&rl, "End", "ll", 2);
    press(&mut rl, key(KeyCode::Home));
    expect(&rl, "Home", "ll", 0);
}

#[test]
fn chain_word_motion_steps() {
    let mut rl = new_rl();
    type_str(&mut rl, "foo bar  baz");
    expect(&rl, "typed", "foo bar  baz", 12);

    // M-b walks word starts: baz -> bar -> foo, then sticks.
    press(&mut rl, alt('b'));
    expect(&rl, "M-b to baz", "foo bar  baz", 9);
    press(&mut rl, alt('b'));
    expect(&rl, "M-b to bar", "foo bar  baz", 4);
    press(&mut rl, alt('b'));
    expect(&rl, "M-b to foo", "foo bar  baz", 0);
    press(&mut rl, alt('b'));
    expect(&rl, "M-b sticks at start", "foo bar  baz", 0);

    // M-f walks word ends: foo -> bar -> baz, then sticks.
    press(&mut rl, alt('f'));
    expect(&rl, "M-f to end of foo", "foo bar  baz", 3);
    press(&mut rl, alt('f'));
    expect(&rl, "M-f to end of bar", "foo bar  baz", 7);
    press(&mut rl, alt('f'));
    expect(&rl, "M-f to end of baz", "foo bar  baz", 12);
    press(&mut rl, alt('f'));
    expect(&rl, "M-f sticks at end", "foo bar  baz", 12);
}

// ---------------------------------------------------------------------
// B. Kill / yank chains (GNU keymap)
// ---------------------------------------------------------------------

#[test]
fn chain_kill_line_and_yank_back() {
    let mut rl = new_rl();
    type_str(&mut rl, "alpha beta gamma");
    expect(&rl, "typed", "alpha beta gamma", 16);

    // Mid-line C-k kills to the end of the line only.
    press(&mut rl, ctrl('a'));
    for _ in 0..6 {
        press(&mut rl, ctrl('f'));
    }
    expect(&rl, "point after 'alpha '", "alpha beta gamma", 6);
    press(&mut rl, ctrl('k'));
    expect(&rl, "C-k mid-line", "alpha ", 6);
    expect_ring(&rl, "C-k mid-line", "beta gamma", 1);

    // C-y puts it back at the point.
    press(&mut rl, ctrl('y'));
    expect(&rl, "C-y restores", "alpha beta gamma", 16);

    // C-a C-k kills the whole line into a fresh slot (the yank in
    // between broke the kill chain, so no merge with the first slot).
    press(&mut rl, ctrl('a'));
    press(&mut rl, ctrl('k'));
    expect(&rl, "C-k whole line", "", 0);
    expect_ring(&rl, "C-k whole line", "alpha beta gamma", 2);

    press(&mut rl, ctrl('y'));
    expect(&rl, "yank whole line", "alpha beta gamma", 16);
}

#[test]
fn chain_unix_line_discard_and_yank() {
    let mut rl = new_rl();
    type_str(&mut rl, "hello world");
    press(&mut rl, ctrl('a'));
    for _ in 0..5 {
        press(&mut rl, ctrl('f'));
    }
    expect(&rl, "point after hello", "hello world", 5);

    // GNU rl_unix_line_discard (kill.c:406): kill from point back to
    // the start of the line; the suffix after the point survives.
    press(&mut rl, ctrl('u'));
    expect(&rl, "C-u", " world", 0);
    expect_ring(&rl, "C-u", "hello", 1);

    press(&mut rl, ctrl('y'));
    expect(&rl, "C-y restores prefix", "hello world", 5);
}

#[test]
fn chain_unix_word_rubout_merges_backward_kills() {
    let mut rl = new_rl();
    type_str(&mut rl, "foo bar baz");
    expect(&rl, "typed", "foo bar baz", 11);

    // GNU rl_unix_word_rubout (kill.c:307): skip whitespace back, then
    // kill the non-whitespace run (plus the whitespace skipped over).
    press(&mut rl, ctrl('w'));
    expect(&rl, "C-w #1", "foo bar ", 8);
    expect_ring(&rl, "C-w #1", "baz", 1);

    // Consecutive kills merge into one slot; backward kills prepend,
    // so the slot rebuilds the original order.
    press(&mut rl, ctrl('w'));
    expect(&rl, "C-w #2", "foo ", 4);
    expect_ring(&rl, "C-w #2 merged", "bar baz", 1);

    press(&mut rl, ctrl('w'));
    expect(&rl, "C-w #3", "", 0);
    expect_ring(&rl, "C-w #3 merged", "foo bar baz", 1);

    // One yank restores the whole phrase.
    press(&mut rl, ctrl('y'));
    expect(&rl, "C-y", "foo bar baz", 11);

    // C-w at the start of the line is a no-op (GNU dings).
    press(&mut rl, ctrl('a'));
    press(&mut rl, ctrl('w'));
    expect(&rl, "C-w at start no-op", "foo bar baz", 0);
    expect_ring(&rl, "C-w at start no-op", "foo bar baz", 1);
}

#[test]
fn chain_forward_kills_append_and_yank_pop_cycles() {
    let mut rl = new_rl();
    type_str(&mut rl, "one two three");
    press(&mut rl, ctrl('a'));
    expect(&rl, "at start", "one two three", 0);

    // M-d kills the word ahead; consecutive forward kills append.
    press(&mut rl, alt('d'));
    expect(&rl, "M-d #1", " two three", 0);
    expect_ring(&rl, "M-d #1", "one", 1);
    press(&mut rl, alt('d'));
    expect_ring(&rl, "M-d #2 appended", "one two", 1);

    // Break the kill chain with an insertion: the next kill is a
    // separate slot.
    press(&mut rl, ch('X'));
    expect(&rl, "insert X", "X three", 1);
    press(&mut rl, ctrl('a'));
    press(&mut rl, ctrl('k'));
    expect(&rl, "C-k fresh slot", "", 0);
    expect_ring(&rl, "C-k fresh slot", "X three", 2);

    // A third, separate slot — with only two slots a yank-pop cycle
    // cannot distinguish walking backward from walking forward, so
    // the ring must hold three before the pop sequence below means
    // anything about direction.
    type_str(&mut rl, "solo");
    press(&mut rl, ctrl('u'));
    expect(&rl, "C-u third slot", "", 0);
    expect_ring(&rl, "C-u third slot", "solo", 3);

    // Yank takes the newest slot; M-y walks back through the ring
    // and wraps around to the newest again.
    press(&mut rl, ctrl('y'));
    expect(&rl, "C-y newest", "solo", 4);
    press(&mut rl, alt('y'));
    expect(&rl, "M-y older #1", "X three", 7);
    press(&mut rl, alt('y'));
    expect(&rl, "M-y older #2", "one two", 7);
    press(&mut rl, alt('y'));
    expect(&rl, "M-y wraps", "solo", 4);

    // M-y without a preceding yank is a no-op.
    press(&mut rl, ch('!'));
    expect(&rl, "edit after yank", "solo!", 5);
    press(&mut rl, alt('y'));
    expect(&rl, "M-y after edit no-op", "solo!", 5);
}

#[test]
fn chain_kill_ring_cap_evicts_oldest() {
    let mut rl = new_rl();

    // Eleven distinct kills (typing between them breaks the merge
    // chain): GNU's ring keeps 10 slots and evicts the oldest.
    for i in 0..11 {
        type_str(&mut rl, &format!("w{i}"));
        press(&mut rl, ctrl('u'));
        assert_eq!(buf(&rl), "", "kill {i} empties the line");
    }
    expect_ring(&rl, "after 11 kills", "w10", 10);

    press(&mut rl, ctrl('y'));
    expect(&rl, "yank newest", "w10", 3);
    // Nine yank-pops reach the oldest surviving slot: w1 (w0 evicted).
    for i in (1..=9).rev() {
        press(&mut rl, alt('y'));
        assert_eq!(buf(&rl), format!("w{i}"), "yank-pop down to w{i}");
    }
    // One more wraps back to the newest.
    press(&mut rl, alt('y'));
    expect(&rl, "yank-pop wraps", "w10", 3);
}

// ---------------------------------------------------------------------
// C. Undo / redo chains
// ---------------------------------------------------------------------

#[test]
fn chain_undo_walks_back_step_by_step() {
    let mut rl = new_rl();

    // Three discrete edits: paste, kill, yank.
    paste(&mut rl, "hello world");
    expect(&rl, "paste", "hello world", 11);
    press(&mut rl, ctrl('a'));
    press(&mut rl, alt('d'));
    expect(&rl, "M-d kills hello", " world", 0);
    press(&mut rl, ctrl('y'));
    expect(&rl, "C-y yanks it back", "hello world", 5);

    // Each C-_ peels exactly one edit, newest first, restoring the
    // point as it was before the undone command (0, after the C-a).
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo yank", " world", 0);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo kill", "hello world", 0);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo paste", "", 0);

    // Undo at the bottom of the stack is a no-op, not a panic.
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo exhausted", "", 0);
}

#[test]
fn chain_redo_and_new_edit_invalidation() {
    let mut rl = new_rl();

    paste(&mut rl, "one ");
    paste(&mut rl, "two");
    expect(&rl, "two pastes", "one two", 7);

    press(&mut rl, ctrl('_'));
    expect(&rl, "undo paste two", "one ", 4);

    // Redo reapplies the undone edit.
    rl.run_edit_commands(&[EditCommand::Redo]);
    expect(&rl, "redo paste two", "one two", 7);

    // Undo, then a *new* edit: the redo stack must be invalidated,
    // so redoing now must not resurrect "two".
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo again", "one ", 4);
    press(&mut rl, ch('X'));
    expect(&rl, "new edit", "one X", 5);
    rl.run_edit_commands(&[EditCommand::Redo]);
    expect(&rl, "redo after new edit is a no-op", "one X", 5);

    // The undo chain still walks back through the new edit first.
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo X", "one ", 4);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo paste one", "", 0);
}

#[test]
fn chain_revert_line_clears_fresh_line() {
    let mut rl = new_rl();

    // On a fresh line, revert-line (M-r) undoes every change made to
    // the line — back to the empty line editing started from.
    type_str(&mut rl, "scratch");
    expect(&rl, "fresh line typed", "scratch", 7);
    press(&mut rl, alt('r'));
    expect(&rl, "M-r clears fresh line", "", 0);

    // The line is editable again afterwards, and the new edits form
    // a fresh undo history of their own.
    type_str(&mut rl, "again");
    expect(&rl, "retyped", "again", 5);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo retyped", "", 0);
}

/// GNU revert-line undoes the changes made to *this* line, and a
/// recalled history line starts a fresh undo boundary: fetching an
/// entry installs that entry's own undo list (misc.c
/// `rl_replace_from_history` — `rl_undo_list = entry->data`, empty
/// for an entry not edited this session), so undo and revert-line
/// can never cross the recall into the line that was live before
/// traversal. Recall `git status`, edit it, and M-r must give back
/// `git status` (issue #53).
#[test]
fn chain_revert_line_restores_recalled_history() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["git status"]);

    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall history", "git status", 10);

    type_str(&mut rl, " -s");
    press(&mut rl, ctrl('a'));
    type_str(&mut rl, "x");
    expect(&rl, "edited recall", "xgit status -s", 1);

    // GNU revert-line (undo.c:331), M-r: every change to the recalled
    // line is undone in one step.
    press(&mut rl, alt('r'));
    expect(&rl, "M-r reverts", "git status", 10);
}

#[test]
fn chain_undo_and_revert_on_untouched_recall_are_noops() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["git status"]);

    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall history", "git status", 10);

    // The recalled line carries an empty undo list (misc.c
    // `rl_replace_from_history`): C-_ right after the recall has
    // nothing to undo and must not touch the line — the recall
    // itself is not an undoable edit.
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo on untouched recall is a no-op", "git status", 10);

    // Same for revert-line on the untouched recall.
    press(&mut rl, alt('r'));
    expect(&rl, "M-r on untouched recall is a no-op", "git status", 10);
}

#[test]
fn chain_undo_stops_at_recall_boundary() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["git status"]);

    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall history", "git status", 10);

    type_str(&mut rl, " -s");
    press(&mut rl, ctrl('a'));
    type_str(&mut rl, "x");
    expect(&rl, "edited recall", "xgit status -s", 1);

    // Undo walks the edits made *since* the recall, newest first,
    // and stops at the recalled text: the boundary is the floor.
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo x", "git status -s", 0);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo suffix", "git status", 10);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo at boundary is a no-op", "git status", 10);
}

#[test]
fn chain_revert_line_stops_at_each_recall_hop() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["git status", "git diff"]);

    // Each traversal hop lands on an entry with its own undo
    // boundary: editing the older entry and reverting gives back
    // that entry, not the newer one visited in between.
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall newest", "git diff", 8);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall older", "git status", 10);

    type_str(&mut rl, " -s");
    expect(&rl, "edited older entry", "git status -s", 13);
    press(&mut rl, alt('r'));
    expect(&rl, "M-r reverts to this hop's entry", "git status", 10);
}

#[test]
fn chain_undo_stops_at_recall_boundary_after_typed_prefix() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["git status"]);

    // Type on the live line first, so the pre-recall undo stack is
    // non-empty when the recall lands (the chains above all recall
    // from an empty line, where the boundary clear is unobservable).
    // With a non-empty buffer and the cursor at its end, Up runs a
    // prefix search on the typed text, so the typed text is a prefix
    // of the seeded entry; the landing still routes through the same
    // history-navigation throat.
    type_str(&mut rl, "git");
    expect(&rl, "typed live line", "git", 3);

    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall history", "git status", 10);

    // Edit the recalled line, then undo repeatedly: undo stops at
    // the recalled text and never crosses into the pre-recall "git".
    type_str(&mut rl, "!");
    expect(&rl, "edited recall", "git status!", 11);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo edit", "git status", 10);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo at floor is a no-op", "git status", 10);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo still at floor", "git status", 10);

    // Revert-line likewise stops at the floor.
    press(&mut rl, alt('r'));
    expect(&rl, "M-r at floor is a no-op", "git status", 10);
}

// ---------------------------------------------------------------------
// D. Reverse incremental search (C-r) chains
// ---------------------------------------------------------------------

#[test]
fn chain_isearch_narrows_per_keystroke() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["alpha one", "beta two", "alpha three"]);

    press(&mut rl, ctrl('r'));
    assert_eq!(rl.input_mode, InputMode::HistorySearch, "C-r enters search");

    // Each typed char re-searches: the displayed match is the newest
    // entry matching the needle so far, step by step — while the
    // editor buffer keeps the pre-search (empty) line untouched.
    press(&mut rl, ch('a'));
    assert_eq!(search_line(&rl), Some("alpha three".into()), "needle 'a'");
    expect(&rl, "buffer untouched by search", "", 0);
    assert_eq!(rl.input_mode, InputMode::HistorySearch);
    press(&mut rl, ch('l'));
    assert_eq!(search_line(&rl), Some("alpha three".into()), "needle 'al'");
    press(&mut rl, ch('p'));
    assert_eq!(search_line(&rl), Some("alpha three".into()), "needle 'alp'");
    press(&mut rl, ch('h'));
    assert_eq!(
        search_line(&rl),
        Some("alpha three".into()),
        "needle after four chars"
    );
    press(&mut rl, ch('a'));
    assert_eq!(
        search_line(&rl),
        Some("alpha three".into()),
        "needle 'alpha'"
    );

    // C-r again walks to the next older match; a third C-r finds no
    // older match and stays put (still in search mode).
    press(&mut rl, ctrl('r'));
    assert_eq!(
        search_line(&rl),
        Some("alpha one".into()),
        "C-r to older match"
    );
    press(&mut rl, ctrl('r'));
    assert_eq!(
        search_line(&rl),
        Some("alpha one".into()),
        "C-r exhausted stays"
    );
    assert_eq!(rl.input_mode, InputMode::HistorySearch);

    // Accepting lands on the found entry: point at the match start
    // (GNU _rl_isearch_fini), mode back to regular.
    press(&mut rl, key(KeyCode::Enter));
    assert_eq!(rl.input_mode, InputMode::Regular, "Enter accepts search");
    expect(&rl, "accepted line", "alpha one", 0);
}

/// Regression pin for ruline#52 D4: after an isearch is accepted,
/// the history cursor must stay positioned *on* the found entry —
/// GNU's `_rl_isearch_fini` moves the history position onto
/// `last_found_line` (isearch.c:288-291) — so Up/Down step to its
/// neighbours. (Seed history: PR #51 recorded the desync this pins
/// against — Up from the accepted middle entry jumped to a NEWER
/// entry, and accepting the newest entry wedged navigation.)
#[test]
fn chain_isearch_accept_positions_history_cursor() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["alpha one", "beta two", "alpha three"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "beta");
    press(&mut rl, key(KeyCode::Enter));
    expect(&rl, "accepted middle entry", "beta two", 0);

    // Plain adjacent stepping from the found entry, point at the
    // end of each recalled line.
    press(&mut rl, key(KeyCode::Down));
    expect(&rl, "Down to newer neighbour", "alpha three", 11);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "Up back to found entry", "beta two", 8);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "Up to older neighbour", "alpha one", 9);
    press(&mut rl, key(KeyCode::Down));
    expect(&rl, "Down again", "beta two", 8);
}

#[test]
fn chain_isearch_abort_restores_original_line() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["beta two", "gamma three"]);

    type_str(&mut rl, "draft");
    expect(&rl, "typed draft", "draft", 5);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "beta");
    assert_eq!(search_line(&rl), Some("beta two".into()), "searching beta");
    expect(&rl, "original line preserved", "draft", 5);
    assert_eq!(rl.input_mode, InputMode::HistorySearch);

    // C-g aborts: the line being edited before the search comes back
    // verbatim, point included.
    press(&mut rl, ctrl('g'));
    assert_eq!(rl.input_mode, InputMode::Regular, "C-g leaves search");
    expect(&rl, "abort restores draft", "draft", 5);
}

/// Regression pin for ruline#52 D5: GNU's dispatch finalizes the
/// search first — found line in the buffer, point at the match
/// start — and *then* executes the pending command on it
/// (isearch.c:513-524). (Seed history: PR #51 recorded the engine's
/// fallback `run_history_commands` running the command against the
/// pre-search buffer instead, silently discarding its effect —
/// C-e left the point at 0 instead of the line end.)
#[test]
fn chain_isearch_command_key_exits_and_executes() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["beta two", "gamma three"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "gamma");
    assert_eq!(
        search_line(&rl),
        Some("gamma three".into()),
        "searching gamma"
    );

    // GNU isearch dispatch (isearch.c:513-524): a base-keymap
    // command key terminates the search — keeping the found line —
    // and then executes as a command on it. C-e lands the point at
    // the match start on finalize, then itself moves to line end.
    press(&mut rl, ctrl('e'));
    assert_eq!(rl.input_mode, InputMode::Regular, "C-e leaves search");
    expect(&rl, "found line kept, C-e executed", "gamma three", 11);

    // And it is an ordinary line now: editing continues on it.
    type_str(&mut rl, " --verbose");
    expect(&rl, "edit after exit", "gamma three --verbose", 21);
}

/// Regression pin for ruline#52 D2: in GNU, Esc is a default
/// member of `isearch-terminators` (isearch.c:529-532,
/// "\033\012") — it terminates the search *keeping the found line*;
/// only C-g (rl_abort) restores the pre-search line. (Seed history:
/// PR #51 recorded Esc throwing the found line away exactly like
/// C-g, because the keymap layer mapped both keys to the same
/// event.)
///
/// Point expectation corrected against the GNU oracle by the #52
/// lane: the seed originally expected point 11 (line end) after
/// Esc, but `_rl_isearch_fini` sets `rl_point = cxt->sline_index`
/// (isearch.c:302) — the match start — and a PTY probe against GNU
/// bash 5.3.20 confirms it (C-r `gamma` Esc, then typing `X`,
/// yields `Xgamma three`: the marker lands at offset 0). The chain
/// therefore expects the match start after Esc and presses C-e
/// before typing on, preserving the seed's "the line stays
/// editable" intent at the GNU-true point.
#[test]
fn chain_isearch_escape_keeps_found_line() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["beta two", "gamma three"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "gamma");
    assert_eq!(
        search_line(&rl),
        Some("gamma three".into()),
        "searching gamma"
    );

    // Esc terminates the search and keeps the found line for
    // editing, point at the match start (GNU fini, isearch.c:302).
    press(&mut rl, key(KeyCode::Esc));
    assert_eq!(rl.input_mode, InputMode::Regular, "Esc leaves search");
    expect(&rl, "found line kept", "gamma three", 0);

    // And it is an ordinary line now: editing continues on it.
    press(&mut rl, ctrl('e'));
    expect(&rl, "C-e to line end", "gamma three", 11);
    type_str(&mut rl, " --verbose");
    expect(&rl, "edit after Esc", "gamma three --verbose", 21);
}

#[test]
fn chain_isearch_failed_needle_recovers_on_backspace() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["alpha one", "beta two"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "beta");
    assert_eq!(search_line(&rl), Some("beta two".into()), "needle beta");

    // Extending the needle past every match fails the search but
    // does not leave it: the mode and the needle survive.
    press(&mut rl, ch('x'));
    assert_eq!(rl.input_mode, InputMode::HistorySearch, "still searching");
    assert_eq!(
        rl.history_cursor.get_navigation(),
        HistoryNavigationQuery::SubstringSearch("betax".into()),
        "needle records the failed extension"
    );
    press(&mut rl, ch('y'));
    assert_eq!(rl.input_mode, InputMode::HistorySearch, "still searching");

    // Deleting needle chars re-searches and recovers the match.
    press(&mut rl, key(KeyCode::Backspace));
    press(&mut rl, key(KeyCode::Backspace));
    assert_eq!(
        search_line(&rl),
        Some("beta two".into()),
        "needle back to beta"
    );

    press(&mut rl, ctrl('g'));
    expect(&rl, "abort to empty line", "", 0);
    assert_eq!(rl.input_mode, InputMode::Regular);
}

/// Regression pin for ruline#52 D3: when an isearch needle stops
/// matching, GNU keeps the last successful match on the line and
/// only flags the prompt as failed (isearch.c:863-870: on failure
/// `history_pos` returns to `last_found_line`, whose line is what
/// `rl_display_search` shows — PTY-verified against GNU bash
/// 5.3.20: `(failed reverse-i-search)`betax': beta two`).
/// (Seed history: PR #51 recorded the engine dropping the match —
/// the history cursor lost its entry and the painter rendered an
/// empty line.)
#[test]
fn chain_isearch_failed_needle_keeps_last_match() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["alpha one", "beta two"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "beta");
    assert_eq!(search_line(&rl), Some("beta two".into()), "needle beta");

    press(&mut rl, ch('x'));
    assert_eq!(
        search_line(&rl),
        Some("beta two".into()),
        "needle betax failed keeps last match"
    );
    assert_eq!(rl.input_mode, InputMode::HistorySearch);
    press(&mut rl, ch('y'));
    assert_eq!(
        search_line(&rl),
        Some("beta two".into()),
        "needle betaxy failed keeps last match"
    );
}

/// Regression pin for ruline#60: a standalone movement event in
/// isearch is a base-keymap command that is not `rl_insert`, so GNU's
/// dispatch stashes it as the pending command, finalizes the search
/// (found line in the buffer, point at the match start), and then
/// executes it (isearch.c:513-524). The Right arrow arrives as
/// `UntilFound([HistoryHintComplete, MenuRight, Right])`; the first
/// two are inapplicable in search mode, so the chain must resolve at
/// `Right` — on the base tree every candidate is Inapplicable and the
/// search swallows the key.
#[test]
fn chain_issue60_isearch_right_arrow_exits_and_moves() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["beta two", "gamma three"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "gamma");
    assert_eq!(
        search_line(&rl),
        Some("gamma three".into()),
        "searching gamma"
    );
    assert_eq!(rl.input_mode, InputMode::HistorySearch);

    // Fini puts the point at the match start (0); Right then moves
    // one grapheme into the found line.
    press(&mut rl, key(KeyCode::Right));
    assert_eq!(rl.input_mode, InputMode::Regular, "Right leaves search");
    expect(&rl, "found line kept, Right executed", "gamma three", 1);

    // And it is an ordinary line now: editing continues on it.
    type_str(&mut rl, "X");
    expect(&rl, "edit after exit", "gXamma three", 2);
}

/// Regression pin for ruline#60: `C-f` spells the same
/// `UntilFound([HistoryHintComplete, MenuRight, Right])` composite as
/// the Right arrow, so in isearch it too must finalize the search and
/// then move — on the base tree its net effect is "accept the search
/// without moving" at best, and here it does not even leave the mode.
#[test]
fn chain_issue60_isearch_ctrl_f_exits_and_moves() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["beta two", "gamma three"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "gamma");
    assert_eq!(
        search_line(&rl),
        Some("gamma three".into()),
        "searching gamma"
    );

    press(&mut rl, ctrl('f'));
    assert_eq!(rl.input_mode, InputMode::Regular, "C-f leaves search");
    expect(&rl, "found line kept, C-f executed", "gamma three", 1);

    type_str(&mut rl, "X");
    expect(&rl, "edit after exit", "gXamma three", 2);
}

/// Regression pin for ruline#60: `Left` after a mid-line match —
/// fini leaves the point at the match start, and the pending `Left`
/// moves one grapheme further into the found line (isearch.c:513-524).
#[test]
fn chain_issue60_isearch_left_exits_and_moves() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["xx beta yy"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "beta");
    assert_eq!(
        search_line(&rl),
        Some("xx beta yy".into()),
        "searching beta"
    );

    // Fini point is the match start (3); Left moves to 2.
    press(&mut rl, key(KeyCode::Left));
    assert_eq!(rl.input_mode, InputMode::Regular, "Left leaves search");
    expect(&rl, "found line kept, Left executed", "xx beta yy", 2);

    type_str(&mut rl, "Q");
    expect(&rl, "edit after exit", "xxQ beta yy", 3);
}

/// Regression pin for ruline#60: the search finalizes even when the
/// pending movement cannot move the point — GNU terminates the search
/// first (the fini is unconditional, isearch.c:513-524); the command
/// then dings in place. `C-b` at the match start must still leave the
/// mode with the found line in the buffer.
#[test]
fn chain_issue60_isearch_left_at_match_start_still_exits() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["beta two"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "beta");
    assert_eq!(search_line(&rl), Some("beta two".into()), "searching beta");

    press(&mut rl, ctrl('b'));
    assert_eq!(rl.input_mode, InputMode::Regular, "C-b leaves search");
    expect(&rl, "found line kept, point unmoved", "beta two", 0);

    // Ordinary editing resumes from the finalized state.
    press(&mut rl, ctrl('e'));
    expect(&rl, "C-e to line end", "beta two", 8);
    type_str(&mut rl, "!");
    expect(&rl, "edit after exit", "beta two!", 9);
}

/// Regression pin for ruline#60: the standalone `ToEnd` alias event
/// (no default Emacs keybinding produces it, but hosts send it) is
/// the same movement class — fini, then execute (isearch.c:513-524).
#[test]
fn chain_issue60_isearch_to_end_exits_and_moves() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["beta two", "gamma three"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "gamma");
    assert_eq!(
        search_line(&rl),
        Some("gamma three".into()),
        "searching gamma"
    );

    let status = send_event(&mut rl, ReedlineEvent::ToEnd);
    assert!(matches!(status, EventStatus::Handled), "ToEnd handled");
    assert_eq!(rl.input_mode, InputMode::Regular, "ToEnd leaves search");
    expect(&rl, "found line kept, ToEnd executed", "gamma three", 11);

    type_str(&mut rl, " --verbose");
    expect(&rl, "edit after exit", "gamma three --verbose", 21);
}

/// Regression pin for ruline#60: the standalone `ToStart` alias with
/// a mid-line match — fini leaves the point at the match start (3),
/// and the pending `ToStart` takes it to the line start.
#[test]
fn chain_issue60_isearch_to_start_exits_and_moves() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["xx gamma yy"]);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "gamma");
    assert_eq!(
        search_line(&rl),
        Some("xx gamma yy".into()),
        "searching gamma"
    );

    let status = send_event(&mut rl, ReedlineEvent::ToStart);
    assert!(matches!(status, EventStatus::Handled), "ToStart handled");
    assert_eq!(rl.input_mode, InputMode::Regular, "ToStart leaves search");
    expect(&rl, "found line kept, ToStart executed", "xx gamma yy", 0);

    type_str(&mut rl, "Q");
    expect(&rl, "edit after exit", "Qxx gamma yy", 1);
}

/// Regression pin for ruline#60: a movement key after a search that
/// never landed on any entry still terminates the mode — the fini is
/// a no-op (nothing was found), the pre-search line is what remains,
/// and the movement executes on it (here pinned at the line end,
/// where it cannot move). On the base tree the key is swallowed and
/// the engine stays in search mode over a failed needle.
#[test]
fn chain_issue60_isearch_movement_after_failed_search_keeps_line() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["beta two", "gamma three"]);

    type_str(&mut rl, "draft");
    expect(&rl, "typed draft", "draft", 5);

    press(&mut rl, ctrl('r'));
    type_str(&mut rl, "zzz");
    assert_eq!(rl.input_mode, InputMode::HistorySearch, "still searching");
    assert_eq!(search_line(&rl), None, "needle never matched");

    press(&mut rl, key(KeyCode::Right));
    assert_eq!(rl.input_mode, InputMode::Regular, "Right leaves search");
    expect(&rl, "pre-search line kept", "draft", 5);

    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "edit after exit", "draf", 4);
}

/// Regression pin for ruline#60 (noninc side): GNU's non-incremental
/// search has no fini-then-execute path — `_rl_nsearch_dispatch`
/// (search.c:308) reads raw characters and its default case inserts
/// them into the search string; the buffer being edited *is* the
/// search string. In reedline's event model the standalone movement
/// events are the aliases of the same movements the `Edit` arm
/// already applies to that string, so they move the point *within
/// the search string* and the mode stays `NonIncrementalSearch` —
/// the class treatment parallel to isearch, on this mode's own
/// mechanism. On the base tree all four fall to `Inapplicable`.
#[test]
fn chain_issue60_noninc_movement_stays_in_search_string() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["alpha one", "beta two", "gamma three"]);

    // M-p opens the backward non-incremental search (search.c:462).
    press(&mut rl, alt('p'));
    assert_eq!(rl.input_mode, InputMode::NonIncrementalSearch);
    expect(&rl, "search string starts empty", "", 0);

    type_str(&mut rl, "beta");
    expect(&rl, "search string typed", "beta", 4);

    press(&mut rl, key(KeyCode::Left));
    assert_eq!(
        rl.input_mode,
        InputMode::NonIncrementalSearch,
        "Left stays in the search"
    );
    expect(&rl, "Left inside the string", "beta", 3);

    press(&mut rl, key(KeyCode::Right));
    assert_eq!(rl.input_mode, InputMode::NonIncrementalSearch);
    expect(&rl, "Right inside the string", "beta", 4);

    let status = send_event(&mut rl, ReedlineEvent::ToStart);
    assert!(matches!(status, EventStatus::Handled), "ToStart handled");
    assert_eq!(rl.input_mode, InputMode::NonIncrementalSearch);
    expect(&rl, "ToStart inside the string", "beta", 0);

    let status = send_event(&mut rl, ReedlineEvent::ToEnd);
    assert!(matches!(status, EventStatus::Handled), "ToEnd handled");
    assert_eq!(rl.input_mode, InputMode::NonIncrementalSearch);
    expect(&rl, "ToEnd inside the string", "beta", 4);

    // The chain still commits as a search for the string as edited:
    // Enter runs the one backward search and lands on the match,
    // point at 0 (`rl_point = 0`, search.c:217-219).
    press(&mut rl, key(KeyCode::Enter));
    assert_eq!(rl.input_mode, InputMode::Regular, "Enter commits search");
    expect(&rl, "search landed", "beta two", 0);
}

// ---------------------------------------------------------------------
// E. Multi-line chains
// ---------------------------------------------------------------------

/// Engine with the crate's `DefaultValidator` wired the way a host
/// wires it: an unbalanced quote or bracket makes Enter continue the
/// line instead of submitting it.
fn new_rl_validated() -> Reedline {
    let mut rl = Reedline::create()
        .with_edit_mode(Box::<Emacs>::default())
        .with_validator(Box::new(crate::DefaultValidator));
    rl.painter.force_prompt_anchored_for_test(0);
    rl
}

#[test]
fn chain_continuation_via_unclosed_quote() {
    let mut rl = new_rl_validated();

    type_str(&mut rl, "echo \"abc");
    expect(&rl, "typed open quote", "echo \"abc", 9);

    // Enter on an incomplete line inserts a newline; nothing submits.
    let flow = press_flow(&mut rl, key(KeyCode::Enter));
    assert!(
        matches!(flow, ControlFlow::Continue(())),
        "unclosed quote must continue, not submit"
    );
    expect(&rl, "continuation line", "echo \"abc\n", 10);

    type_str(&mut rl, "def\"");
    expect(&rl, "closing quote typed", "echo \"abc\ndef\"", 14);

    // Now the line validates complete and Enter submits it whole.
    let flow = press_flow(&mut rl, key(KeyCode::Enter));
    match flow {
        ControlFlow::Break(Signal::Success(line)) => {
            assert_eq!(line, "echo \"abc\ndef\"");
        }
        other => panic!("expected submit of the full two-line buffer, got {other:?}"),
    }
}

#[test]
fn chain_continuation_via_unclosed_bracket() {
    let mut rl = new_rl_validated();

    type_str(&mut rl, "echo (abc");
    let flow = press_flow(&mut rl, key(KeyCode::Enter));
    assert!(matches!(flow, ControlFlow::Continue(())));
    expect(&rl, "bracket continuation", "echo (abc\n", 10);

    type_str(&mut rl, "def)");
    let flow = press_flow(&mut rl, key(KeyCode::Enter));
    match flow {
        ControlFlow::Break(Signal::Success(line)) => {
            assert_eq!(line, "echo (abc\ndef)");
        }
        other => panic!("expected submit after closing bracket, got {other:?}"),
    }
}

/// Build "one\ntwo\nthree" via Alt+Enter line breaks.
fn three_line_rl() -> Reedline {
    let mut rl = new_rl();
    type_str(&mut rl, "one");
    press(&mut rl, alt_key(KeyCode::Enter));
    type_str(&mut rl, "two");
    press(&mut rl, alt_key(KeyCode::Enter));
    type_str(&mut rl, "three");
    expect(&rl, "three lines built", "one\ntwo\nthree", 13);
    rl
}

#[test]
fn chain_multiline_navigation() {
    let mut rl = three_line_rl();
    seed_history(&mut rl, &["old cmd"]);

    // C-a / C-e are line-segment local in a multi-line buffer (GNU
    // rl_beginning_of_line works on the current line of the buffer).
    press(&mut rl, ctrl('a'));
    expect(&rl, "C-a to segment start", "one\ntwo\nthree", 8);
    press(&mut rl, ctrl('e'));
    expect(&rl, "C-e to segment end", "one\ntwo\nthree", 13);

    // Up walks buffer lines first, keeping the goal column where the
    // target line is long enough and clamping where it is not.
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "Up to line 2 (clamped)", "one\ntwo\nthree", 7);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "Up to line 1", "one\ntwo\nthree", 3);

    // Up on the first buffer line leaves the buffer for history.
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "Up into history", "old cmd", 7);

    // Down returns to the live multi-line buffer. The saved line
    // carries no point of its own (GNU's saved line is text-only),
    // so the point lands at the end of the restored buffer.
    press(&mut rl, key(KeyCode::Down));
    expect(&rl, "Down back to live line", "one\ntwo\nthree", 13);
}

#[test]
fn chain_multiline_kills() {
    let mut rl = three_line_rl();

    // GNU rl_unix_line_discard kills from the point to the start of
    // the *buffer* (kill.c:406 calls rl_kill_text(rl_point, 0)), so
    // C-u on line 3 swallows lines 1-2 and the newline before point.
    press(&mut rl, ctrl('a'));
    expect(&rl, "C-a segment start", "one\ntwo\nthree", 8);
    press(&mut rl, ctrl('u'));
    expect(&rl, "C-u across lines", "three", 0);
    expect_ring(&rl, "C-u across lines", "one\ntwo\n", 1);
    press(&mut rl, ctrl('y'));
    expect(&rl, "yank restores all", "one\ntwo\nthree", 8);

    // C-k inside line 1 kills to that segment's end only (GNU
    // rl_kill_line first moves to the end of the current line).
    // From the segment-3 start (goal column 0), Up walks to the
    // start of each segment above.
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "Up to line 2 start", "one\ntwo\nthree", 4);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "Up to line 1 start", "one\ntwo\nthree", 0);
    press(&mut rl, ctrl('f'));
    expect(&rl, "point mid line 1", "one\ntwo\nthree", 1);
    press(&mut rl, ctrl('k'));
    expect(&rl, "C-k segment-local", "o\ntwo\nthree", 1);
    expect_ring(&rl, "C-k segment-local", "ne", 2);

    // Backspace at a segment start joins the lines.
    press(&mut rl, ctrl('e'));
    expect(&rl, "C-e at segment end", "o\ntwo\nthree", 1);
    press(&mut rl, ctrl('f'));
    expect(&rl, "start of line 2", "o\ntwo\nthree", 2);
    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "backspace joins lines", "otwo\nthree", 1);
}

// ---------------------------------------------------------------------
// F. Wide-character chains
// ---------------------------------------------------------------------

#[test]
fn chain_wide_char_insert_move_delete() {
    let mut rl = new_rl();

    press(&mut rl, ch('a'));
    expect(&rl, "type a", "a", 1);
    press(&mut rl, ch('你'));
    expect(&rl, "type CJK", "a你", 4);
    assert_eq!(gpos(&rl), 2, "two graphemes after a+CJK");
    assert_eq!(col(&rl), 3, "a (1 cell) + CJK (2 cells)");
    press(&mut rl, ch('b'));
    expect(&rl, "type b", "a你b", 5);
    assert_eq!(gpos(&rl), 3);

    // C-b steps over the whole 3-byte grapheme, never into its bytes.
    press(&mut rl, ctrl('b'));
    expect(&rl, "C-b over b", "a你b", 4);
    press(&mut rl, ctrl('b'));
    expect(&rl, "C-b over CJK", "a你b", 1);
    assert_eq!(gpos(&rl), 1);
    press(&mut rl, ctrl('f'));
    expect(&rl, "C-f over CJK", "a你b", 4);

    // Backspace removes the CJK char as one unit.
    press(&mut rl, ctrl('e'));
    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "backspace b", "a你", 4);
    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "backspace CJK whole", "a", 1);
    assert_eq!(gpos(&rl), 1);
    assert_eq!(col(&rl), 1);
}

#[test]
fn chain_emoji_and_zwj_cluster_move_as_one() {
    let mut rl = new_rl();

    press(&mut rl, ch('😊'));
    expect(&rl, "type emoji", "😊", 4);
    assert_eq!(gpos(&rl), 1, "emoji is one grapheme");
    assert_eq!(col(&rl), 2, "emoji is two cells wide");
    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "backspace emoji whole", "", 0);

    // A ZWJ sequence is a single grapheme cluster in this editor's
    // grapheme model: 17 bytes, one motion step, one backspace.
    let cluster = "🤦🏼‍♂️";
    assert_eq!(cluster.len(), 17, "fixture: cluster byte length");
    type_str(&mut rl, cluster);
    expect(&rl, "type ZWJ cluster", cluster, 17);
    assert_eq!(gpos(&rl), 1, "ZWJ cluster is one grapheme");
    assert_eq!(col(&rl), 2, "ZWJ cluster renders two cells wide");

    press(&mut rl, ctrl('b'));
    expect(&rl, "C-b over whole cluster", cluster, 0);
    press(&mut rl, ctrl('f'));
    expect(&rl, "C-f over whole cluster", cluster, 17);
    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "backspace removes whole cluster", "", 0);
}

#[test]
fn chain_mixed_script_word_motion() {
    let mut rl = new_rl();
    // "你好 world": CJK letters form a word run just like Latin ones.
    // (3 + 3 + 1 + 5 = 12 bytes.)
    type_str(&mut rl, "你好 world");
    expect(&rl, "typed mixed", "你好 world", 12);

    press(&mut rl, alt('b'));
    expect(&rl, "M-b to world", "你好 world", 7);
    press(&mut rl, alt('b'));
    expect(&rl, "M-b to start", "你好 world", 0);
    press(&mut rl, alt('f'));
    expect(&rl, "M-f over CJK word", "你好 world", 6);
    press(&mut rl, alt('f'));
    expect(&rl, "M-f to end", "你好 world", 12);
}

// ---------------------------------------------------------------------
// G. Bracketed-paste chains
// ---------------------------------------------------------------------

#[test]
fn chain_paste_multiline_does_not_submit() {
    let mut rl = new_rl();

    // Bracketed paste inserts embedded newlines literally: no line
    // may execute on the way in, not even the trailing newline.
    let flow = paste_flow(&mut rl, "echo one\necho two\n");
    assert!(
        matches!(flow, ControlFlow::Continue(())),
        "paste must never submit"
    );
    expect(&rl, "pasted two lines", "echo one\necho two\n", 18);

    // It is an ordinary buffer afterwards.
    type_str(&mut rl, "echo three");
    expect(
        &rl,
        "typing after paste",
        "echo one\necho two\necho three",
        28,
    );

    let flow = press_flow(&mut rl, key(KeyCode::Enter));
    match flow {
        ControlFlow::Break(Signal::Success(line)) => {
            assert_eq!(line, "echo one\necho two\necho three");
        }
        other => panic!("expected one submit of the pasted buffer, got {other:?}"),
    }
}

#[test]
fn chain_paste_is_one_undo_unit() {
    let mut rl = new_rl();

    type_str(&mut rl, "x");
    paste(&mut rl, "line1\nline2");
    expect(&rl, "paste after x", "xline1\nline2", 12);

    // One undo removes the entire paste, however many lines it had.
    press(&mut rl, ctrl('_'));
    expect(&rl, "one undo removes paste", "x", 1);
    press(&mut rl, ctrl('_'));
    expect(&rl, "second undo removes x", "", 0);

    rl.run_edit_commands(&[EditCommand::Redo]);
    expect(&rl, "redo x", "x", 1);
    rl.run_edit_commands(&[EditCommand::Redo]);
    expect(&rl, "redo paste whole", "xline1\nline2", 12);
}

#[test]
fn chain_paste_verbatim_with_auto_pairs() {
    let mut rl = new_rl_with_pairs(&[('(', ')')]);

    type_str(&mut rl, "echo ");
    // Pasted text is never rewritten by auto-pairs: no closing paren
    // is invented for the pasted opener.
    paste(&mut rl, "(a");
    expect(&rl, "pasted opener verbatim", "echo (a", 7);

    // A *typed* closer afterwards is just a character here (the next
    // char is not a paired closer to overtype).
    press(&mut rl, ch(')'));
    expect(&rl, "typed closer", "echo (a)", 8);
}

// ---------------------------------------------------------------------
// H. Editing inside quotes and brackets
// ---------------------------------------------------------------------

#[test]
fn chain_auto_pair_insert_overtype_delete() {
    let mut rl = new_rl_with_pairs(&[('(', ')'), ('"', '"'), ('[', ']')]);

    press(&mut rl, ch('('));
    expect(&rl, "open paren pairs", "()", 1);
    press(&mut rl, ch('x'));
    expect(&rl, "type inside pair", "(x)", 2);
    // Typing the closer overtypes the auto-inserted one.
    press(&mut rl, ch(')'));
    expect(&rl, "overtype closer", "(x)", 3);
    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "backspace closer", "(x", 2);
    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "backspace x", "(", 1);

    // A fresh opener inside the remains of the earlier pair inserts
    // its own pair (the outer closer was already deleted above, so
    // the buffer is the lone "(" plus the new "()").
    press(&mut rl, ch('('));
    expect(&rl, "nested open", "(()", 2);
    // Backspace inside that empty inner pair removes both sides.
    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "backspace empty pair", "(", 1);
    press(&mut rl, key(KeyCode::Backspace));
    expect(&rl, "backspace lone opener", "", 0);

    // Quotes pair the same way, and the closer overtypes.
    press(&mut rl, ch('"'));
    expect(&rl, "open quote pairs", "\"\"", 1);
    press(&mut rl, ch('a'));
    expect(&rl, "type inside quotes", "\"a\"", 2);
    press(&mut rl, ch('"'));
    expect(&rl, "overtype quote", "\"a\"", 3);
}

#[test]
fn chain_edit_inside_quotes() {
    let mut rl = new_rl();

    type_str(&mut rl, "echo \"hello world\"");
    expect(&rl, "typed quoted", "echo \"hello world\"", 18);

    // Word motion inside the quotes treats the quote as a boundary.
    press(&mut rl, ctrl('b'));
    expect(&rl, "step inside closing quote", "echo \"hello world\"", 17);
    press(&mut rl, alt('b'));
    expect(&rl, "M-b to world start", "echo \"hello world\"", 12);
    press(&mut rl, alt('b'));
    expect(&rl, "M-b to hello start", "echo \"hello world\"", 6);

    type_str(&mut rl, "say ");
    expect(&rl, "insert inside quotes", "echo \"say hello world\"", 10);

    // Kill the word I just landed in the middle of: point sits right
    // before "hello"; M-d removes it, leaving the rest of the string.
    press(&mut rl, alt('d'));
    expect(&rl, "M-d inside quotes", "echo \"say  world\"", 10);
    expect_ring(&rl, "M-d inside quotes", "hello", 1);
}

#[test]
fn chain_transpose_chars() {
    let mut rl = new_rl();
    type_str(&mut rl, "ab");
    expect(&rl, "typed ab", "ab", 2);

    // GNU rl_transpose_chars at end of line swaps the last two chars.
    press(&mut rl, ctrl('t'));
    expect(&rl, "C-t at end", "ba", 2);
    press(&mut rl, ctrl('t'));
    expect(&rl, "C-t again swaps back", "ab", 2);

    // Mid-line, C-t swaps the chars around the point and moves past.
    press(&mut rl, ctrl('a'));
    press(&mut rl, ctrl('f'));
    expect(&rl, "point between a and b", "ab", 1);
    press(&mut rl, ctrl('t'));
    expect(&rl, "C-t mid-line", "ba", 2);
}

// ---------------------------------------------------------------------
// I. Per-entry undo lists across history traversal (issue #57)
// ---------------------------------------------------------------------
//
// GNU readline saves each line's undo list with the line when
// traversal leaves it, and reinstalls that list when traversal lands
// back on it: leaving an edited history entry stores the edited text
// plus `rl_undo_list` into the entry (misc.c
// `_rl_maybe_replace_line`), landing installs the entry's saved list
// (misc.c `rl_replace_from_history` — `rl_undo_list = entry->data`),
// and the live line the traversal started from is kept as the saved
// line (misc.c `_rl_alloc_saved_line`) and restored — text and undo
// list — when traversal walks back past the newest entry (misc.c
// `rl_maybe_unsave_line`). Issue #53 (PR #54) made a landing restart
// the undo stack at the landed text; that covers entries never
// edited this session, whose saved list is empty. The chains below
// pin the saved-list layer on top of it: an edited entry revisited
// must offer the edits made before leaving it to C-_ / M-r, and so
// must the draft line.

#[test]
fn chain_undo_on_revisited_entry_restores_saved_list() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["first entry", "second entry"]);

    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall newest", "second entry", 12);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall older entry", "first entry", 11);

    // Edit the older entry, then hop away: GNU stores the edited
    // text and its undo list into the entry on leaving.
    type_str(&mut rl, " -x");
    expect(&rl, "edited older entry", "first entry -x", 14);
    press(&mut rl, key(KeyCode::Down));
    expect(&rl, "hop to newest", "second entry", 12);

    // Landing back on the entry restores the edited text...
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "back on edited entry", "first entry -x", 14);

    // ...and its saved undo list: C-_ undoes the edit made before
    // leaving, then the list is exhausted.
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo restores pre-edit text", "first entry", 11);
    press(&mut rl, ctrl('_'));
    expect(&rl, "saved list exhausted is a no-op", "first entry", 11);
    press(&mut rl, alt('r'));
    expect(&rl, "revert after undo is a no-op", "first entry", 11);
}

#[test]
fn chain_revert_line_on_revisited_entry_restores_saved_list() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["first entry", "second entry"]);

    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall newest", "second entry", 12);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall older entry", "first entry", 11);

    type_str(&mut rl, " -x");
    expect(&rl, "edited older entry", "first entry -x", 14);
    press(&mut rl, key(KeyCode::Down));
    expect(&rl, "hop to newest", "second entry", 12);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "back on edited entry", "first entry -x", 14);

    // Revert-line walks the entry's whole saved undo list in one
    // step, back to the entry's original text.
    press(&mut rl, alt('r'));
    expect(&rl, "M-r reverts the saved edits", "first entry", 11);
}

#[test]
fn chain_edited_entries_keep_separate_undo_lists() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["first entry", "second entry"]);

    // Edit the newest entry, hop to the older one and edit that too:
    // each entry keeps its own saved text and undo list.
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall newest", "second entry", 12);
    type_str(&mut rl, " -b");
    expect(&rl, "edited newest entry", "second entry -b", 15);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "hop to older entry", "first entry", 11);
    type_str(&mut rl, " -a");
    expect(&rl, "edited older entry", "first entry -a", 14);

    // Back on the newest entry: its edited text is intact, and C-_
    // undoes only its own edit.
    press(&mut rl, key(KeyCode::Down));
    expect(&rl, "back on edited newest", "second entry -b", 15);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo newest entry's edit", "second entry", 12);

    // The older entry's saved edit survived the round trip and is
    // still undoable on its own list.
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "older entry still edited", "first entry -a", 14);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo older entry's edit", "first entry", 11);
}

#[test]
fn chain_undo_on_draft_line_survives_history_roundtrip() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["first entry", "second entry"]);

    // A hand-typed draft with two discrete edits (two pastes, the
    // unit model of chain_undo_walks_back_step_by_step).
    paste(&mut rl, "one ");
    expect(&rl, "draft first paste", "one ", 4);
    paste(&mut rl, "two");
    expect(&rl, "draft second paste", "one two", 7);

    // Enter the walk with the point off the end of the line: plain
    // Up on a non-empty line whose point sits at the end is this
    // engine's prefix search by design (see
    // `get_history_navigation_based_on_line_buffer`), which is not
    // what this chain pins — the saved-line undo restore is.
    press(&mut rl, ctrl('a'));
    expect(&rl, "draft point to start", "one two", 0);

    // Traversal starts: GNU saves the draft line and its undo list
    // as the saved line. Walking back down restores both.
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall newest", "second entry", 12);
    press(&mut rl, key(KeyCode::Down));
    expect(&rl, "back on draft", "one two", 7);

    // C-_ now undoes the draft's own edits, newest first — the
    // history visit in between is invisible to the draft's list.
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo draft's second paste", "one ", 4);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo draft's first paste", "", 0);
    press(&mut rl, ctrl('_'));
    expect(&rl, "draft list exhausted is a no-op", "", 0);
}

#[test]
fn chain_end_of_history_restores_draft_and_its_undo_list() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["first entry", "second entry"]);

    paste(&mut rl, "one ");
    paste(&mut rl, "two");
    expect(&rl, "draft typed", "one two", 7);

    // Point off the end, as in the round-trip chain above, so the
    // walk below is a plain walk and not a prefix search.
    press(&mut rl, ctrl('a'));
    expect(&rl, "draft point to start", "one two", 0);

    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall newest", "second entry", 12);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall older entry", "first entry", 11);

    // M-> (GNU end-of-history, misc.c `rl_end_of_history`) jumps
    // straight back to the saved live line, undo list included.
    press(&mut rl, alt('>'));
    expect(&rl, "M-> restores draft", "one two", 7);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo draft's second paste", "one ", 4);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo draft's first paste", "", 0);
}

#[test]
fn chain_untouched_entries_stay_undo_noops_after_hops() {
    let mut rl = new_rl();
    seed_history(&mut rl, &["first entry", "second entry"]);

    // Hopping between entries never edited this session installs an
    // empty saved list at every landing (the #53 boundary, kept
    // under traversal): undo and revert are no-ops everywhere.
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall newest", "second entry", 12);
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "recall older entry", "first entry", 11);
    press(&mut rl, key(KeyCode::Down));
    expect(&rl, "hop back to newest", "second entry", 12);
    press(&mut rl, ctrl('_'));
    expect(
        &rl,
        "undo on untouched newest is a no-op",
        "second entry",
        12,
    );
    press(&mut rl, alt('r'));
    expect(
        &rl,
        "revert on untouched newest is a no-op",
        "second entry",
        12,
    );
    press(&mut rl, key(KeyCode::Up));
    expect(&rl, "hop back to older entry", "first entry", 11);
    press(&mut rl, ctrl('_'));
    expect(&rl, "undo on untouched older is a no-op", "first entry", 11);
}
