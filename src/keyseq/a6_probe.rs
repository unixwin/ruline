//! A6 audit probes — keyseq spelling translation, GNU `rl_translate_keyseq`
//! semantics (bind.c / rldefs escape definitions).
#![cfg(test)]

use super::{parse_readline_chord, parse_readline_keyseq};
use crossterm::event::{KeyCode, KeyModifiers};

#[test]
fn a6_f12_control_question_is_del() {
    // GNU: \C-? is the DEL character (0x7f) — the Backspace key event.
    assert_eq!(
        parse_readline_keyseq(r"\C-?"),
        Some((KeyModifiers::NONE, KeyCode::Backspace))
    );
}

#[test]
fn a6_f12_backslash_d_is_del() {
    // GNU keyseq escape \d = DEL (rubout).
    assert_eq!(
        parse_readline_keyseq(r"\d"),
        Some((KeyModifiers::NONE, KeyCode::Backspace))
    );
}

#[test]
fn a6_f12_backslash_t_is_tab() {
    // GNU keyseq escape \t = TAB character.
    assert_eq!(
        parse_readline_keyseq(r"\t"),
        Some((KeyModifiers::NONE, KeyCode::Tab))
    );
}

#[test]
fn a6_f13_caret_csi_is_one_key_not_a_chord() {
    // "^[[A" names ONE key (Up), exactly as parse_readline_keyseq treats it;
    // the chord splitter must not shred it into Esc + '[' + 'A'.
    assert_eq!(
        parse_readline_keyseq("^[[A"),
        Some((KeyModifiers::NONE, KeyCode::Up))
    );
    assert_ne!(
        parse_readline_chord("^[[A"),
        Some(vec![
            (KeyModifiers::NONE, KeyCode::Esc),
            (KeyModifiers::NONE, KeyCode::Char('[')),
            (KeyModifiers::NONE, KeyCode::Char('A')),
        ])
    );
}

#[test]
fn a6_f12_meta_control_combination() {
    // GNU: "\M-\C-f" is meta+control f.
    assert_eq!(
        parse_readline_keyseq(r"\M-\C-f"),
        Some((
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyCode::Char('f')
        ))
    );
}

#[test]
fn a6_f12_control_meta_combination() {
    // GNU: "\C-\M-f" likewise.
    assert_eq!(
        parse_readline_keyseq(r"\C-\M-f"),
        Some((
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyCode::Char('f')
        ))
    );
}
