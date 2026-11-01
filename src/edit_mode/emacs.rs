use crate::{
    edit_mode::{
        combine_events,
        keybindings::{
            add_common_control_bindings, add_common_edit_bindings, add_common_navigation_bindings,
            add_common_selection_bindings, edit_bind, KeyCombination, Keybindings,
        },
        normalized_key_combination, parse_non_key_event, EditMode, SequenceResolution,
        SequenceResolver,
    },
    enums::{EditCommand, EventStatus, ReedlineEvent, ReedlineRawEvent},
    Direction, PromptEditMode,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

/// Returns the current default emacs keybindings
pub fn default_emacs_keybindings() -> Keybindings {
    use EditCommand as EC;
    use KeyCode as KC;
    use KeyModifiers as KM;

    let mut kb = Keybindings::new();
    add_common_control_bindings(&mut kb);
    add_common_navigation_bindings(&mut kb);
    add_common_edit_bindings(&mut kb);
    add_common_selection_bindings(&mut kb);

    // This could be in common, but in Vi it also changes the mode
    kb.add_binding(KM::NONE, KC::Enter, ReedlineEvent::Enter);

    // *** CTRL ***
    // Moves
    kb.add_binding(
        KM::CONTROL,
        KC::Char('b'),
        ReedlineEvent::UntilFound(vec![ReedlineEvent::MenuLeft, ReedlineEvent::Left]),
    );
    kb.add_binding(
        KM::CONTROL,
        KC::Char('f'),
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::MenuRight,
            ReedlineEvent::Right,
        ]),
    );
    // Undo/Redo. GNU's emacs keymap leaves C-z unbound — the tty
    // driver's VSUSP char sends SIGTSTP to the foreground pgrp before
    // readline sees the byte (signals.c rl_stop handles the caught
    // signal). With ISIG off there is no VSUSP delivery, so we bind the
    // suspend event directly to reproduce GNU's observable behavior.
    // Undo is C-_ (emacs_keymap.c:66 `rl_undo_command`). Terminals
    // spell the 0x1f byte three ways: crossterm's ANSI parser decodes
    // it as Control-'7' (`\x1C`..`\x1F` maps to '4'..'7'), the kitty
    // protocol reports the '_' glyph, and Windows INPUT_RECORDs (the
    // ConPTY path) surface as Control+Shift+'-' — which the
    // `ReedlineRawEvent` ingest seam folds onto Control+'_'
    // (`normalize_control_underscore`). Both surviving spellings bind.
    kb.add_binding(KM::CONTROL, KC::Char('_'), edit_bind(EC::Undo));
    kb.add_binding(KM::CONTROL, KC::Char('7'), edit_bind(EC::Undo));
    // GNU `abort` — `rl_abort` (readline misc.c): cancels the current
    // editing command and rings the bell. `bind -p` prints `"\C-g":
    // abort`; the same slot sits in the vi-command map (`bind -m
    // vi-command -p`). GNU readline has NO default `redo` binding at all
    // (`bind -l` lists no `redo` function — only `vi-redo`, unbound), so
    // the previous `Redo` binding here was an invention that also broke
    // `C-g` aborting an active search: the engine's search dispatches
    // abort on the `Esc` event.
    kb.add_binding(KM::CONTROL, KC::Char('g'), ReedlineEvent::Esc);
    kb.add_binding(KM::CONTROL, KC::Char('z'), ReedlineEvent::Suspend);
    // GNU revert-line (undo.c:331): M-r and M-C-r undo every change to
    // the current line (emacs_keymap.c:445 and :336 respectively).
    kb.add_binding(KM::ALT, KC::Char('r'), edit_bind(EC::RevertLine));
    kb.add_binding(
        KM::CONTROL | KM::ALT,
        KC::Char('r'),
        edit_bind(EC::RevertLine),
    );
    // GNU character-search (text.c:1829): C-] reads a char and moves to
    // its next occurrence, M-C-] searches backward
    // (emacs_keymap.c:64 and :348).
    kb.add_binding(
        KM::CONTROL,
        KC::Char(']'),
        ReedlineEvent::CharSearch {
            direction: Direction::Forward,
        },
    );
    kb.add_binding(
        KM::CONTROL | KM::ALT,
        KC::Char(']'),
        ReedlineEvent::CharSearch {
            direction: Direction::Backward,
        },
    );
    // GNU yank-last-arg / yank-nth-arg (kill.c:603-702): M-. and M-_
    // yank the last argument of the previous history line, M-C-y the
    // first (emacs_keymap.c:367, :424, :343). Alt-underscore arrives as
    // Alt-Shift-hyphen on most terminals, so both spellings bind.
    kb.add_binding(
        KM::ALT,
        KC::Char('.'),
        ReedlineEvent::YankLastArg { count: None },
    );
    kb.add_binding(
        KM::ALT,
        KC::Char('_'),
        ReedlineEvent::YankLastArg { count: None },
    );
    kb.add_binding(
        KM::ALT | KM::SHIFT,
        KC::Char('-'),
        ReedlineEvent::YankLastArg { count: None },
    );
    kb.add_binding(
        KM::ALT | KM::SHIFT,
        KC::Char('_'),
        ReedlineEvent::YankLastArg { count: None },
    );
    kb.add_binding(
        KM::CONTROL | KM::ALT,
        KC::Char('y'),
        ReedlineEvent::YankNthArg { count: 1 },
    );
    // GNU operate-and-get-next (misc.c:782): emacs C-o accepts the line
    // and preloads the next history entry into the next read_line
    // (emacs_keymap.c:50). Overrides the common map's OpenEditor, which
    // stays bound in the vi/helix maps — GNU leaves vi C-o unbound.
    kb.add_binding(KM::CONTROL, KC::Char('o'), ReedlineEvent::OperateAndGetNext);
    // Cutting
    // GNU C-y is `rl_yank` (kill.c:509) — the kill ring's current slot —
    // and M-y is `rl_yank_pop` (kill.c:527, emacs_keymap.c Meta-y).
    kb.add_binding(KM::CONTROL, KC::Char('y'), edit_bind(EC::Yank));
    kb.add_binding(KM::ALT, KC::Char('y'), edit_bind(EC::YankPop));
    kb.add_binding(KM::CONTROL, KC::Char('w'), edit_bind(EC::CutWordLeft));
    kb.add_binding(KM::CONTROL, KC::Char('k'), edit_bind(EC::KillLine));
    kb.add_binding(KM::CONTROL, KC::Char('u'), edit_bind(EC::CutFromStart));
    kb.add_binding(KM::ALT, KC::Char('d'), edit_bind(EC::CutWordRight));
    // Edits
    kb.add_binding(KM::CONTROL, KC::Char('t'), edit_bind(EC::SwapGraphemes));

    // GNU history jumps: `\e<`/`\e>` are beginning-of-history /
    // end-of-history (emacs_keymap.c:399/:401 — `bind -p` shows
    // `"\e<": beginning-of-history`, `"\e>": end-of-history`), NOT a
    // buffer-edges move. `M->` returns to the live (new) line, which the
    // engine's `last_history` models by leaving history traversal.
    kb.add_binding(KM::ALT, KC::Char('<'), ReedlineEvent::FirstHistoryEntry);
    kb.add_binding(KM::ALT, KC::Char('>'), ReedlineEvent::LastHistoryEntry);
    // For kitty keyboard protocol
    kb.add_binding(
        KM::SHIFT | KM::ALT,
        KC::Char(','),
        ReedlineEvent::FirstHistoryEntry,
    );
    kb.add_binding(
        KM::SHIFT | KM::ALT,
        KC::Char('.'),
        ReedlineEvent::LastHistoryEntry,
    );

    // *** ALT ***
    // Moves
    kb.add_binding(
        KM::ALT,
        KC::Left,
        edit_bind(EC::MoveWordLeft { select: false }),
    );
    kb.add_binding(
        KM::ALT,
        KC::Right,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintWordComplete,
            edit_bind(EC::MoveWordRight { select: false }),
        ]),
    );
    kb.add_binding(
        KM::ALT,
        KC::Char('b'),
        edit_bind(EC::MoveWordLeft { select: false }),
    );
    kb.add_binding(
        KM::ALT,
        KC::Char('f'),
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintWordComplete,
            edit_bind(EC::MoveWordRight { select: false }),
        ]),
    );
    // Edits
    kb.add_binding(KM::ALT, KC::Delete, edit_bind(EC::DeleteWord));
    kb.add_binding(KM::ALT, KC::Backspace, edit_bind(EC::BackspaceWord));
    kb.add_binding(
        KM::ALT,
        KC::Char('m'),
        ReedlineEvent::Edit(vec![EditCommand::BackspaceWord]),
    );
    // Case changes and transposition: GNU's emacs_meta_keymap binds
    // M-u/M-l/M-c to upcase/downcase/capitalize-word and M-t to
    // transpose-words (emacs_keymap.c:447 `rl_transpose_words`).
    kb.add_binding(KM::ALT, KC::Char('u'), edit_bind(EC::UppercaseWord));
    kb.add_binding(KM::ALT, KC::Char('l'), edit_bind(EC::LowercaseWord));
    kb.add_binding(KM::ALT, KC::Char('c'), edit_bind(EC::CapitalizeWord));
    kb.add_binding(KM::ALT, KC::Char('t'), edit_bind(EC::SwapWords));

    // GNU `digit-argument` (misc.c:230): `M-0`..`M-9` accumulate the
    // numeric argument, `M--` negates it — the emacs_meta_keymap rows
    // (emacs_keymap.c:366-380). `C-u` stays `unix-line-discard`, which
    // is GNU's binding for it too (emacs_keymap.c:56).
    for d in 0..=9u8 {
        kb.add_binding(
            KM::ALT,
            KC::Char(char::from(b'0' + d)),
            ReedlineEvent::DigitArgument(Some(d)),
        );
    }
    kb.add_binding(KM::ALT, KC::Char('-'), ReedlineEvent::DigitArgument(None));

    // GNU non-incremental history search (search.c:462 `noninc_search`):
    // `M-n`/`M-p` collect a string in-place, then `Enter` runs one
    // forward/backward search (emacs_keymap.c:441/:443).
    kb.add_binding(
        KM::ALT,
        KC::Char('n'),
        ReedlineEvent::NonIncrementalSearch {
            direction: Direction::Forward,
            prompt_char: ':',
            pattern: false,
        },
    );
    kb.add_binding(
        KM::ALT,
        KC::Char('p'),
        ReedlineEvent::NonIncrementalSearch {
            direction: Direction::Backward,
            prompt_char: ':',
            pattern: false,
        },
    );

    // GNU `set-mark` (text.c:1880): `C-@` and `M-SPC` drop the mark at
    // point (emacs_keymap.c:35, :353). Terminals report `C-@` as
    // Ctrl-Space or Ctrl-@; both spellings bind.
    kb.add_binding(KM::CONTROL, KC::Char(' '), edit_bind(EC::SetMark));
    kb.add_binding(KM::CONTROL, KC::Char('@'), edit_bind(EC::SetMark));
    kb.add_binding(KM::ALT, KC::Char(' '), edit_bind(EC::SetMark));

    // GNU `insert-comment` (text.c:1384 `rl_insert_comment`): `M-#`
    // comments out the line and submits it (emacs_keymap.c:356 —
    // `RL_COMMENT_BEGIN_DEFAULT` is `"# "`). The un-commenting explicit
    // argument form is not modeled.
    kb.add_binding(
        KM::ALT,
        KC::Char('#'),
        ReedlineEvent::Multiple(vec![
            edit_bind(EC::MoveToLineStart { select: false }),
            edit_bind(EC::InsertString("# ".to_string())),
            ReedlineEvent::Submit,
        ]),
    );

    kb
}

/// This parses the incoming Events like a emacs style-editor
pub struct Emacs {
    keybindings: Keybindings,
    /// `C-x` pressed and the chord unresolved: GNU keeps a whole
    /// `emacs_ctlx_keymap` for it (emacs_keymap.c:59 `ISKMAP`). The
    /// general sequence trie can express it too, but the ctlx table is
    /// GNU's own default and an explicit `C-x` rebind still wins, so
    /// the prefix keeps its dedicated slot.
    pending_prefix: bool,
    resolver: SequenceResolver,
}

impl Default for Emacs {
    fn default() -> Self {
        Emacs {
            keybindings: default_emacs_keybindings(),
            pending_prefix: false,
            resolver: SequenceResolver::new(),
        }
    }
}

impl EditMode for Emacs {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        match event.into() {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => {
                // A pending `C-x` resolves against GNU's ctlx keymap
                // first: the second key decides the event regardless of
                // its own binding (emacs_keymap.c:597+), an unbound one
                // dings. Explicit multi-key sequences use the shared
                // resolver inside dispatch_key.
                if self.pending_prefix {
                    self.pending_prefix = false;
                    return Self::resolve_ctrl_x(modifiers, code);
                }
                self.dispatch_key(modifiers, code)
            }

            event => parse_non_key_event(event),
        }
    }

    fn edit_mode(&self) -> PromptEditMode {
        PromptEditMode::Emacs
    }

    fn handle_mode_specific_event(&mut self, event: ReedlineEvent) -> EventStatus {
        // One state, so the only switch emacs accepts is into itself.
        match event {
            ReedlineEvent::SwitchMode(PromptEditMode::Emacs) => {
                // Entering fresh drops a half-typed chord of either
                // kind (the C-x prefix or an explicit sequence).
                self.pending_prefix = false;
                self.resolver.reset();
                EventStatus::Handled
            }
            _ => EventStatus::Inapplicable,
        }
    }

    fn has_pending_sequence(&self) -> bool {
        self.resolver.is_pending() || self.pending_prefix
    }

    fn reset_pending_sequence(&mut self) {
        self.pending_prefix = false;
        self.resolver.reset();
    }

    fn flush_pending_sequence(&mut self) -> ReedlineEvent {
        // A lone `C-x` has no self-insert fallback: GNU's ctlx prefix
        // just dings on timeout.
        if self.pending_prefix {
            self.pending_prefix = false;
            return ReedlineEvent::None;
        }
        let pending = self.resolver.take_pending();
        if pending.is_empty() {
            return ReedlineEvent::None;
        }
        combine_events(self.flush_keys(&pending))
    }
}

impl Emacs {
    /// Emacs style input parsing constructor if you want to use custom keybindings
    pub fn new(keybindings: Keybindings) -> Self {
        Emacs {
            keybindings,
            pending_prefix: false,
            resolver: SequenceResolver::new(),
        }
    }

    /// The `C-x` prefix table — GNU `emacs_ctlx_keymap`
    /// (emacs_keymap.c:597+): `(`/`)`/`e` are the keyboard-macro
    /// commands, `C-u` is `rl_undo_command`, `C-g` aborts
    /// (`bind -p`: `"\C-x\C-g": abort`). Anything else is unbound — GNU
    /// dings. Shift counts as the bare character: the kitty protocol
    /// reports `(` as Shift-9.
    fn resolve_ctrl_x(modifiers: KeyModifiers, code: KeyCode) -> ReedlineEvent {
        match (modifiers, code) {
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('(')) => {
                ReedlineEvent::StartKbdMacro
            }
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(')')) => {
                ReedlineEvent::EndKbdMacro
            }
            (KeyModifiers::NONE, KeyCode::Char('e')) => ReedlineEvent::CallLastKbdMacro,
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => edit_bind(EditCommand::Undo),
            // `C-x C-g` — GNU `abort` (the ctlx keymap's own C-g slot).
            (KeyModifiers::CONTROL, KeyCode::Char('g')) => ReedlineEvent::Esc,
            // `C-x C-x` — GNU `exchange-point-and-mark`
            // (emacs_keymap.c:622).
            (KeyModifiers::CONTROL, KeyCode::Char('x')) => {
                edit_bind(EditCommand::ExchangePointAndMark)
            }
            _ => ReedlineEvent::None,
        }
    }

    /// Route one keypress through the sequence trie first, then the ordinary
    /// single-key table. An unbound plain character still self-inserts.
    fn dispatch_key(&mut self, modifiers: KeyModifiers, code: KeyCode) -> ReedlineEvent {
        let key = normalized_key_combination(modifiers, code);
        match self.resolver.resolve(&self.keybindings, key.clone()) {
            SequenceResolution::Pending => ReedlineEvent::None,
            SequenceResolution::Complete(event) => event,
            SequenceResolution::NoMatch { flush } => {
                if flush.is_empty() {
                    self.fallback_key(modifiers, code)
                } else {
                    let mut events = self.flush_keys(&flush);
                    let current = self.fallback_key(modifiers, code);
                    if current != ReedlineEvent::None {
                        events.push(current);
                    }
                    combine_events(events)
                }
            }
        }
    }

    /// The pre-sequence fallback for one key: GNU's `C-x` prefix keymap
    /// first (an explicit single-key binding of `C-x` still wins), then
    /// the ordinary table, then self-insert for a typeable character.
    fn fallback_key(&mut self, modifiers: KeyModifiers, code: KeyCode) -> ReedlineEvent {
        if modifiers == KeyModifiers::CONTROL
            && code == KeyCode::Char('x')
            && self.keybindings.find_binding(modifiers, code).is_none()
        {
            self.pending_prefix = true;
            return ReedlineEvent::Pending;
        }
        self.single_key_event(modifiers, code)
    }

    /// Resolve keys that were pending when a sequence was interrupted or timed
    /// out. A complete sequence inside the prefix wins; the rest is replayed
    /// through the single-key fallback, so `j` in emacs still inserts `j`.
    fn flush_keys(&self, keys: &[KeyCombination]) -> Vec<ReedlineEvent> {
        let mut events = Vec::new();
        let mut rest = keys;
        while let Some((len, event)) = self.keybindings.longest_sequence_prefix(rest) {
            events.push(event);
            rest = &rest[len..];
        }
        for key in rest {
            let event = self.single_key_event(key.modifier, key.key_code);
            if event != ReedlineEvent::None {
                events.push(event);
            }
        }
        events
    }

    /// The pre-sequence behaviour for one key: the table, then self-insert for
    /// a typeable character.
    fn single_key_event(&self, modifiers: KeyModifiers, code: KeyCode) -> ReedlineEvent {
        match (modifiers, code) {
            (modifier, KeyCode::Char(c)) => {
                // Note. The modifier can also be a combination of modifiers, for
                // example:
                //     KeyModifiers::CONTROL | KeyModifiers::ALT
                //     KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT
                //
                // Mixed modifiers are used by non american keyboards that have extra
                // keys like 'alt gr'. Keep this in mind if in the future there are
                // cases where an event is not being captured
                let c = match modifier {
                    KeyModifiers::NONE => c,
                    _ => c.to_ascii_lowercase(),
                };

                self.keybindings
                    .find_binding(modifier, KeyCode::Char(c))
                    .unwrap_or_else(|| {
                        if modifier == KeyModifiers::NONE
                            || modifier == KeyModifiers::SHIFT
                            || modifier == KeyModifiers::CONTROL | KeyModifiers::ALT
                            || modifier
                                == KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT
                        {
                            ReedlineEvent::Edit(vec![EditCommand::InsertChar(
                                if modifier == KeyModifiers::SHIFT {
                                    c.to_ascii_uppercase()
                                } else {
                                    c
                                },
                            )])
                        } else {
                            ReedlineEvent::None
                        }
                    })
            }
            _ => self
                .keybindings
                .find_binding(modifiers, code)
                .unwrap_or(ReedlineEvent::None),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use pretty_assertions::assert_eq;

    fn keycode(code: KeyCode, modifiers: KeyModifiers) -> ReedlineRawEvent {
        ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, modifiers))).unwrap()
    }

    fn combo(c: char) -> KeyCombination {
        KeyCombination {
            modifier: KeyModifiers::NONE,
            key_code: KeyCode::Char(c),
        }
    }

    fn emacs_with_sequence(keys: &[KeyCombination], event: ReedlineEvent) -> Emacs {
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding_sequence(keys, event);
        Emacs::new(keybindings)
    }

    #[test]
    fn switch_mode_event_accepts_only_emacs() {
        let mut emacs = Emacs::default();
        assert!(matches!(
            emacs.handle_mode_specific_event(ReedlineEvent::SwitchMode(PromptEditMode::Emacs)),
            EventStatus::Handled
        ));
        assert!(matches!(
            emacs.handle_mode_specific_event(ReedlineEvent::SwitchMode(PromptEditMode::Vi(
                crate::PromptViMode::Normal
            ))),
            EventStatus::Inapplicable
        ));
    }

    #[test]
    fn ctrl_l_leads_to_clear_screen_event() {
        let mut emacs = Emacs::default();
        let ctrl_l = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
        let result = emacs.parse_event(ctrl_l);

        assert_eq!(result, ReedlineEvent::ClearScreen);
    }

    #[test]
    fn overriding_default_keybindings_works() {
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('l'),
            ReedlineEvent::HistoryHintComplete,
        );

        let mut emacs = Emacs::new(keybindings);
        let ctrl_l = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
        let result = emacs.parse_event(ctrl_l);

        assert_eq!(result, ReedlineEvent::HistoryHintComplete);
    }

    #[test]
    fn inserting_character_works() {
        let mut emacs = Emacs::default();
        let l = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        let result = emacs.parse_event(l);

        assert_eq!(
            result,
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('l')])
        );
    }

    #[test]
    fn inserting_capital_character_works() {
        let mut emacs = Emacs::default();

        let uppercase_l = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::SHIFT,
        )))
        .unwrap();
        let result = emacs.parse_event(uppercase_l);

        assert_eq!(
            result,
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('L')])
        );
    }

    #[test]
    fn return_none_reedline_event_when_keybinding_is_not_found() {
        let keybindings = Keybindings::default();

        let mut emacs = Emacs::new(keybindings);
        let ctrl_l = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
        let result = emacs.parse_event(ctrl_l);

        assert_eq!(result, ReedlineEvent::None);
    }

    #[test]
    fn inserting_capital_character_for_non_ascii_remains_as_is() {
        let mut emacs = Emacs::default();

        let uppercase_l = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('😀'),
            KeyModifiers::SHIFT,
        )))
        .unwrap();
        let result = emacs.parse_event(uppercase_l);

        assert_eq!(
            result,
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('😀')])
        );
    }

    fn key(c: char, modifiers: KeyModifiers) -> ReedlineRawEvent {
        ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(KeyCode::Char(c), modifiers))).unwrap()
    }

    /// `C-x` arms the chord prefix (`ReedlineEvent::Pending`); the next
    /// key resolves against GNU's ctlx keymap — `(`/`)`/`e` are the
    /// macro commands, `C-u` is undo, anything else dings.
    #[test]
    fn ctrl_x_prefix_resolves_macro_chords() {
        let mut emacs = Emacs::default();
        let cx = || key('x', KeyModifiers::CONTROL);

        assert_eq!(emacs.parse_event(cx()), ReedlineEvent::Pending);
        assert_eq!(
            emacs.parse_event(key('(', KeyModifiers::NONE)),
            ReedlineEvent::StartKbdMacro
        );
        assert_eq!(emacs.parse_event(cx()), ReedlineEvent::Pending);
        assert_eq!(
            emacs.parse_event(key(')', KeyModifiers::NONE)),
            ReedlineEvent::EndKbdMacro
        );
        assert_eq!(emacs.parse_event(cx()), ReedlineEvent::Pending);
        assert_eq!(
            emacs.parse_event(key('e', KeyModifiers::NONE)),
            ReedlineEvent::CallLastKbdMacro
        );
        assert_eq!(emacs.parse_event(cx()), ReedlineEvent::Pending);
        assert_eq!(
            emacs.parse_event(key('u', KeyModifiers::CONTROL)),
            ReedlineEvent::Edit(vec![EditCommand::Undo])
        );
        // `C-x C-g` aborts — GNU binds the ctlx keymap's C-g slot to
        // `abort` (`bind -p`: `"\C-x\C-g": abort`).
        assert_eq!(emacs.parse_event(cx()), ReedlineEvent::Pending);
        assert_eq!(
            emacs.parse_event(key('g', KeyModifiers::CONTROL)),
            ReedlineEvent::Esc
        );
        // An unbound chord key dings (None) and clears the prefix —
        // the same key self-inserts on the next press.
        assert_eq!(emacs.parse_event(cx()), ReedlineEvent::Pending);
        assert_eq!(
            emacs.parse_event(key('q', KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert_eq!(
            emacs.parse_event(key('q', KeyModifiers::NONE)),
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('q')])
        );
    }

    /// An explicit binding on `C-x` wins over the prefix, matching a
    /// GNU rebind of the keymap slot.
    #[test]
    fn explicit_ctrl_x_binding_wins_over_prefix() {
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('x'),
            ReedlineEvent::ClearScreen,
        );
        let mut emacs = Emacs::new(keybindings);
        assert_eq!(
            emacs.parse_event(key('x', KeyModifiers::CONTROL)),
            ReedlineEvent::ClearScreen
        );
    }

    /// `M-0`..`M-9` parse to `DigitArgument`, `M--` to the sign —
    /// emacs_meta_keymap's digit-argument rows (emacs_keymap.c:366-380).
    #[test]
    fn alt_digits_parse_to_digit_argument() {
        let mut emacs = Emacs::default();
        for (c, d) in ('0'..='9').zip(0..=9) {
            assert_eq!(
                emacs.parse_event(key(c, KeyModifiers::ALT)),
                ReedlineEvent::DigitArgument(Some(d))
            );
        }
        assert_eq!(
            emacs.parse_event(key('-', KeyModifiers::ALT)),
            ReedlineEvent::DigitArgument(None)
        );
    }

    /// GNU `emacs_standard_keymap`/`emacs_meta_keymap` favorites:
    /// C-_ is `undo` (emacs_keymap.c:66), M-c is `capitalize-word`,
    /// M-u/M-l are `upcase-word`/`downcase-word`, and M-t is
    /// `transpose-words` (emacs_keymap.c:430-447).
    #[test]
    fn gnu_case_and_undo_bindings() {
        let kb = default_emacs_keybindings();
        assert_eq!(
            kb.find_binding(KeyModifiers::CONTROL, KeyCode::Char('_')),
            Some(ReedlineEvent::Edit(vec![EditCommand::Undo]))
        );
        // crossterm decodes the C-_ byte 0x1f as Control-'7'.
        assert_eq!(
            kb.find_binding(KeyModifiers::CONTROL, KeyCode::Char('7')),
            Some(ReedlineEvent::Edit(vec![EditCommand::Undo]))
        );
        assert_eq!(
            kb.find_binding(KeyModifiers::ALT, KeyCode::Char('c')),
            Some(ReedlineEvent::Edit(vec![EditCommand::CapitalizeWord]))
        );
        assert_eq!(
            kb.find_binding(KeyModifiers::ALT, KeyCode::Char('t')),
            Some(ReedlineEvent::Edit(vec![EditCommand::SwapWords]))
        );
    }

    /// GNU `\e<`/`\e>` — beginning-of-history and end-of-history
    /// (emacs_keymap.c:399/:401; `bind -p` prints `"\e<":
    /// beginning-of-history`). They are NOT buffer-edge moves, and the
    /// vi maps leave them unbound (`bind -m vi-insert -p` reports
    /// "beginning-of-history (not bound)").
    #[test]
    fn meta_angle_brackets_jump_history_edges() {
        let kb = default_emacs_keybindings();
        assert_eq!(
            kb.find_binding(KeyModifiers::ALT, KeyCode::Char('<')),
            Some(ReedlineEvent::FirstHistoryEntry)
        );
        assert_eq!(
            kb.find_binding(KeyModifiers::ALT, KeyCode::Char('>')),
            Some(ReedlineEvent::LastHistoryEntry)
        );
        // The kitty protocol spells the shifted glyph keys Shift+Alt+','/'.'.
        assert_eq!(
            kb.find_binding(KeyModifiers::SHIFT | KeyModifiers::ALT, KeyCode::Char(',')),
            Some(ReedlineEvent::FirstHistoryEntry)
        );
        assert_eq!(
            kb.find_binding(KeyModifiers::SHIFT | KeyModifiers::ALT, KeyCode::Char('.')),
            Some(ReedlineEvent::LastHistoryEntry)
        );
        // GNU's vi maps do not bind them; neither do ours.
        for kb in [
            crate::default_vi_insert_keybindings(),
            crate::default_vi_normal_keybindings(),
        ] {
            assert_eq!(
                kb.find_binding(KeyModifiers::ALT, KeyCode::Char('<')),
                None,
                "vi must leave M-< unbound like GNU"
            );
            assert_eq!(
                kb.find_binding(KeyModifiers::ALT, KeyCode::Char('>')),
                None,
                "vi must leave M-> unbound like GNU"
            );
        }
    }

    /// GNU `abort` (readline `rl_abort`): plain `C-g` is bound to it in
    /// the emacs standard keymap AND in vi-command (`bind -p`: `"\C-g":
    /// abort`). GNU readline defines no `redo` function at all, so the
    /// binding must not be `Edit(Redo)` — which also swallowed `C-g`
    /// during an incremental search instead of aborting it (T2 F3).
    #[test]
    fn ctrl_g_is_abort_not_redo() {
        let kb = default_emacs_keybindings();
        assert_eq!(
            kb.find_binding(KeyModifiers::CONTROL, KeyCode::Char('g')),
            Some(ReedlineEvent::Esc)
        );
        // The vi-command map carries the same abort slot; vi-insert
        // self-inserts C-g in GNU and stays unbound here.
        assert_eq!(
            crate::default_vi_normal_keybindings()
                .find_binding(KeyModifiers::CONTROL, KeyCode::Char('g')),
            Some(ReedlineEvent::Esc)
        );
        assert_eq!(
            crate::default_vi_insert_keybindings()
                .find_binding(KeyModifiers::CONTROL, KeyCode::Char('g')),
            None
        );
    }

    /// Every spelling of the `C-_` byte (0x1F) must parse to `Undo`:
    /// the kitty glyph, crossterm's VT decoding Ctrl+'7', and the
    /// ConPTY-measured Windows INPUT_RECORD spelling Control+Shift+'-'
    /// (folded at the `ReedlineRawEvent` seam). T2 F1.
    #[test]
    fn every_c_underscore_spelling_parses_to_undo() {
        let mut emacs = Emacs::default();
        let spellings = [
            (KeyCode::Char('_'), KeyModifiers::CONTROL),
            (KeyCode::Char('7'), KeyModifiers::CONTROL),
            (
                KeyCode::Char('-'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ];
        for (code, modifiers) in spellings {
            let raw = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, modifiers)))
                .expect("key accepted");
            assert_eq!(
                emacs.parse_event(raw),
                ReedlineEvent::Edit(vec![EditCommand::Undo]),
                "spelling {code:?} + {modifiers:?} must undo"
            );
        }
    }

    #[test]
    fn kill_line() {
        let mut emacs = Emacs::default();

        let ctrl_k = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
        let result = emacs.parse_event(ctrl_k);

        assert_eq!(result, ReedlineEvent::Edit(vec![EditCommand::KillLine]));
    }

    // --- multi-key sequences ---

    #[test]
    fn a_complete_sequence_fires_its_event() {
        let mut emacs = emacs_with_sequence(&[combo('j'), combo('j')], ReedlineEvent::Esc);

        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('j'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert!(emacs.has_pending_sequence());
        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('j'), KeyModifiers::NONE)),
            ReedlineEvent::Esc
        );
        assert!(!emacs.has_pending_sequence());
    }

    #[test]
    fn a_strict_prefix_waits_for_more_keys() {
        let mut emacs = emacs_with_sequence(
            &[combo('a'), combo('b'), combo('c')],
            ReedlineEvent::ClearScreen,
        );

        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('a'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('b'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert!(emacs.has_pending_sequence());
        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('c'), KeyModifiers::NONE)),
            ReedlineEvent::ClearScreen
        );
    }

    #[test]
    fn a_timeout_flushes_the_prefix_through_self_insert() {
        let mut emacs = emacs_with_sequence(&[combo('j'), combo('j')], ReedlineEvent::Esc);

        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('j'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert_eq!(
            emacs.flush_pending_sequence(),
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('j')])
        );
        assert!(!emacs.has_pending_sequence());
    }

    #[test]
    fn an_interrupted_prefix_flushes_then_handles_the_new_key() {
        let mut emacs = emacs_with_sequence(&[combo('j'), combo('j')], ReedlineEvent::Esc);

        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('j'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('x'), KeyModifiers::NONE)),
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Edit(vec![EditCommand::InsertChar('j')]),
                ReedlineEvent::Edit(vec![EditCommand::InsertChar('x')]),
            ])
        );
        assert!(!emacs.has_pending_sequence());
    }

    #[test]
    fn a_key_that_is_both_binding_and_prefix_waits() {
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(KeyModifiers::NONE, KeyCode::Char('j'), ReedlineEvent::Down);
        keybindings.add_binding_sequence(&[combo('j'), combo('j')], ReedlineEvent::Esc);
        let mut emacs = Emacs::new(keybindings);

        // `j` alone is bound, but `jj` continues the sequence, so it waits.
        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('j'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('j'), KeyModifiers::NONE)),
            ReedlineEvent::Esc
        );

        // On its own, the shorter binding is the timeout fallback.
        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('j'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert_eq!(emacs.flush_pending_sequence(), ReedlineEvent::Down);
    }

    #[test]
    fn a_flushed_binding_then_the_new_key_keep_their_order() {
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(KeyModifiers::NONE, KeyCode::Char('j'), ReedlineEvent::Down);
        keybindings.add_binding_sequence(&[combo('j'), combo('j')], ReedlineEvent::Esc);
        let mut emacs = Emacs::new(keybindings);

        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('j'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert_eq!(
            emacs.parse_event(keycode(KeyCode::Char('x'), KeyModifiers::NONE)),
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Down,
                ReedlineEvent::Edit(vec![EditCommand::InsertChar('x')]),
            ])
        );
    }
}
