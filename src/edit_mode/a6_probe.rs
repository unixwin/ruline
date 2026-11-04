//! A6 audit probes — default keymap bindings vs GNU keymaps.
//! A GNU "kill" command must save its text in the kill ring; a binding that
//! lowers to an erase-only command (or is absent) breaks C-y recovery.
#![cfg(test)]

use super::keybindings::Keybindings;
use crate::{
    default_emacs_keybindings, default_vi_insert_keybindings, EditCommand, ReedlineEvent,
};
use crossterm::event::{KeyCode, KeyModifiers};

fn edit_commands(ev: &ReedlineEvent) -> Vec<EditCommand> {
    match ev {
        ReedlineEvent::Edit(cmds) => cmds.clone(),
        ReedlineEvent::Multiple(evs) => evs.iter().flat_map(edit_commands).collect(),
        ReedlineEvent::UntilFound(evs) => evs.iter().flat_map(edit_commands).collect(),
        _ => Vec::new(),
    }
}

fn bound(kb: &Keybindings, m: KeyModifiers, c: KeyCode) -> Vec<EditCommand> {
    kb.find_binding(m, c).map(|ev| edit_commands(&ev)).unwrap_or_default()
}

#[test]
fn g1_emacs_ctrl_w_is_a_kill() {
    let kb = default_emacs_keybindings();
    let cmds = bound(&kb, KeyModifiers::CONTROL, KeyCode::Char('w'));
    assert!(!cmds.is_empty() && cmds.iter().all(|c| c.feeds_kill_ring()), "got {cmds:?}");
}

#[test]
fn g2_vi_insert_ctrl_w_is_a_kill() {
    let kb = default_vi_insert_keybindings();
    let cmds = bound(&kb, KeyModifiers::CONTROL, KeyCode::Char('w'));
    assert!(!cmds.is_empty() && cmds.iter().all(|c| c.feeds_kill_ring()), "got {cmds:?}");
}

#[test]
fn a6_f09_vi_insert_ctrl_u_is_a_kill() {
    // GNU vi-insert keymap: C-u = unix-line-discard (oracle-verified:
    // clears the line, text saved in the kill ring)
    let kb = default_vi_insert_keybindings();
    let cmds = bound(&kb, KeyModifiers::CONTROL, KeyCode::Char('u'));
    assert!(!cmds.is_empty(), "C-u unbound in vi-insert");
    assert!(cmds.iter().all(|c| c.feeds_kill_ring()), "got {cmds:?}");
}

#[test]
fn g5_emacs_meta_backspace_is_a_kill() {
    let kb = default_emacs_keybindings();
    let cmds = bound(&kb, KeyModifiers::ALT, KeyCode::Backspace);
    assert!(!cmds.is_empty() && cmds.iter().all(|c| c.feeds_kill_ring()), "got {cmds:?}");
}

// ---------- C-x chord table (GNU emacs_ctlx_keymap) ---------------------------

use crate::{Emacs, EditMode, ReedlineRawEvent};
use crossterm::event::{Event, KeyEvent};

fn emacs_key(em: &mut Emacs, code: KeyCode, mods: KeyModifiers) -> ReedlineEvent {
    let raw = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, mods))).unwrap();
    em.parse_event(raw)
}

#[test]
fn g6_ctrl_x_ctrl_r_re_reads_init_file() {
    // GNU emacs_ctlx_keymap (emacs_keymap.c:616): C-x C-r =
    // re-read-init-file. The chord must resolve to a real event.
    let mut em = Emacs::default();
    let _ = emacs_key(&mut em, KeyCode::Char('x'), KeyModifiers::CONTROL);
    let ev = emacs_key(&mut em, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_ne!(ev, ReedlineEvent::None, "C-x C-r resolved to nothing");
}

#[test]
fn issue37_ctrl_x_ctrl_r_yields_the_re_read_init_file_event() {
    // The chord resolves to exactly the dedicated public event, and a
    // plain (non-control) `r` after C-x stays unbound — only C-r is
    // re-read-init-file in the ctlx keymap.
    let mut em = Emacs::default();
    let _ = emacs_key(&mut em, KeyCode::Char('x'), KeyModifiers::CONTROL);
    let ev = emacs_key(&mut em, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(ev, ReedlineEvent::ReReadInitFile);

    let mut em = Emacs::default();
    let _ = emacs_key(&mut em, KeyCode::Char('x'), KeyModifiers::CONTROL);
    let ev = emacs_key(&mut em, KeyCode::Char('r'), KeyModifiers::NONE);
    assert_eq!(ev, ReedlineEvent::None);
}

#[test]
fn g7_ctrl_x_ctrl_u_is_undo() {
    let mut em = Emacs::default();
    let _ = emacs_key(&mut em, KeyCode::Char('x'), KeyModifiers::CONTROL);
    let ev = emacs_key(&mut em, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(ev, ReedlineEvent::Edit(vec![EditCommand::Undo]));
}
