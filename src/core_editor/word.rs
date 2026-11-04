//! Character classification for word-boundary detection — the shared substrate
//! for vi, emacs, and helix word motions.
//!
//! Every "word" notion in the editor is built on one classifier: each character
//! is sorted into a [`CharClass`], and a word boundary is a *transition* between
//! classes. The flavors differ only in which transitions count:
//! - **small word** (`w`/`b`/`e`): any class change is a boundary.
//! - **big WORD** (`W`/`B`/`E`): only whitespace/EOL transitions count, so a run
//!   of `Word` and `Punctuation` together is one WORD.
//!
//! Modes pick a flavor; the resolver (`locate_word`) scans with the matching
//! predicate. Keeping the classifier here — mode-agnostic and tested in isolation
//! — means vi-word, vi-WORD, emacs-word, and helix-word are thin variations over
//! one definition rather than eight ad-hoc functions. Two flavors don't fit the
//! classifier and keep their own scans below: `Unicode` (UAX-29 segmentation)
//! and `Alphabetic` (GNU readline's alphanumeric runs, whose separators form
//! no words of their own).

use unicode_segmentation::UnicodeSegmentation;

use crate::core_editor::graphemes::{
    ensure_grapheme_boundary_next, ensure_grapheme_boundary_prev, next_grapheme_boundary,
};
use crate::core_editor::line_buffer::is_whitespace_str;
use crate::enums::{Direction, WordEdge, WordKind};

/// Classification of a character for word-boundary detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CharClass {
    /// Alphanumeric or `_` — the characters that make up a "word".
    Word,
    /// Anything else that isn't whitespace or a line ending.
    Punctuation,
    /// Spaces, tabs, etc.
    Whitespace,
    /// Line endings, kept separate from whitespace so a newline always forms a
    /// boundary with adjacent spaces (a word motion never glides across lines).
    /// Both `\n` and the `\r` of a `\r\n` terminator land here, so a word motion
    /// never treats the carriage return as a punctuation "word" and never parks
    /// the cursor or an operator edge on it (matches `line::end_of_line`).
    Eol,
}

/// Sort `ch` into a [`CharClass`].
pub(crate) fn categorize_char(ch: char) -> CharClass {
    match ch {
        '\n' | '\r' => CharClass::Eol,
        ch if ch.is_alphanumeric() || ch == '_' => CharClass::Word,
        ch if ch.is_whitespace() => CharClass::Whitespace,
        _ => CharClass::Punctuation,
    }
}

/// `true` if a *small word* boundary lies between `a` and `b` — any class change.
pub(crate) fn is_word_boundary(a: char, b: char) -> bool {
    categorize_char(a) != categorize_char(b)
}

/// `true` if a *big WORD* boundary lies between `a` and `b` — a class change,
/// except `Word`↔`Punctuation`, which stay fused into one WORD.
pub(crate) fn is_long_word_boundary(a: char, b: char) -> bool {
    match (categorize_char(a), categorize_char(b)) {
        (CharClass::Word, CharClass::Punctuation) | (CharClass::Punctuation, CharClass::Word) => {
            false
        }
        (a, b) => a != b,
    }
}

/// Byte offset of the *bar* word boundary reached from `origin`, scanning
/// `forward` (or backward), using `kind`'s boundary predicate and landing on
/// `edge`. A boundary is a gap between graphemes — a `Start` is a word's first
/// grapheme, an `End` is the gap *after* its last grapheme.
///
/// This is the single resolver the 8 ad-hoc `LineBuffer::*_index` functions
/// collapse into. The `(forward, edge)` pairs map to motions:
/// - `(true,  Start)` → `w` / `W`   (next word's first char)
/// - `(true,  End)`   → `M-f` / `E` (word's trailing boundary)
/// - `(false, Start)` → `b` / `B`   (previous word's first char)
///
/// Pure word *structure* — caret geometry (block vs bar) lives one layer up in
/// [`resolve_motion`](super::resolve_motion), which turns a forward word-end
/// into the on-grapheme vi-`e` landing when the mode is block-cellular.
pub(crate) fn locate_word(
    buf: &str,
    origin: usize,
    kind: WordKind,
    edge: WordEdge,
    direction: Direction,
) -> usize {
    // Every flavor below slices `buf` at `origin`. A caller holding a
    // position from before a length-changing edit (e.g. a case
    // conversion that shrank the buffer) must not panic the scan:
    // resolve from the nearest valid boundary at or before it. A6-F04.
    let origin = buf.floor_char_boundary(origin.min(buf.len()));
    let forward = direction == Direction::Forward;

    let is_boundary: fn(char, char) -> bool = match kind {
        WordKind::Word => is_word_boundary,
        WordKind::LongWord => is_long_word_boundary,
        // `Unicode` (emacs) is not expressible as a char-class boundary
        // predicate — it uses UAX-29 segmentation, with its own scan.
        WordKind::Unicode => return locate_unicode_word(buf, origin, edge, direction),
        // Neither is `Alphabetic` (GNU): its separators (punctuation, `_`)
        // are non-word *and* form no words of their own, so a transition
        // resolver would wrongly stop on them. Its own scan below.
        WordKind::Alphabetic => return locate_alphabetic_word(buf, origin, edge, direction),
    };

    // Is `ch` (with neighbors `before`/`after`, `None` at the buffer edges) the
    // `edge` of a word? A word excludes whitespace/EOL, so its `Start` is a
    // non-whitespace char with a boundary on its left (or the buffer start),
    // and its `End` one with a boundary on its right (or the buffer end).
    let is_target = |before: Option<char>, ch: char, after: Option<char>| -> bool {
        // A word excludes whitespace and line endings — use the module's own
        // classifier rather than a bare `is_whitespace` so `\n` (an `Eol`) is
        // handled by the same definition the boundary checks trust.
        if matches!(categorize_char(ch), CharClass::Whitespace | CharClass::Eol) {
            return false;
        }
        match edge {
            WordEdge::Start => before.is_none_or(|b| is_boundary(b, ch)),
            WordEdge::End => after.is_none_or(|a| is_boundary(ch, a)),
        }
    };

    // Scan outward from the origin without materializing the buffer: each
    // candidate's outer neighbor is read through `peek()`, its inner neighbor
    // carried from the previous step (seeded with the char on the origin's
    // other side).
    if forward {
        // first target strictly after origin
        let mut before = buf[..origin].chars().next_back();
        let mut iter = buf[origin..]
            .char_indices()
            .map(|(i, c)| (origin + i, c))
            .peekable();
        while let Some((byte, ch)) = iter.next() {
            let after = iter.peek().map(|&(_, c)| c);
            if is_target(before, ch, after) {
                // Bar boundary: a `Start` is the first grapheme of the word; an
                // `End` is the gap *after* its last grapheme. (The block-caret
                // `e` rendering is applied by the identity at the top.)
                let boundary = match edge {
                    // `char_indices` yields char boundaries, which can fall
                    // *inside* a grapheme cluster (e.g. a base char followed by a
                    // combining mark of a different `CharClass`). Floor a `Start`
                    // onto its grapheme boundary so operators never cut mid-cluster
                    // and orphan a combining mark. (`End` already snaps forward.)
                    WordEdge::Start => ensure_grapheme_boundary_prev(buf, byte),
                    WordEdge::End => next_grapheme_boundary(buf, byte),
                };
                if boundary > origin {
                    return boundary;
                }
            }
            before = Some(ch);
        }
        // none found: both `w` and the trailing-boundary `e` run to the end.
        buf.len()
    } else {
        // nearest target strictly before origin
        let mut after = buf[origin..].chars().next();
        let mut iter = buf[..origin].char_indices().rev().peekable();
        while let Some((byte, ch)) = iter.next() {
            let before = iter.peek().map(|&(_, c)| c);
            if is_target(before, ch, after) {
                // Floor onto a grapheme boundary — see the forward `Start` note.
                return ensure_grapheme_boundary_prev(buf, byte);
            }
            after = Some(ch);
        }
        0
    }
}

/// `Unicode` (emacs) word location via UAX-29 segmentation: skip whitespace
/// segments from `split_word_bound_indices`, land on the requested *bar*
/// boundary. The block-caret `e` rendering is applied by the identity at the top
/// of [`locate_word`], so this only ever computes exclusive boundaries.
fn locate_unicode_word(buf: &str, origin: usize, edge: WordEdge, direction: Direction) -> usize {
    let forward = direction == Direction::Forward;
    match (forward, edge) {
        // `w` — start of the next word (skip the cursor's own segment + whitespace).
        (true, WordEdge::Start) => buf[origin..]
            .split_word_bound_indices()
            .find(|(i, w)| *i != 0 && !is_whitespace_str(w))
            .map_or(buf.len(), |(i, _)| origin + i),
        // `M-f` — the trailing boundary of the next word (the gap after its last
        // grapheme), strictly past origin so a caret mid-word completes it.
        (true, WordEdge::End) => buf[origin..]
            .split_word_bound_indices()
            .filter(|(_, w)| !is_whitespace_str(w))
            .find_map(|(i, w)| (origin + i + w.len() > origin).then_some(origin + i + w.len()))
            .unwrap_or(buf.len()),
        // `b` — start of the previous word.
        (false, WordEdge::Start) => buf[..origin]
            .split_word_bound_indices()
            .rfind(|(_, w)| !is_whitespace_str(w))
            .map_or(0, |(i, _)| i),
        // `ge` — last grapheme of the previous word. No legacy reference (vi `ge`
        // uses a class flavor); defined here only to keep the resolver total.
        (false, WordEdge::End) => buf[..origin]
            .split_word_bound_indices()
            .rev()
            .find(|(_, w)| !is_whitespace_str(w))
            .and_then(|(i, w)| w.grapheme_indices(true).next_back().map(|(gi, _)| i + gi))
            .unwrap_or(0),
    }
}

/// `Alphabetic` (GNU readline) word location: a word is a maximal run of
/// alphanumeric characters (`_rl_walphabetic`, lib/readline/rlmbutil.h:216 —
/// `ISALNUM`, chardefs.h:103; the multibyte build's `iswalnum`, which
/// `char::is_alphanumeric` mirrors). The classification applies per
/// grapheme cluster by its base char, since the multibyte build reads
/// base+combining-marks as one character: U+0301 alone is not
/// alphanumeric, yet "e\u{301}" is one alphabetic character to GNU and
/// the run continues into the next letter. Everything else —
/// whitespace, line endings, punctuation, `_` — is separator and forms
/// no word, which is the whole behavioral content of
/// `rl_forward_word` / `rl_backward_word` (text.c:488 / text.c:540):
/// skip separators, then consume the run.
///
/// Landings are *bar* boundaries over the run set, mirroring the class
/// flavors' geometry: `(Forward, End)` is the end of the first run ending
/// after `origin` (GNU `M-f`), `(Backward, Start)` the start of the last
/// run starting before `origin` (GNU `M-b`); `(Forward, Start)` and
/// `(Backward, End)` are the symmetric next-start / previous-end for the
/// consumers that ask for them. Start landings floor onto a grapheme
/// boundary and end landings snap forward, like [`locate_word`]'s scans.
fn locate_alphabetic_word(buf: &str, origin: usize, edge: WordEdge, direction: Direction) -> usize {
    // Maximal alphanumeric runs as (start, end) byte ranges, classified
    // per grapheme cluster by its base char: a cluster whose base is
    // alphanumeric is a word cluster whole, combining marks and all.
    // A per-char scan ends the run mid-grapheme after the base of e.g.
    // "e\u{301}" (U+0301 is not itself alphanumeric) and the landing
    // snap then stops the motion at the grapheme ceiling, where GNU's
    // wide-char build reads base+marks as one alphabetic character —
    // `M-f` over "e\u{301}x" lands past the `x`, one whole word.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut run_start: Option<usize> = None;
    for (byte, cluster) in buf.grapheme_indices(true) {
        let word_cluster = cluster
            .chars()
            .next()
            .is_some_and(|base| base.is_alphanumeric());
        if word_cluster {
            if run_start.is_none() {
                run_start = Some(byte);
            }
        } else if let Some(start) = run_start.take() {
            runs.push((start, byte));
        }
    }
    if let Some(start) = run_start {
        runs.push((start, buf.len()));
    }

    let forward = direction == Direction::Forward;
    match (forward, edge) {
        (true, WordEdge::End) => runs
            .iter()
            .map(|&(_, end)| end)
            .find(|&end| end > origin)
            .map_or(buf.len(), |end| ensure_grapheme_boundary_next(buf, end)),
        (true, WordEdge::Start) => runs
            .iter()
            .map(|&(start, _)| start)
            .find(|&start| start > origin)
            .map_or(buf.len(), |start| ensure_grapheme_boundary_prev(buf, start)),
        (false, WordEdge::Start) => runs
            .iter()
            .map(|&(start, _)| start)
            .rfind(|&start| start < origin)
            .map_or(0, |start| ensure_grapheme_boundary_prev(buf, start)),
        // The previous run's end: the last run that ends at or before
        // `origin` without containing it (a run `origin` sits inside is
        // the *current* word, not the previous one).
        (false, WordEdge::End) => runs
            .iter()
            .filter(|&&(start, end)| end <= origin && start < origin)
            .map(|&(_, end)| end)
            .next_back()
            .map_or(0, |end| ensure_grapheme_boundary_next(buf, end)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categorize_each_class() {
        assert_eq!(categorize_char('a'), CharClass::Word);
        assert_eq!(categorize_char('Z'), CharClass::Word);
        assert_eq!(categorize_char('7'), CharClass::Word);
        assert_eq!(categorize_char('_'), CharClass::Word);
        assert_eq!(categorize_char('é'), CharClass::Word); // unicode alphanumeric
        assert_eq!(categorize_char(' '), CharClass::Whitespace);
        assert_eq!(categorize_char('\t'), CharClass::Whitespace);
        assert_eq!(categorize_char('\n'), CharClass::Eol);
        assert_eq!(categorize_char('\r'), CharClass::Eol);
        assert_eq!(categorize_char('.'), CharClass::Punctuation);
        assert_eq!(categorize_char('-'), CharClass::Punctuation);
    }

    #[test]
    fn carriage_return_is_eol_not_a_word() {
        // `\r` and `\n` are line endings, not word chars. "ab\r\ncd" is bytes
        // a=0 b=1 \r=2 \n=3 c=4 d=5. A forward word-end stops at "ab"'s trailing
        // boundary (byte 2, the gap before `\r`) — `\r` is not fused into the
        // word...
        assert_eq!(
            locate_word(
                "ab\r\ncd",
                0,
                WordKind::Word,
                WordEdge::End,
                Direction::Forward
            ),
            2
        );
        // ...and from the line ending it crosses `\r\n` to finish "cd" at the
        // buffer end (byte 6), never parking on `\r` as a word of its own.
        assert_eq!(
            locate_word(
                "ab\r\ncd",
                2,
                WordKind::Word,
                WordEdge::End,
                Direction::Forward
            ),
            6
        );
    }

    #[test]
    fn small_word_boundary_is_any_class_change() {
        assert!(is_word_boundary('a', '.')); // Word → Punctuation
        assert!(is_word_boundary('.', 'a')); // Punctuation → Word
        assert!(is_word_boundary('a', ' ')); // Word → Whitespace
        assert!(is_word_boundary(' ', '\n')); // Whitespace → Eol
        assert!(!is_word_boundary('a', 'b')); // both Word
        assert!(!is_word_boundary('.', ',')); // both Punctuation
    }

    #[test]
    fn issue31_alphabetic_forward_end_stops_at_punctuation() {
        // GNU `M-f` on "foo.bar" from 0 ends after "foo" (text.c:488):
        // '.' is a separator, and separators form no word to stop on.
        assert_eq!(
            locate_word(
                "foo.bar",
                0,
                WordKind::Alphabetic,
                WordEdge::End,
                Direction::Forward
            ),
            3
        );
        // From the '.' itself, `M-f` skips it and finishes "bar".
        assert_eq!(
            locate_word(
                "foo.bar",
                3,
                WordKind::Alphabetic,
                WordEdge::End,
                Direction::Forward
            ),
            7
        );
        // Mid-word, `M-f` completes the current run only.
        assert_eq!(
            locate_word(
                "foo.bar",
                1,
                WordKind::Alphabetic,
                WordEdge::End,
                Direction::Forward
            ),
            3
        );
    }

    #[test]
    fn issue46_alphabetic_combining_marks_ride_their_base_char() {
        // "e\u{301}x" is e + COMBINING ACUTE + x: 4 bytes, 2 graphemes.
        // U+0301 is not alphanumeric by itself, but GNU's multibyte
        // build reads base+marks as one alphabetic character, so `M-f`
        // from 0 consumes the whole string as one word (PTY-probed
        // against GNU readline) instead of stopping at the é grapheme.
        let buf = "e\u{301}x";
        assert_eq!(
            locate_word(buf, 0, WordKind::Alphabetic, WordEdge::End, Direction::Forward),
            4
        );
        assert_eq!(
            locate_word(
                buf,
                4,
                WordKind::Alphabetic,
                WordEdge::Start,
                Direction::Backward
            ),
            0
        );
        // A mark on the far side of a separator still separates: the
        // buffer "a \u{301}b" has the mark leading its own cluster
        // (base = the mark, not alphanumeric), so `M-f` from 0 stops
        // after "a" and from the space finishes "b".
        let buf = "a \u{301}b";
        assert_eq!(
            locate_word(buf, 0, WordKind::Alphabetic, WordEdge::End, Direction::Forward),
            1
        );
        assert_eq!(
            locate_word(buf, 1, WordKind::Alphabetic, WordEdge::End, Direction::Forward),
            5
        );
    }

    #[test]
    fn issue31_alphabetic_underscore_is_a_separator() {
        // `_` is not alphabetic in GNU (chardefs.h:103 ISALNUM), unlike
        // the class flavors where it joins the word: "foo_bar" is two
        // GNU words.
        assert_eq!(
            locate_word(
                "foo_bar",
                0,
                WordKind::Alphabetic,
                WordEdge::End,
                Direction::Forward
            ),
            3
        );
        assert_eq!(
            locate_word(
                "foo_bar",
                7,
                WordKind::Alphabetic,
                WordEdge::Start,
                Direction::Backward
            ),
            4
        );
    }

    #[test]
    fn issue31_alphabetic_backward_start_and_next_start() {
        // GNU `M-b` from the end of "foo.bar" lands on "bar"'s start;
        // inside "bar" it lands on that same start; at "bar"'s start it
        // retreats to "foo"'s start (text.c:540).
        for (origin, want) in [(7, 4), (6, 4), (4, 0), (3, 0), (0, 0)] {
            assert_eq!(
                locate_word(
                    "foo.bar",
                    origin,
                    WordKind::Alphabetic,
                    WordEdge::Start,
                    Direction::Backward
                ),
                want,
                "M-b from {origin}"
            );
        }
        // Next word start skips separator runs wholesale.
        assert_eq!(
            locate_word(
                "foo... bar",
                0,
                WordKind::Alphabetic,
                WordEdge::Start,
                Direction::Forward
            ),
            7
        );
        // No word ahead: the buffer end; no word behind: 0.
        assert_eq!(
            locate_word(
                "foo ",
                4,
                WordKind::Alphabetic,
                WordEdge::End,
                Direction::Forward
            ),
            4
        );
        assert_eq!(
            locate_word(
                " foo",
                0,
                WordKind::Alphabetic,
                WordEdge::Start,
                Direction::Backward
            ),
            0
        );
    }

    #[test]
    fn issue31_alphabetic_digits_join_letters() {
        // ISALNUM covers digits: "abc123" is one GNU word, and a leading
        // digit run is a word of its own ("123abc" capitalizes as one).
        assert_eq!(
            locate_word(
                "abc123 x",
                0,
                WordKind::Alphabetic,
                WordEdge::End,
                Direction::Forward
            ),
            6
        );
    }

    #[test]
    fn big_word_boundary_fuses_word_and_punctuation() {
        // Word↔Punctuation is NOT a big-WORD boundary (e.g. `foo.bar` is one WORD)
        assert!(!is_long_word_boundary('o', '.'));
        assert!(!is_long_word_boundary('.', 'b'));
        // but whitespace/eol transitions still are
        assert!(is_long_word_boundary('a', ' '));
        assert!(is_long_word_boundary('.', ' '));
        assert!(is_long_word_boundary(' ', '\n'));
        assert!(!is_long_word_boundary('a', 'b')); // same class, no boundary
    }
}
