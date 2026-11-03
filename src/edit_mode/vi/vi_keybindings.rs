use crossterm::event::{KeyCode, KeyModifiers};

use crate::{
    edit_mode::keybindings::{
        add_common_control_bindings, add_common_edit_bindings, add_common_navigation_bindings,
        add_common_selection_bindings, add_extending_navigation_bindings, edit_bind, Keybindings,
    },
    EditCommand, Granularity, PromptEditMode, PromptViMode, ReedlineEvent,
};

/// Default Vi normal keybindings
pub fn default_vi_normal_keybindings() -> Keybindings {
    let mut kb = Keybindings::new();
    use EditCommand as EC;
    use KeyCode as KC;
    use KeyModifiers as KM;

    add_common_control_bindings(&mut kb);
    add_common_navigation_bindings(&mut kb);
    add_common_selection_bindings(&mut kb);
    // Replicate vi's default behavior for Backspace and delete
    kb.add_binding(
        KM::NONE,
        KC::Backspace,
        edit_bind(EC::MoveLeft { select: false }),
    );
    kb.add_binding(KM::NONE, KC::Delete, edit_bind(EC::Delete));
    // GNU vi-command binds `C-g` to `abort` (`bind -m vi-command -p`
    // prints `"\C-g": abort`); vi-insert self-inserts it instead, so the
    // insert table does not get this binding.
    kb.add_binding(KM::CONTROL, KC::Char('g'), ReedlineEvent::Esc);
    // C-z suspends — raw mode has no VSUSP delivery, so every mode table
    // needs the explicit binding (same shape as vi-insert above).
    kb.add_binding(KM::CONTROL, KC::Char('z'), ReedlineEvent::Suspend);
    // GNU vi-command binds ^J/^M to accept-line (vi_keymap.c).
    kb.add_binding(KM::CONTROL, KC::Char('j'), ReedlineEvent::Enter);

    kb
}

/// Default Vi visual keybindings: the normal table with its navigation keys
/// rebound to extend the selection.
///
/// Normal's bindings move with `select: false`, which in visual drops the
/// selection and starts a new one under the cursor, so here every key does
/// what its modal twin does in visual. Up and Down never reach history and no
/// key accepts a history hint, as in helix select, which shares the
/// rebinding. Layer custom bindings onto this table rather than onto the
/// normal one.
pub fn default_vi_visual_keybindings() -> Keybindings {
    use EditCommand as EC;
    use KeyCode as KC;
    use KeyModifiers as KM;

    let mut kb = default_vi_normal_keybindings();

    add_extending_navigation_bindings(
        &mut kb,
        EC::MoveLeft { select: true },
        EC::MoveRight { select: true },
    );
    // `d`: take the selection and return to normal. A binding cannot move the
    // machine itself, so the mode change rides along as an event.
    kb.add_binding(
        KM::NONE,
        KC::Delete,
        ReedlineEvent::Multiple(vec![
            edit_bind(EC::CutSelection {
                granularity: Granularity::CharWise,
            }),
            ReedlineEvent::SwitchMode(PromptEditMode::Vi(PromptViMode::Normal)),
        ]),
    );

    kb
}

/// Default Vi insert keybindings
pub fn default_vi_insert_keybindings() -> Keybindings {
    let mut kb = Keybindings::new();

    add_common_control_bindings(&mut kb);
    add_common_navigation_bindings(&mut kb);
    add_common_edit_bindings(&mut kb);
    add_common_selection_bindings(&mut kb);

    // GNU vi-insert's keymap actually binds C-z to rl_insert
    // (vi_keymap.c:340) — the slot is never *reached* because the tty
    // driver's VSUSP char delivers SIGTSTP to the foreground pgrp
    // before readline reads the byte (signals.c). Reedline runs raw
    // mode with ISIG off, so reproducing the observable behavior —
    // C-z suspends — requires this explicit binding.
    kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('z'),
        ReedlineEvent::Suspend,
    );

    // GNU vi-insert binds C-u to `unix-line-discard` (kill.c
    // `rl_unix_line_discard`: kill from point back to the start of the
    // buffer). The common edit bindings carry no C-u, so without this the
    // keypress self-inserts nothing and silently does nothing at all.
    // A6-F09.
    kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('u'),
        edit_bind(EditCommand::CutFromStart),
    );

    kb
}
