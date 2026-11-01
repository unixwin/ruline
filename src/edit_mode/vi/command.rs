use super::{motion::Motion, parser::ReedlineOption, ViMode};
use crate::enums::{
    TextObject, TextObjectBracket, TextObjectQuote, TextObjectScope, TextObjectType,
};
use crate::{Direction, EditCommand, Granularity, MotionTarget, ReedlineEvent, Vi};
use std::iter::Peekable;

pub(super) fn parse_command<'iter, I>(mode: ViMode, input: &mut Peekable<I>) -> Option<Command>
where
    I: Iterator<Item = &'iter char>,
{
    match input.peek() {
        Some('d') => {
            let _ = input.next();
            text_object_to_command(input, Command::Delete, |text_object| {
                Command::DeleteTextObject { text_object }
            })
        }
        // Checking for "yi(" or "yiw" etc.
        Some('y') => {
            let _ = input.next();
            text_object_to_command(input, Command::Yank, |text_object| {
                Command::YankTextObject { text_object }
            })
        }
        Some('p') => {
            let _ = input.next();
            Some(Command::PasteAfter)
        }
        Some('P') => {
            let _ = input.next();
            Some(Command::PasteBefore)
        }
        Some('i') => {
            let _ = input.next();
            Some(Command::EnterViInsert)
        }
        Some('a') => {
            let _ = input.next();
            Some(Command::EnterViAppend)
        }
        Some('u') if mode == ViMode::Normal => {
            let _ = input.next();
            Some(Command::Undo)
        }
        // Checking for "ci(" or "ciw" etc.
        Some('c') => {
            let _ = input.next();
            text_object_to_command(input, Command::Change, |text_object| {
                Command::ChangeTextObject { text_object }
            })
        }
        Some('x') => {
            let _ = input.next();
            Some(Command::DeleteChar)
        }
        Some('X') => {
            let _ = input.next();
            Some(Command::DeleteCharBackward)
        }
        Some('r') => {
            let _ = input.next();
            input
                .next()
                .map(|c| Command::ReplaceChar(*c))
                .or(Some(Command::Incomplete))
        }
        Some('s') => {
            let _ = input.next();
            Some(Command::SubstituteCharWithInsert)
        }
        // GNU `rl_vi_search` (vi_mode.c:365): `?` searches forward
        // (newer entries), `/` backward (older) — both non-incremental
        // under `SF_PATTERN`, not isearch.
        Some('?') if mode == ViMode::Normal => {
            let _ = input.next();
            Some(Command::ViSearch {
                direction: Direction::Forward,
                prompt_char: '?',
            })
        }
        Some('/') if mode == ViMode::Normal => {
            let _ = input.next();
            Some(Command::ViSearch {
                direction: Direction::Backward,
                prompt_char: '/',
            })
        }
        // GNU `rl_vi_search_again` (vi_mode.c:348): `n` resumes the
        // saved search backward, `N` forward.
        Some('n') if mode == ViMode::Normal => {
            let _ = input.next();
            Some(Command::SearchAgain {
                direction: Direction::Backward,
            })
        }
        Some('N') if mode == ViMode::Normal => {
            let _ = input.next();
            Some(Command::SearchAgain {
                direction: Direction::Forward,
            })
        }
        // GNU `U` — `rl_revert_line` (vi_keymap.c:129).
        Some('U') if mode == ViMode::Normal => {
            let _ = input.next();
            Some(Command::RevertLine)
        }
        // GNU `rl_vi_yank_arg` (vi_mode.c:326, `_`): the count-th arg
        // of the previous history line, or its last arg without one.
        Some('_') if mode == ViMode::Normal => {
            let _ = input.next();
            Some(Command::YankArg)
        }
        // GNU `#` in the vi map — `rl_insert_comment`
        // (vi_keymap.c:73): comment the line out and submit it.
        Some('#') if mode == ViMode::Normal => {
            let _ = input.next();
            Some(Command::InsertComment)
        }
        Some('C') => {
            let _ = input.next();
            Some(Command::ChangeToLineEnd)
        }
        Some('D') => {
            let _ = input.next();
            Some(Command::DeleteToEnd)
        }
        Some('I') => {
            let _ = input.next();
            Some(Command::PrependToStart)
        }
        Some('A') => {
            let _ = input.next();
            Some(Command::AppendToEnd)
        }
        Some('S') => {
            let _ = input.next();
            Some(Command::RewriteCurrentLine)
        }
        Some('~') => {
            let _ = input.next();
            Some(Command::Switchcase)
        }
        Some('.') => {
            let _ = input.next();
            Some(Command::RepeatLastAction)
        }
        // `o` swaps cursor and anchor in ruline's vi Visual extension.
        // In Normal mode GNU's vi-command keymap leaves `o`/`O` UNBOUND
        // (`bind -m vi-command -p` prints no `o`/`O` row; vim's
        // open-below/above does not exist in readline), so they fall to
        // the `_ => None` arm and the sequence parses as invalid — a
        // no-op, with the NEXT key starting a fresh parse.
        Some('o' | 'O') if mode == ViMode::Visual => {
            let _ = input.next();
            Some(Command::SwapCursorAndAnchor)
        }
        Some(&&u @ ('u' | 'U')) if mode == ViMode::Visual => {
            let _ = input.next();
            if u.is_ascii_lowercase() {
                Some(Command::Lowercase)
            } else {
                Some(Command::Uppercase)
            }
        }
        _ => None,
    }
}

pub(super) fn text_object_to_command<'iter, I, F>(
    input: &mut Peekable<I>,
    incomplete_command: Command,
    command_generator: F,
) -> Option<Command>
where
    I: Iterator<Item = &'iter char>,
    F: FnOnce(TextObject) -> Command,
{
    let scope = match input.peek() {
        Some('i') => TextObjectScope::Inner,
        Some('a') => TextObjectScope::Around,
        _ => return Some(incomplete_command),
    };
    let _ = input.next();
    input
        .next()
        .and_then(|c| char_to_text_object(*c, scope))
        .map(command_generator)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    Incomplete,
    Delete,
    DeleteChar,
    DeleteCharBackward,
    ReplaceChar(char),
    SubstituteCharWithInsert,
    PasteAfter,
    PasteBefore,
    EnterViAppend,
    EnterViInsert,
    Undo,
    ChangeToLineEnd,
    DeleteToEnd,
    AppendToEnd,
    PrependToStart,
    RewriteCurrentLine,
    Change,
    /// vi `/`/`?` — GNU `rl_vi_search`, a non-incremental glob search.
    ViSearch {
        direction: Direction,
        prompt_char: char,
    },
    /// vi `n`/`N` — GNU `rl_vi_search_again`.
    SearchAgain {
        direction: Direction,
    },
    /// vi `U` — GNU `rl_revert_line` (vi_keymap.c:129).
    RevertLine,
    /// vi `_` — GNU `rl_vi_yank_arg` (vi_mode.c:326).
    YankArg,
    /// vi `#` — GNU `rl_insert_comment` (vi_keymap.c:73).
    InsertComment,
    Lowercase,
    Uppercase,
    Switchcase,
    RepeatLastAction,
    Yank,
    ChangeTextObject {
        text_object: TextObject,
    },
    YankTextObject {
        text_object: TextObject,
    },
    DeleteTextObject {
        text_object: TextObject,
    },
    SwapCursorAndAnchor,
}

impl Command {
    pub fn whole_line_char(&self) -> Option<char> {
        match self {
            Command::Delete => Some('d'),
            Command::Change => Some('c'),
            Command::Yank => Some('y'),
            _ => None,
        }
    }

    pub fn requires_motion(&self) -> bool {
        matches!(self, Command::Delete | Command::Change | Command::Yank)
    }

    pub fn to_reedline(&self, vi_state: &mut Vi) -> Vec<ReedlineOption> {
        match self {
            Self::EnterViInsert => vec![ReedlineOption::Event(ReedlineEvent::Repaint)],
            Self::EnterViAppend => vec![ReedlineOption::Edit(EditCommand::MoveRight {
                select: false,
            })],
            Self::PasteAfter => vec![ReedlineOption::Edit(EditCommand::PasteCutBufferAfter)],
            Self::PasteBefore => vec![ReedlineOption::Edit(EditCommand::PasteCutBufferBefore)],
            Self::Undo => vec![ReedlineOption::Edit(EditCommand::Undo)],
            Self::ChangeToLineEnd => vec![ReedlineOption::Edit(EditCommand::ClearToLineEnd)],
            Self::DeleteToEnd => vec![ReedlineOption::Edit(EditCommand::CutToLineEnd)],
            Self::AppendToEnd => vec![ReedlineOption::Edit(EditCommand::MoveToLineEnd {
                select: false,
            })],
            Self::PrependToStart => vec![ReedlineOption::Edit(EditCommand::MoveToLineStart {
                select: false,
            })],
            Self::DeleteCharBackward => {
                if vi_state.mode == ViMode::Visual {
                    vec![ReedlineOption::Edit(EditCommand::CutSelection {
                        granularity: Granularity::LineWise,
                    })]
                } else {
                    vec![ReedlineOption::Edit(EditCommand::CutCharLeft)]
                }
            }
            // `S` ≡ `cc` (vim): change the whole line, keeping the blank line
            // for insert mode and filling the register linewise.
            Self::RewriteCurrentLine => vec![ReedlineOption::Edit(EditCommand::Change {
                target: MotionTarget::LineEdge(Direction::Forward),
                granularity: Granularity::LineWise,
            })],
            Self::DeleteChar => {
                if vi_state.mode == ViMode::Visual {
                    vec![ReedlineOption::Edit(EditCommand::CutSelection {
                        granularity: Granularity::CharWise,
                    })]
                } else {
                    vec![ReedlineOption::Edit(EditCommand::CutChar)]
                }
            }
            Self::ReplaceChar(c) => {
                vec![ReedlineOption::Edit(EditCommand::ReplaceChar(*c))]
            }
            Self::SubstituteCharWithInsert => {
                if vi_state.mode == ViMode::Visual {
                    vec![ReedlineOption::Edit(EditCommand::CutSelection {
                        granularity: Granularity::CharWise,
                    })]
                } else {
                    vec![ReedlineOption::Edit(EditCommand::CutChar)]
                }
            }
            Self::ViSearch {
                direction,
                prompt_char,
            } => vec![ReedlineOption::Event(ReedlineEvent::NonIncrementalSearch {
                direction: *direction,
                prompt_char: *prompt_char,
                // GNU sets `SF_PATTERN` for `?`/`/` read from vi command
                // mode (search.c:236-238).
                pattern: true,
            })],
            Self::SearchAgain { direction } => {
                vec![ReedlineOption::Event(
                    ReedlineEvent::NonIncrementalSearchAgain {
                        direction: *direction,
                    },
                )]
            }
            Self::RevertLine => vec![ReedlineOption::Edit(EditCommand::RevertLine)],
            // The arg form needs the multiplier, which `to_reedline`
            // never sees; the parser's `to_reedline_event` lowers `_`.
            // Bare `_` yanks the last argument (`rl_yank_nth_arg '$'`,
            // vi_mode.c:334).
            Self::YankArg => vec![ReedlineOption::Event(ReedlineEvent::YankLastArg {
                count: None,
            })],
            Self::InsertComment => vec![
                ReedlineOption::Edit(EditCommand::MoveToLineStart { select: false }),
                ReedlineOption::Edit(EditCommand::InsertString("# ".to_string())),
                ReedlineOption::Event(ReedlineEvent::Submit),
            ],
            Self::Lowercase => {
                vec![ReedlineOption::Edit(EditCommand::LowercaseSelection)]
            }
            Self::Uppercase => {
                vec![ReedlineOption::Edit(EditCommand::UppercaseSelection)]
            }
            Self::Switchcase => {
                if vi_state.mode == ViMode::Visual {
                    vec![ReedlineOption::Edit(EditCommand::SwitchcaseSelection)]
                } else {
                    vec![ReedlineOption::Edit(EditCommand::SwitchcaseChar)]
                }
            }
            // Whenever a motion is required to finish the command we must be in visual mode
            Self::Delete | Self::Change => vec![ReedlineOption::Edit(EditCommand::CutSelection {
                granularity: Granularity::CharWise,
            })],
            Self::Yank => vec![ReedlineOption::Edit(EditCommand::CopySelection)],
            Self::Incomplete => vec![ReedlineOption::Incomplete],
            Self::RepeatLastAction => match &vi_state.previous {
                Some(event) => vec![ReedlineOption::Event(event.clone())],
                None => vec![],
            },
            Self::ChangeTextObject { text_object } => {
                vec![ReedlineOption::Edit(EditCommand::CutTextObject {
                    text_object: *text_object,
                })]
            }
            Self::YankTextObject { text_object } => {
                vec![ReedlineOption::Edit(EditCommand::CopyTextObject {
                    text_object: *text_object,
                })]
            }
            Self::DeleteTextObject { text_object } => {
                vec![ReedlineOption::Edit(EditCommand::CutTextObject {
                    text_object: *text_object,
                })]
            }
            Self::SwapCursorAndAnchor => {
                vec![ReedlineOption::Edit(EditCommand::SwapCursorAndAnchor)]
            }
        }
    }

    pub fn to_reedline_with_motion(
        &self,
        motion: &Motion,
        vi_state: &mut Vi,
    ) -> Option<Vec<ReedlineOption>> {
        match self {
            Self::Delete => match motion {
                // `dd` — the whole current line, linewise.
                Motion::Line => Some(vec![ReedlineOption::Edit(EditCommand::Cut {
                    target: MotionTarget::LineEdge(Direction::Forward),
                    granularity: Granularity::LineWise,
                })]),
                // Word and line-edge motions lower through one parameterized verb:
                // cut to the motion's target (`operator_span` makes `e`/`E` inclusive).
                Motion::NextWord
                | Motion::NextBigWord
                | Motion::NextWordEnd
                | Motion::NextBigWordEnd
                | Motion::PreviousWord
                | Motion::PreviousBigWord
                | Motion::Start
                | Motion::End
                | Motion::MatchBrackets
                | Motion::Column => motion.target().map(|target| {
                    vec![ReedlineOption::Edit(EditCommand::Cut {
                        target,
                        granularity: Granularity::CharWise,
                    })]
                }),
                Motion::RightUntil(_)
                | Motion::RightBefore(_)
                | Motion::LeftUntil(_)
                | Motion::LeftBefore(_) => motion.target().map(|target| {
                    vi_state.last_char_search = Some(target);
                    vec![ReedlineOption::Edit(EditCommand::Cut {
                        target,
                        granularity: Granularity::CharWise,
                    })]
                }),
                Motion::NonBlankStart => Some(vec![ReedlineOption::Edit(
                    EditCommand::CutFromLineNonBlankStart,
                )]),
                Motion::Left => Some(vec![ReedlineOption::Edit(EditCommand::Backspace)]),
                Motion::Right => Some(vec![ReedlineOption::Edit(EditCommand::Delete)]),
                // `dj`/`dk`/`dgg`/`dG` — whole lines to the adjacent line or the
                // buffer edge, linewise. The targets + the LineWise snap (incl.
                // the buffer-end `\n` fixup) reproduce the dedicated commands.
                Motion::Down | Motion::Up | Motion::FirstLine | Motion::LastLine => {
                    motion.target().map(|target| {
                        vec![ReedlineOption::Edit(EditCommand::Cut {
                            target,
                            granularity: Granularity::LineWise,
                        })]
                    })
                }
                Motion::ReplayCharSearch => vi_state.last_char_search.map(|target| {
                    vec![ReedlineOption::Edit(EditCommand::Cut {
                        target,
                        granularity: Granularity::CharWise,
                    })]
                }),
                Motion::ReverseCharSearch => vi_state.last_char_search.map(|target| {
                    vec![ReedlineOption::Edit(EditCommand::Cut {
                        target: target.reversed(),
                        granularity: Granularity::CharWise,
                    })]
                }),
            },
            Self::Change => {
                let op = match motion {
                    // `cc` — change the whole line: its content is cut (the
                    // blank line remains for insert mode) and the register is
                    // filled linewise, so `p` after `cc` pastes as a line.
                    Motion::Line => Some(vec![ReedlineOption::Edit(EditCommand::Change {
                        target: MotionTarget::LineEdge(Direction::Forward),
                        granularity: Granularity::LineWise,
                    })]),
                    // `cw`/`cW` act like `ce`/`cE`: change to the word *end*, not the
                    // next word's start. Other word and line-edge motions (`c$`/`c0`)
                    // use their own target.
                    Motion::NextWord
                    | Motion::NextBigWord
                    | Motion::NextWordEnd
                    | Motion::NextBigWordEnd
                    | Motion::PreviousWord
                    | Motion::PreviousBigWord
                    | Motion::Start
                    | Motion::End
                    | Motion::MatchBrackets
                    | Motion::Column => {
                        let target = match motion {
                            Motion::NextWord => Motion::NextWordEnd.target(),
                            Motion::NextBigWord => Motion::NextBigWordEnd.target(),
                            other => other.target(),
                        };
                        target.map(|target| {
                            vec![ReedlineOption::Edit(EditCommand::Cut {
                                target,
                                granularity: Granularity::CharWise,
                            })]
                        })
                    }
                    Motion::RightUntil(_)
                    | Motion::RightBefore(_)
                    | Motion::LeftUntil(_)
                    | Motion::LeftBefore(_) => motion.target().map(|target| {
                        vi_state.last_char_search = Some(target);
                        vec![ReedlineOption::Edit(EditCommand::Cut {
                            target,
                            granularity: Granularity::CharWise,
                        })]
                    }),
                    Motion::NonBlankStart => Some(vec![ReedlineOption::Edit(
                        EditCommand::CutFromLineNonBlankStart,
                    )]),
                    Motion::Left => Some(vec![ReedlineOption::Edit(EditCommand::Backspace)]),
                    Motion::Right => Some(vec![ReedlineOption::Edit(EditCommand::Delete)]),
                    // `cj`/`ck`/`cgg`/`cG` — linewise change: the spanned lines'
                    // content is cut, one blank line remains, and insert mode
                    // re-enters on it (`Change`'s LineWise snap keeps the
                    // terminators where `Cut`'s consumes them).
                    Motion::Down | Motion::Up | Motion::FirstLine | Motion::LastLine => {
                        motion.target().map(|target| {
                            vec![ReedlineOption::Edit(EditCommand::Change {
                                target,
                                granularity: Granularity::LineWise,
                            })]
                        })
                    }
                    Motion::ReplayCharSearch => vi_state.last_char_search.map(|target| {
                        vec![ReedlineOption::Edit(EditCommand::Cut {
                            target,
                            granularity: Granularity::CharWise,
                        })]
                    }),
                    Motion::ReverseCharSearch => vi_state.last_char_search.map(|target| {
                        vec![ReedlineOption::Edit(EditCommand::Cut {
                            target: target.reversed(),
                            granularity: Granularity::CharWise,
                        })]
                    }),
                };
                // Semihack: Append `Repaint` to ensure the mode change gets displayed
                op.map(|mut vec| {
                    vec.push(ReedlineOption::Event(ReedlineEvent::Repaint));
                    vec
                })
            }
            Self::Yank => match motion {
                // `yy` — the whole current line, linewise.
                Motion::Line => Some(vec![ReedlineOption::Edit(EditCommand::Copy {
                    target: MotionTarget::LineEdge(Direction::Forward),
                    granularity: Granularity::LineWise,
                })]),
                Motion::NextWord
                | Motion::NextBigWord
                | Motion::NextWordEnd
                | Motion::NextBigWordEnd
                | Motion::PreviousWord
                | Motion::PreviousBigWord
                | Motion::Start
                | Motion::End
                | Motion::MatchBrackets
                | Motion::Column => motion.target().map(|target| {
                    vec![ReedlineOption::Edit(EditCommand::Copy {
                        target,
                        granularity: Granularity::CharWise,
                    })]
                }),
                Motion::RightUntil(_)
                | Motion::RightBefore(_)
                | Motion::LeftUntil(_)
                | Motion::LeftBefore(_) => motion.target().map(|target| {
                    vi_state.last_char_search = Some(target);
                    vec![ReedlineOption::Edit(EditCommand::Copy {
                        target,
                        granularity: Granularity::CharWise,
                    })]
                }),
                Motion::NonBlankStart => Some(vec![ReedlineOption::Edit(
                    EditCommand::CopyFromLineNonBlankStart,
                )]),
                Motion::Left => Some(vec![ReedlineOption::Edit(EditCommand::CopyLeft)]),
                Motion::Right => Some(vec![ReedlineOption::Edit(EditCommand::CopyRight)]),
                // `yj`/`yk`/`ygg`/`yG` — whole lines to the adjacent line or
                // the buffer edge, linewise.
                Motion::Down | Motion::Up | Motion::FirstLine | Motion::LastLine => {
                    motion.target().map(|target| {
                        vec![ReedlineOption::Edit(EditCommand::Copy {
                            target,
                            granularity: Granularity::LineWise,
                        })]
                    })
                }
                Motion::ReplayCharSearch => vi_state.last_char_search.map(|target| {
                    vec![ReedlineOption::Edit(EditCommand::Copy {
                        target,
                        granularity: Granularity::CharWise,
                    })]
                }),
                Motion::ReverseCharSearch => vi_state.last_char_search.map(|target| {
                    vec![ReedlineOption::Edit(EditCommand::Copy {
                        target: target.reversed(),
                        granularity: Granularity::CharWise,
                    })]
                }),
            },
            _ => None,
        }
    }
}

fn char_to_text_object(c: char, scope: TextObjectScope) -> Option<TextObject> {
    match c {
        'b' => Some(TextObject {
            scope,
            object_type: TextObjectType::Brackets(TextObjectBracket::All),
            check_next: true,
        }),
        'q' => Some(TextObject {
            scope,
            object_type: TextObjectType::Quotes(TextObjectQuote::All),
            check_next: true,
        }),
        '$' => Some(TextObject {
            scope,
            object_type: TextObjectType::Pair {
                left: '$',
                right: '$',
            },
            check_next: true,
        }),
        _ => {
            let tot = TextObjectType::from_char(c)?;

            if matches!(tot, TextObjectType::Pair { .. }) {
                return None;
            }

            Some(TextObject {
                scope,
                object_type: tot,
                check_next: true,
            })
        }
    }
}
