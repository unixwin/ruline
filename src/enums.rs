use crate::core_editor::EditCommandStatus;
use crate::PromptEditMode;
use crossterm::event::{Event, KeyEvent, KeyEventKind};
use serde::{Deserialize, Serialize};
use strum::{EnumDiscriminants, EnumIter, EnumString, VariantArray};

/// Which mouse button was pressed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    /// Left mouse button
    #[default]
    Left,
    /// Right mouse button
    Right,
    /// Middle mouse button
    Middle,
}

impl From<crossterm::event::MouseButton> for MouseButton {
    fn from(button: crossterm::event::MouseButton) -> Self {
        match button {
            crossterm::event::MouseButton::Left => Self::Left,
            crossterm::event::MouseButton::Right => Self::Right,
            crossterm::event::MouseButton::Middle => Self::Middle,
        }
    }
}

/// Valid ways how `Reedline::read_line()` can return
#[non_exhaustive]
#[derive(Debug)]
pub enum Signal {
    /// Entry succeeded with the provided content
    Success(String),
    /// Entry was aborted with `Ctrl+C`
    CtrlC, // Interrupt current editing
    /// Abort with `Ctrl+D` signalling `EOF` or abort of a whole interactive session
    CtrlD, // End terminal session

    /// A custom, uninterpreted payload passed back to the host application.
    ///
    /// This signal is triggered by a [`ReedlineEvent::ExecuteHostCommand`].
    /// The contained string is a "passthrough" value that Reedline does not
    /// inspect or modify; it is up to the caller to define the protocol
    /// and execution logic for this payload.
    HostCommand(String),

    /// An external signal requested that `read_line()` return.
    /// Contains the current buffer contents at the time of interruption.
    ExternalBreak(String),
}

/// Scope of text object operation ("i" inner or "a" around)
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum TextObjectScope {
    /// Just the text object itself
    Inner,
    /// Expanded to include surrounding based on object type
    Around,
}

/// Text object quote types
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum TextObjectQuote {
    /// '
    SingleQuote,
    /// "
    DoubleQuote,
    /// \`
    Tick,
    /// ', ", \`
    All,
}

/// Text object bracket types
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum TextObjectBracket {
    /// (, )
    Parenthesis,
    /// \[, ]
    SquareBracket,
    /// {, }
    CurlyBracket,
    /// <, >
    AngleBracket,
    /// (, ), \[, ], {, }, <, >
    All,
}

/// Type of text object to operate on
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum TextObjectType {
    /// word (delimited by non-alphanumeric characters)
    Word,
    /// WORD (delimited only by whitespace)
    BigWord,
    /// Brackets pairs (`(`, `)`, `[`, `]`, `{`, `}`, `<`, `>`)
    Brackets(TextObjectBracket),
    /// Quotes pairs (`"`, `'`, `\``)
    Quotes(TextObjectQuote),
    /// Custom pair
    Pair {
        /// left character of the pair
        left: char,
        /// right character of the pair
        right: char,
    },
}

impl TextObjectType {
    /// Convert a character to its corresponding [`TextObjectType`](Self)
    pub fn from_char(ch: char) -> Option<Self> {
        match ch {
            'w' => Some(Self::Word),
            'W' => Some(Self::BigWord),
            '(' | ')' => Some(Self::Brackets(TextObjectBracket::Parenthesis)),
            '[' | ']' => Some(Self::Brackets(TextObjectBracket::SquareBracket)),
            '{' | '}' => Some(Self::Brackets(TextObjectBracket::CurlyBracket)),
            '<' | '>' => Some(Self::Brackets(TextObjectBracket::AngleBracket)),
            '"' => Some(Self::Quotes(TextObjectQuote::DoubleQuote)),
            '\'' => Some(Self::Quotes(TextObjectQuote::SingleQuote)),
            '`' => Some(Self::Quotes(TextObjectQuote::Tick)),
            _ if !ch.is_ascii_control() && ch.is_ascii() => Some(Self::Pair {
                left: ch,
                right: ch,
            }),
            _ => None,
        }
    }

    /// Returns the pair of character if any is associated
    pub fn to_chars(self) -> Option<(char, char)> {
        match self {
            Self::Word
            | Self::BigWord
            | Self::Brackets(TextObjectBracket::All)
            | Self::Quotes(TextObjectQuote::All) => None,
            Self::Brackets(TextObjectBracket::Parenthesis) => Some(('(', ')')),
            Self::Brackets(TextObjectBracket::SquareBracket) => Some(('[', ']')),
            Self::Brackets(TextObjectBracket::CurlyBracket) => Some(('{', '}')),
            Self::Brackets(TextObjectBracket::AngleBracket) => Some(('<', '>')),
            Self::Quotes(TextObjectQuote::SingleQuote) => Some(('\'', '\'')),
            Self::Quotes(TextObjectQuote::DoubleQuote) => Some(('"', '"')),
            Self::Quotes(TextObjectQuote::Tick) => Some(('`', '`')),
            Self::Pair { left, right } => Some((left, right)),
        }
    }
}

/// Text objects that can be operated on with vim-style commands
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct TextObject {
    /// Whether to include surrounding context
    pub scope: TextObjectScope,
    /// The type of text object
    pub object_type: TextObjectType,
    /// check the next text object if no text object found around the cursor
    pub check_next: bool,
}

impl Default for TextObject {
    fn default() -> Self {
        Self {
            scope: TextObjectScope::Inner,
            object_type: TextObjectType::Word,
            check_next: true,
        }
    }
}

/// Direction a cursor motion travels through the buffer.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Toward the end of the buffer (right / `w` / `$`).
    Forward,
    /// Toward the start of the buffer (left / `b` / `0`).
    Backward,
}

impl Direction {
    pub(crate) fn reversed(self) -> Self {
        match self {
            Direction::Forward => Direction::Backward,
            Direction::Backward => Direction::Forward,
        }
    }
}

/// Which "word" notion a word motion uses.
///
/// The flavors differ only in which character-class transitions count as a
/// boundary; see the classifier in `core_editor::word`.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum WordKind {
    /// `w`/`b`/`e` — a boundary at any character-class change.
    Word,
    /// `W`/`B`/`E` — a boundary only at whitespace/line-ending, so a run like
    /// `foo.bar` is one WORD.
    LongWord,
    /// Emacs `M-f`/`M-b` — Unicode (UAX-29) word segmentation, so e.g. `can't`
    /// and `3.14` stay single words. The one flavor that isn't a thin char-class
    /// predicate; see `locate_word`. Holding those together turns on the
    /// characters flanking a `'` or a `.` rather than on their class, thus it
    /// stays a separate scan instead of collapsing onto a boundary predicate.
    Unicode,
}

/// Which edge of a word a motion lands on.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum WordEdge {
    /// First character of the word — `w`/`W` (forward), `b`/`B` (backward).
    Start,
    /// Last character of the word, inclusive — `e`/`E`.
    End,
}

/// Where a character-search motion stops relative to the found character.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum FindStop {
    /// Land on the found character — vi `f`/`F`.
    On,
    /// Land just before it — vi `t`/`T`.
    Before,
}

/// Granularity of an operator and of the register it fills: inline characters or
/// whole lines.
///
/// `LineWise` (vi `dd`/`yy`/`V`) operates on complete lines and pastes on a new
/// line below/above; `CharWise` is the default inline behavior. Carried on the
/// operator and stored on the cut buffer, so paste knows which to do.
///
/// Non-exhaustive: granularities may be added (e.g. block-wise), so external
/// matches need a wildcard arm.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Granularity {
    /// Inline at the cursor — the default.
    #[default]
    CharWise,
    /// Whole lines, pasted below/above.
    LineWise,
}

/// A human-readable, parameterized motion target — the public vocabulary every
/// cursor motion lowers from.
///
/// `Move`/`Extend`/`Cut`/`Copy`/`Erase` over a `MotionTarget` are the
/// going-forward motion API. They resolve through the private `resolve_motion`
/// and apply to the cursor, both free to change. Mode differences (vi vs emacs
/// vs helix word rules) are carried as *data* here (e.g. [`WordKind`]) rather
/// than as separate commands.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum MotionTarget {
    /// One grapheme in `direction` — `MoveLeft`/`MoveRight`.
    Grapheme(Direction),
    /// A word edge — vi `w`/`W`/`e`/`E`/`b`/`B`.
    Word {
        /// Small word vs big WORD.
        kind: WordKind,
        /// First vs last character of the word.
        edge: WordEdge,
        /// Travel direction.
        direction: Direction,
    },
    /// Logical line edge: `Backward` = line start (`0`), `Forward` = line end (`$`).
    LineEdge(Direction),
    /// First non-whitespace character on the current line (helix `gs`). A blank
    /// line has none, so the motion stays put.
    LineStartNonBlank,
    /// Whole-buffer edge: `Backward` = start (`gg`), `Forward` = end (`G`).
    BufferEdge(Direction),
    /// The adjacent logical line: `Forward` = line below (`j`), `Backward` =
    /// line above (`k`). Used by the linewise operators (`dj`/`dk`); the head
    /// lands on the adjacent line so a `LineWise` span covers both lines.
    Line(Direction),
    /// Character search — vi `f`/`F`/`t`/`T`.
    Find {
        /// The character to search for.
        ch: char,
        /// Travel direction.
        direction: Direction,
        /// Land on the character vs just before it.
        stop: FindStop,
    },
    /// The bracket matching the one under the cursor — vi `%`, GNU
    /// `rl_vi_match` (vi_mode.c:1951). When the cursor is not on a
    /// bracket, GNU scans forward for the first bracket on the line
    /// and matches that instead; no bracket at all is a no-op. The
    /// pairs are `()`, `[]`, `{}` (GNU's `rl_vi_bracktype`,
    /// vi_mode.c:1926 — `<>` is not a vi pair).
    MatchingBracket,
    /// The `n`th grapheme of the buffer, 1-based — vi `|`, GNU
    /// `rl_vi_column` (vi_mode.c:1053), which moves `count - 1`
    /// characters from buffer start. Past the buffer end it lands on
    /// the last grapheme (GNU `rl_end_of_line`); `0` degenerates to
    /// the buffer start.
    Column(usize),
    /// A byte position, clamped into the buffer — measured from the buffer
    /// start, not a displacement from the cursor.
    Position(usize),
}

impl MotionTarget {
    /// The `,`-style reverse: flip a [`Find`](MotionTarget::Find)'s direction.
    ///
    /// Only `Find` is reversible — every other target passes through unchanged,
    /// because a reversed word/line edge is a *different* motion, not the same
    /// motion the other way (e.g. backward word-end is `ge`, not `e` flipped).
    pub(crate) fn reversed(self) -> Self {
        match self {
            MotionTarget::Find {
                ch,
                direction,
                stop,
            } => MotionTarget::Find {
                ch,
                direction: direction.reversed(),
                stop,
            },
            other => other,
        }
    }

    /// Which way the motion travels, or `None` for a target that names a
    /// destination rather than a displacement.
    pub(crate) fn direction(self) -> Option<Direction> {
        match self {
            MotionTarget::Grapheme(direction)
            | MotionTarget::Word { direction, .. }
            | MotionTarget::LineEdge(direction)
            | MotionTarget::BufferEdge(direction)
            | MotionTarget::Line(direction)
            | MotionTarget::Find { direction, .. } => Some(direction),
            // Destination-shaped targets go here. Where they lie depends on the
            // cursor, so callers resolve first and compare after.
            MotionTarget::Position(_) => None,
            MotionTarget::LineStartNonBlank => None,
            MotionTarget::MatchingBracket => None,
            MotionTarget::Column(_) => None,
        }
    }
}

/// Editing actions which can be mapped to key bindings.
///
/// Executed by `Reedline::run_edit_commands()`
#[non_exhaustive]
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq, EnumDiscriminants)]
#[strum_discriminants(doc = "This is the auto generated discriminant type for [`EditCommand`]")]
#[strum_discriminants(derive(EnumIter, EnumString, VariantArray))]
#[strum_discriminants(strum(ascii_case_insensitive))]
pub enum EditCommand {
    /// Move to the start of the buffer
    MoveToStart {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move to the start of the current line
    MoveToLineStart {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move to the start of the current line skipping any whitespace
    MoveToLineNonBlankStart {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move to the end of the buffer
    MoveToEnd {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move to the end of the current line
    MoveToLineEnd {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one line up
    MoveLineUp {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one line down
    MoveLineDown {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one character to the left
    MoveLeft {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one character to the right
    MoveRight {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one word to the left
    MoveWordLeft {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one WORD to the left
    MoveBigWordLeft {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one word to the right
    MoveWordRight {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one word to the right, stop at start of word
    MoveWordRightStart {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one WORD to the right, stop at start of WORD
    MoveBigWordRightStart {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one word to the right, stop at end of word
    MoveWordRightEnd {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move one WORD to the right, stop at end of WORD
    MoveBigWordRightEnd {
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move to position
    MoveToPosition {
        /// Position to move to
        position: usize,
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move the cursor to a [`MotionTarget`], collapsing any selection.
    ///
    /// The parameterized, human-readable face of the selection primitive.
    /// `Move`/`Extend`/`Cut`/`Copy`/`Change`/`Erase` over a [`MotionTarget`] are
    /// the going-forward motion API; the older `MoveWord*`/`CutWord*` variants
    /// are kept as sugar over them.
    Move(MotionTarget),

    /// Extend the selection to a [`MotionTarget`]: move the cursor head to the
    /// target while keeping the anchor fixed.
    Extend(MotionTarget),

    /// Cut the text between the cursor and a [`MotionTarget`] into the cut buffer.
    /// `granularity` decides whether the span is taken char-wise or snapped to
    /// whole lines (and tags the register accordingly).
    Cut {
        /// Where the operator reaches to.
        target: MotionTarget,
        /// Char-wise span or whole lines.
        granularity: Granularity,
    },

    /// Copy the text between the cursor and a [`MotionTarget`] into the cut
    /// buffer, leaving the buffer and cursor unchanged.
    Copy {
        /// Where the operator reaches to.
        target: MotionTarget,
        /// Char-wise span or whole lines.
        granularity: Granularity,
    },

    /// Select up to a [`MotionTarget`]: drop a fresh anchor at the caret, then
    /// move the head to the target, so the selection covers the span just traveled.
    Select(MotionTarget),

    /// Select a [`TextObject`]
    SelectTextObject(TextObject),

    /// Cut like [`EditCommand::Cut`], except that a `LineWise` span keeps its
    /// line terminators: only the lines' *content* is consumed, so one blank
    /// line remains — the vi change operator's linewise semantics
    /// (`cc`/`cj`/`cgg`), which re-enter insert mode on that blank line.
    /// Identical to `Cut` for `CharWise` spans.
    Change {
        /// Where the operator reaches to.
        target: MotionTarget,
        /// Char-wise span or whole lines.
        granularity: Granularity,
    },

    /// Erase the text between the cursor and a [`MotionTarget`] without touching
    /// the cut buffer (no-register counterpart of [`EditCommand::Cut`]).
    Erase(MotionTarget),

    /// Insert a character at the current insertion point
    InsertChar(char),

    /// Insert a string at the current insertion point
    InsertString(String),

    /// Insert a character pair around the selection or insertion point
    InsertPair {
        /// Opening character of the pair
        open: char,
        /// Closing character of the pair
        close: char,
    },

    /// Inserts the system specific new line character
    ///
    /// - On Unix systems LF (`"\n"`)
    /// - On Windows CRLF (`"\r\n"`)
    InsertNewline,

    /// Inserts a new line above the current line
    ///
    /// - On Unix systems LF (`"\n"`)
    /// - On Windows CRLF (`"\r\n"`)
    InsertNewlineAbove,

    /// Inserts a new line below the current line
    ///
    /// - On Unix systems LF (`"\n"`)
    /// - On Windows CRLF (`"\r\n"`)
    InsertNewlineBelow,

    /// Replace a character
    ReplaceChar(char),

    /// Replace characters with string
    ReplaceChars(usize, String),

    /// Backspace delete from the current insertion point
    Backspace,

    /// Backspace delete an empty character pair
    BackspacePair {
        /// Opening character of the pair
        open: char,
        /// Closing character of the pair
        close: char,
    },

    /// Delete in-place from the current insertion point
    Delete,

    /// Cut the grapheme left from the current insertion point
    CutCharLeft,

    /// Cut the grapheme right from the current insertion point
    CutChar,

    /// Backspace delete a word from the current insertion point
    BackspaceWord,

    /// Delete in-place a word from the current insertion point
    DeleteWord,

    /// Clear the current buffer
    Clear,

    /// Clear to the end of the current line
    ClearToLineEnd,

    /// Insert completion: entire completion if there is only one possibility, or else up to shared prefix.
    Complete,

    /// Cut the current line
    ///
    /// Legacy — prefer [`EditCommand::Cut`] with
    /// [`MotionTarget::LineEdge`]`(Forward)` and [`Granularity::LineWise`],
    /// which all builtin bindings lower through.
    CutCurrentLine,

    /// Cut from the start of the buffer to the insertion point
    CutFromStart,

    /// Cut from the start of the buffer to the line of insertion point
    ///
    /// Legacy — prefer [`EditCommand::Cut`] / [`EditCommand::Change`] (for
    /// `leave_blank_line: true`) with [`MotionTarget::BufferEdge`]`(Backward)`
    /// and [`Granularity::LineWise`], which all builtin bindings lower through.
    CutFromStartLinewise {
        /// When true, an empty line will remain after the operation
        leave_blank_line: bool,
    },

    /// Cut from the start of the current line to the insertion point
    CutFromLineStart,

    /// Cut from the first non whitespace character of the current line to the insertion point
    CutFromLineNonBlankStart,

    /// Cut from the insertion point to the end of the buffer
    CutToEnd,

    /// Cut from the line of insertion point to the end of the buffer
    ///
    /// Legacy — prefer [`EditCommand::Cut`] / [`EditCommand::Change`] (for
    /// `leave_blank_line: true`) with [`MotionTarget::BufferEdge`]`(Forward)`
    /// and [`Granularity::LineWise`], which all builtin bindings lower through.
    CutToEndLinewise {
        /// When true, an empty line will remain after the operation
        leave_blank_line: bool,
    },

    /// Cut from the insertion point to the end of the current line
    CutToLineEnd,

    /// Cut from the insertion point to the end of the current line
    /// If the cursor is already at the end of the line, remove the newline character
    KillLine,

    /// Cut the word left of the insertion point
    CutWordLeft,

    /// Cut the WORD left of the insertion point
    CutBigWordLeft,

    /// Cut the word right of the insertion point
    CutWordRight,

    /// Cut the word right of the insertion point
    CutBigWordRight,

    /// Cut the word right of the insertion point and any following space
    CutWordRightToNext,

    /// Cut the WORD right of the insertion point and any following space
    CutBigWordRightToNext,

    /// Paste the cut buffer in front of the insertion point (Emacs, vi `P`)
    PasteCutBufferBefore,

    /// Paste the cut buffer after the insertion point (vi `p`)
    PasteCutBufferAfter,

    /// Paste the cut buffer at the selection edge in the given `direction` and
    /// select the pasted text (helix `p`/`P`). Selecting the result means the
    /// command cannot simply be issued twice for a count, so it carries `count`
    /// explicitly.
    PasteAtSelectionEdge {
        /// Whether to paste on the forward or backward edge of the selection
        direction: Direction,
        /// Number of times the cut buffer content is placed
        count: usize,
    },

    /// Insert the kill ring's current slot at the caret — GNU `rl_yank`
    /// (kill.c:509-525), bound to `C-y` in the emacs keymap
    /// (emacs_keymap.c). The pre-insert caret is where GNU drops the mark
    /// (`_rl_set_mark_at_pos`); the inserted text is what a following
    /// [`YankPop`](EditCommand::YankPop) rewrites.
    Yank,

    /// Cycle the just-yanked text back to the next-older kill-ring slot —
    /// GNU `rl_yank_pop` (`M-y`, kill.c:527-556). Applicable only directly
    /// after [`Yank`](EditCommand::Yank) or another `YankPop`
    /// (`rl_last_func`), and only while the last yanked text still
    /// literally precedes the caret (`STREQN`); any intervening command
    /// makes it inapplicable.
    YankPop,

    /// Kill one filename component backward into the kill ring — GNU
    /// `rl_unix_filename_rubout` (kill.c:338-397): back over whitespace,
    /// then a `/` run, then a non-`/` non-whitespace component. GNU leaves
    /// it unbound in the emacs keymap; it is exposed here for custom
    /// bindings.
    CutFilenameLeft,

    /// Upper case the current word
    UppercaseWord,

    /// Lower case the current word
    LowercaseWord,

    /// Capitalize the current word: upper case its first character and
    /// leave the rest untouched (GNU readline `capitalize-word`, M-c)
    CapitalizeWord,

    /// Capitalize the current character
    CapitalizeChar,

    /// Switch the case of the current character
    SwitchcaseChar,

    /// Swap the current word with the word to the right
    SwapWords,

    /// Deletes all whitespace around the insertion point (readline
    /// `delete-horizontal-space`, `M-\`).
    DeleteHorizontalSpace,

    /// Swap the current grapheme/character with the one to the right
    SwapGraphemes,

    /// Undo the previous edit command
    Undo,

    /// Redo an edit command from the undo history
    Redo,

    /// Undo every change to the current line in one step — GNU readline
    /// `revert-line` (undo.c:331 `rl_revert_line`, bound to `M-r` and
    /// `M-C-r` in the emacs map). Reports inapplicable when the undo
    /// stack is empty, matching GNU's `rl_ding` on an empty
    /// `rl_undo_list`.
    ///
    /// Caveat vs GNU: the revert target is the bottom of reedline's
    /// undo stack — the buffer as it was when this line's editing
    /// began. GNU instead restores the current history entry's saved
    /// original (`entry->data`); since reedline's undo coalesces
    /// history navigation into one entry per hop, a recalled-then-edited
    /// entry reverts to the live line as it was before traversal, not
    /// to the entry's text.
    RevertLine,

    /// CutUntil right until char
    CutRightUntil(char),

    /// CutUntil right before char
    CutRightBefore(char),

    /// CutUntil right until char
    MoveRightUntil {
        /// Char to move towards
        c: char,
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// CutUntil right before char
    MoveRightBefore {
        /// Char to move towards
        c: char,
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// CutUntil left until char
    CutLeftUntil(char),

    /// CutUntil left before char
    CutLeftBefore(char),

    /// Move left until char
    MoveLeftUntil {
        /// Char to move towards
        c: char,
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Move left before char
    MoveLeftBefore {
        /// Char to move towards
        c: char,
        /// Select the text between the current cursor position and destination
        select: bool,
    },

    /// Select whole input buffer
    SelectAll,

    /// Snap the selection out to the whole lines it touches (helix `x`), or take
    /// one more line when it already spans them exactly.
    ///
    /// Repeating therefore grows it a line at a time, so a count is the command
    /// applied that many times.
    SelectLine,

    /// Delete the selection without filling the cut buffer (helix `Alt-d`).
    ///
    /// [`CutSelection`](EditCommand::CutSelection) clobbers the register, which
    /// is exactly what this avoids when the text is not wanted back.
    EraseSelection,

    /// Cut selection to local buffer
    CutSelection {
        /// Char-wise span or whole lines.
        granularity: Granularity,
    },

    /// Collapse the selection (or a block cursor's width) to a caret at one of its edges
    CollapseSelection(Direction),

    /// Copy selection to local buffer
    CopySelection,

    /// LowercaseSelection
    LowercaseSelection,

    /// Uppercase selection
    UppercaseSelection,

    /// Switchcase selection
    SwitchcaseSelection,

    /// Paste content from local buffer at the current cursor position
    Paste,

    /// Copy from the start of the buffer to the insertion point
    CopyFromStart,

    /// Copy from the start of the buffer to the line of insertion point
    ///
    /// Legacy — prefer [`EditCommand::Copy`] with
    /// [`MotionTarget::BufferEdge`]`(Backward)` and [`Granularity::LineWise`].
    CopyFromStartLinewise,

    /// Copy from the start of the current line to the insertion point
    CopyFromLineStart,

    /// Copy from the first non whitespace character of the current line to the insertion point
    CopyFromLineNonBlankStart,

    /// Copy from the insertion point to the end of the buffer
    CopyToEnd,

    /// Copy from the line of insertion point to the end of the buffer
    ///
    /// Legacy — prefer [`EditCommand::Copy`] with
    /// [`MotionTarget::BufferEdge`]`(Forward)` and [`Granularity::LineWise`].
    CopyToEndLinewise,

    /// Copy from the insertion point to the end of the current line
    CopyToLineEnd,

    /// Copy the current line
    CopyCurrentLine,

    /// Copy the word left of the insertion point
    CopyWordLeft,

    /// Copy the WORD left of the insertion point
    CopyBigWordLeft,

    /// Copy the word right of the insertion point
    CopyWordRight,

    /// Copy the WORD right of the insertion point
    CopyBigWordRight,

    /// Copy the word right of the insertion point and any following space
    CopyWordRightToNext,

    /// Copy the WORD right of the insertion point and any following space
    CopyBigWordRightToNext,

    /// Copy one character to the left
    CopyLeft,

    /// Copy one character to the right
    CopyRight,

    /// Copy until right until char
    CopyRightUntil(char),

    /// Copy right before char
    CopyRightBefore(char),

    /// Copy left until char
    CopyLeftUntil(char),

    /// Copy left before char
    CopyLeftBefore(char),
    /// Swap the positions of the cursor and anchor
    SwapCursorAndAnchor,

    /// Drop the mark at the insertion point — GNU `set-mark`
    /// (`C-@`/`M-SPC`, text.c:1880 `_rl_set_mark_at_pos`, bound at
    /// emacs_keymap.c:35 and :353). The mark is a saved buffer
    /// position, not a selection: it is invisible by default, matching
    /// GNU with `rl_enable_active_region` unset, and it does not move
    /// with the cursor, so the region it defines grows as point moves.
    SetMark,

    /// Swap the insertion point with the mark — GNU
    /// `exchange-point-and-mark` (`C-x C-x`, text.c:1885
    /// `rl_exchange_point_and_mark`, bound at emacs_keymap.c:622).
    /// Inapplicable when no mark is set — GNU's `rl_mark < 0` ding.
    ExchangePointAndMark,

    /// Kill the text between the mark and the insertion point into the
    /// kill ring — GNU `kill-region` (kill.c:449 `rl_kill_region` →
    /// `region_kill_internal`). The cursor lands on the region's start,
    /// as GNU's `npoint` assignment does. Unbound in GNU's emacs map
    /// too; exposed for custom bindings.
    KillRegion,

    /// Copy the text between the mark and the insertion point into the
    /// kill ring without deleting it — GNU `copy-region-as-kill`
    /// (kill.c:442 `rl_copy_region_to_kill`). Cursor does not move.
    /// Unbound in GNU's emacs map too; exposed for custom bindings.
    CopyRegionAsKill,

    /// Cut selection to system clipboard
    #[cfg(feature = "system_clipboard")]
    CutSelectionSystem,

    /// Copy selection to system clipboard
    #[cfg(feature = "system_clipboard")]
    CopySelectionSystem,

    /// Paste content from system clipboard at the current cursor position
    #[cfg(feature = "system_clipboard")]
    PasteSystem,

    /// Cut the specified text object
    CutTextObject {
        /// The text object to operate on
        text_object: TextObject,
    },

    /// Copy the specified text object
    CopyTextObject {
        /// The text object to operate on
        text_object: TextObject,
    },
    /// Add the specified text object around the selection
    AddTextObject {
        /// The text object to operate on
        text_object: TextObjectType,
    },
    /// Remove the nearest specified text object around the cursor head
    RemoveTextObject {
        /// The text object to operate on
        text_object: TextObjectType,
    },
    /// Replace the nearest specified text object around the cursor head
    ReplaceTextObject {
        /// The old text object to replace
        old: TextObjectType,
        /// The new text object to replace with
        new: TextObjectType,
    },
}

impl EditCommand {
    /// Determine if a certain operation should be undoable
    /// or if the operations should be coalesced for undoing
    pub fn edit_type(&self) -> EditType {
        match self {
            // Cursor moves
            EditCommand::MoveToStart { select, .. }
            | EditCommand::MoveToEnd { select, .. }
            | EditCommand::MoveToLineStart { select, .. }
            | EditCommand::MoveToLineEnd { select, .. }
            | EditCommand::MoveToLineNonBlankStart { select, .. }
            | EditCommand::MoveToPosition { select, .. }
            | EditCommand::MoveLineUp { select, .. }
            | EditCommand::MoveLineDown { select, .. }
            | EditCommand::MoveLeft { select, .. }
            | EditCommand::MoveRight { select, .. }
            | EditCommand::MoveWordLeft { select, .. }
            | EditCommand::MoveBigWordLeft { select, .. }
            | EditCommand::MoveWordRight { select, .. }
            | EditCommand::MoveWordRightStart { select, .. }
            | EditCommand::MoveBigWordRightStart { select, .. }
            | EditCommand::MoveWordRightEnd { select, .. }
            | EditCommand::MoveBigWordRightEnd { select, .. }
            | EditCommand::MoveRightUntil { select, .. }
            | EditCommand::MoveRightBefore { select, .. }
            | EditCommand::MoveLeftUntil { select, .. }
            | EditCommand::MoveLeftBefore { select, .. } => {
                EditType::MoveCursor { select: *select }
            }
            EditCommand::ExchangePointAndMark => EditType::MoveCursor { select: false },
            EditCommand::SwapCursorAndAnchor => EditType::MoveCursor { select: true },

            EditCommand::SelectAll => EditType::MoveCursor { select: true },
            EditCommand::EraseSelection => EditType::EditText,
            EditCommand::SelectLine => EditType::MoveCursor { select: true },
            // Text edits
            EditCommand::InsertChar(_)
            | EditCommand::Backspace
            | EditCommand::BackspacePair { .. }
            | EditCommand::Delete
            | EditCommand::CutChar
            | EditCommand::CutCharLeft
            | EditCommand::InsertString(_)
            | EditCommand::InsertPair { .. }
            | EditCommand::InsertNewline
            | EditCommand::InsertNewlineAbove
            | EditCommand::InsertNewlineBelow
            | EditCommand::ReplaceChar(_)
            | EditCommand::ReplaceChars(_, _)
            | EditCommand::BackspaceWord
            | EditCommand::DeleteWord
            | EditCommand::DeleteHorizontalSpace
            | EditCommand::Clear
            | EditCommand::ClearToLineEnd
            | EditCommand::Complete
            | EditCommand::CutCurrentLine
            | EditCommand::CutFromStart
            | EditCommand::CutFromStartLinewise { .. }
            | EditCommand::CutFromLineStart
            | EditCommand::CutFromLineNonBlankStart
            | EditCommand::CutToLineEnd
            | EditCommand::KillLine
            | EditCommand::CutToEnd
            | EditCommand::CutToEndLinewise { .. }
            | EditCommand::CutWordLeft
            | EditCommand::CutBigWordLeft
            | EditCommand::CutWordRight
            | EditCommand::CutBigWordRight
            | EditCommand::CutWordRightToNext
            | EditCommand::CutBigWordRightToNext
            | EditCommand::PasteCutBufferBefore
            | EditCommand::PasteCutBufferAfter
            | EditCommand::Yank
            | EditCommand::YankPop
            | EditCommand::CutFilenameLeft
            | EditCommand::UppercaseWord
            | EditCommand::LowercaseWord
            | EditCommand::CapitalizeWord
            | EditCommand::SwitchcaseChar
            | EditCommand::CapitalizeChar
            | EditCommand::SwapWords
            | EditCommand::SwapGraphemes
            | EditCommand::CutRightUntil(_)
            | EditCommand::CutRightBefore(_)
            | EditCommand::CutLeftUntil(_)
            | EditCommand::CutLeftBefore(_)
            | EditCommand::CutSelection { .. }
            | EditCommand::LowercaseSelection
            | EditCommand::UppercaseSelection
            | EditCommand::SwitchcaseSelection
            | EditCommand::Paste
            | EditCommand::CutTextObject { .. }
            | EditCommand::PasteAtSelectionEdge { .. }
            | EditCommand::KillRegion => EditType::EditText,

            #[cfg(feature = "system_clipboard")] // Sadly cfg attributes in patterns don't work
            EditCommand::CutSelectionSystem | EditCommand::PasteSystem => EditType::EditText,

            EditCommand::Undo | EditCommand::Redo | EditCommand::RevertLine => EditType::UndoRedo,

            EditCommand::CopySelection => EditType::NoOp,
            #[cfg(feature = "system_clipboard")]
            EditCommand::CopySelectionSystem => EditType::NoOp,
            EditCommand::CopyFromStart
            | EditCommand::CopyFromStartLinewise
            | EditCommand::CopyFromLineStart
            | EditCommand::CopyFromLineNonBlankStart
            | EditCommand::CopyToEnd
            | EditCommand::CopyToEndLinewise
            | EditCommand::CopyToLineEnd
            | EditCommand::CopyCurrentLine
            | EditCommand::CopyWordLeft
            | EditCommand::CopyBigWordLeft
            | EditCommand::CopyWordRight
            | EditCommand::CopyBigWordRight
            | EditCommand::CopyWordRightToNext
            | EditCommand::CopyBigWordRightToNext
            | EditCommand::CopyLeft
            | EditCommand::CopyRight
            | EditCommand::CopyRightUntil(_)
            | EditCommand::CopyRightBefore(_)
            | EditCommand::CopyLeftUntil(_)
            | EditCommand::CopyLeftBefore(_)
            | EditCommand::CopyTextObject { .. }
            | EditCommand::SetMark
            | EditCommand::CopyRegionAsKill => EditType::NoOp,

            EditCommand::AddTextObject { .. }
            | EditCommand::RemoveTextObject { .. }
            | EditCommand::ReplaceTextObject { .. } => EditType::EditText,

            // The six MotionTarget verbs. `Move`/`Extend` carry the old `select`
            // bool in the verb itself (Extend must be `select: true` so the editor
            // does not clear the selection it is extending). `Cut`/`Change`/`Erase`
            // mutate the buffer; `Copy` does not.
            EditCommand::Move(_) => EditType::MoveCursor { select: false },
            EditCommand::Extend(_) => EditType::MoveCursor { select: true },
            EditCommand::CollapseSelection(_) => EditType::MoveCursor { select: false },
            EditCommand::Select(_) | EditCommand::SelectTextObject(_) => {
                EditType::MoveCursor { select: true }
            }
            EditCommand::Cut { .. } => EditType::EditText,
            EditCommand::Copy { .. } => EditType::NoOp,
            EditCommand::Change { .. } => EditType::EditText,
            EditCommand::Erase(_) => EditType::EditText,
        }
    }

    /// Whether executing this command can feed the kill ring — the
    /// `_rl_last_command_was_kill` bookkeeping of readline.c's charloop
    /// (readline.c:588,711-716): only a kill-class command re-arms the flag
    /// that lets the *next* kill merge into the newest slot
    /// (`_rl_copy_to_kill_ring`, kill.c:90); every other command clears it.
    /// Both cuts and copies count, as in GNU (`region_kill_internal`
    /// bumps the flag for `copy-region-as-kill` too, kill.c:445-449), and
    /// an attempted kill that moves no text still counts (kill.c:150-153).
    ///
    /// Pastes, [`Yank`](EditCommand::Yank) and
    /// [`YankPop`](EditCommand::YankPop) read the ring without feeding it,
    /// so they break a merge chain just like GNU's non-kill commands do.
    pub(crate) fn feeds_kill_ring(&self) -> bool {
        match self {
            EditCommand::Cut { .. }
            | EditCommand::Change { .. }
            | EditCommand::CutChar
            | EditCommand::CutCharLeft
            | EditCommand::BackspaceWord
            | EditCommand::CutCurrentLine
            | EditCommand::CutFromStart
            | EditCommand::CutFromStartLinewise { .. }
            | EditCommand::CutFromLineStart
            | EditCommand::CutFromLineNonBlankStart
            | EditCommand::CutToEnd
            | EditCommand::CutToEndLinewise { .. }
            | EditCommand::CutToLineEnd
            | EditCommand::KillLine
            | EditCommand::CutWordLeft
            | EditCommand::CutBigWordLeft
            | EditCommand::CutWordRight
            | EditCommand::CutBigWordRight
            | EditCommand::CutWordRightToNext
            | EditCommand::CutBigWordRightToNext
            | EditCommand::CutRightUntil(_)
            | EditCommand::CutRightBefore(_)
            | EditCommand::CutLeftUntil(_)
            | EditCommand::CutLeftBefore(_)
            | EditCommand::CutSelection { .. }
            | EditCommand::CutTextObject { .. }
            | EditCommand::CutFilenameLeft
            | EditCommand::KillRegion
            | EditCommand::Copy { .. }
            | EditCommand::CopyRegionAsKill
            | EditCommand::CopySelection
            | EditCommand::CopyFromStart
            | EditCommand::CopyFromStartLinewise
            | EditCommand::CopyFromLineStart
            | EditCommand::CopyFromLineNonBlankStart
            | EditCommand::CopyToEnd
            | EditCommand::CopyToEndLinewise
            | EditCommand::CopyToLineEnd
            | EditCommand::CopyCurrentLine
            | EditCommand::CopyWordLeft
            | EditCommand::CopyBigWordLeft
            | EditCommand::CopyWordRight
            | EditCommand::CopyBigWordRight
            | EditCommand::CopyWordRightToNext
            | EditCommand::CopyBigWordRightToNext
            | EditCommand::CopyLeft
            | EditCommand::CopyRight
            | EditCommand::CopyRightUntil(_)
            | EditCommand::CopyRightBefore(_)
            | EditCommand::CopyLeftUntil(_)
            | EditCommand::CopyLeftBefore(_)
            | EditCommand::CopyTextObject { .. } => true,
            // The system-clipboard cut still feeds the local ring through
            // `cut_range`; the system copy does not.
            #[cfg(feature = "system_clipboard")]
            EditCommand::CutSelectionSystem => true,
            _ => false,
        }
    }
}

/// Specifies the types of edit commands, used to simplify grouping edits
/// to mark undo behavior
#[derive(PartialEq, Eq)]
pub enum EditType {
    /// Cursor movement commands
    MoveCursor { select: bool },
    /// Undo/Redo commands
    UndoRedo,
    /// Text editing commands
    EditText,
    /// No effect on line buffer
    NoOp,
}

/// Every line change should come with an `UndoBehavior` tag, which can be used to
/// calculate how the change should be reflected on the undo stack
#[derive(Debug)]
pub enum UndoBehavior {
    /// Character insertion, tracking the character inserted
    InsertCharacter(char),
    /// Backspace command, tracking the deleted character (left of cursor)
    /// Warning: this does not track the whole grapheme, just the character
    Backspace(Option<char>),
    /// Delete command, tracking the deleted character (right of cursor)
    /// Warning: this does not track the whole grapheme, just the character
    Delete(Option<char>),
    /// Move the cursor position
    MoveCursor,
    /// Navigated the history using up or down arrows
    HistoryNavigation,
    /// Catch-all for actions that should always form a unique undo point and never be
    /// grouped with later edits
    CreateUndoPoint,
    /// For actions that shouldn't be reflected on the edit stack e.g. Undo/Redo
    NoOp,
}

impl UndoBehavior {
    /// Return if the current operation should start a new undo set, or be
    /// combined with the previous operation
    pub fn create_undo_point_after(&self, previous: &UndoBehavior) -> bool {
        use UndoBehavior as UB;
        match (previous, self) {
            // Never start an undo set with cursor movement
            (_, UB::MoveCursor) => false,
            (UB::HistoryNavigation, UB::HistoryNavigation) => false,
            // When inserting/deleting repeatedly, each undo set should encompass
            // inserting/deleting a complete word and the associated whitespace
            (UB::InsertCharacter(c_prev), UB::InsertCharacter(c_new)) => {
                (*c_prev == '\n' || *c_prev == '\r')
                    || (!c_prev.is_whitespace() && c_new.is_whitespace())
            }
            (UB::Backspace(Some(c_prev)), UB::Backspace(Some(c_new))) => {
                (*c_new == '\n' || *c_new == '\r')
                    || (c_prev.is_whitespace() && !c_new.is_whitespace())
            }
            (UB::Backspace(_), UB::Backspace(_)) => false,
            (UB::Delete(Some(c_prev)), UB::Delete(Some(c_new))) => {
                (*c_new == '\n' || *c_new == '\r')
                    || (c_prev.is_whitespace() && !c_new.is_whitespace())
            }
            (UB::Delete(_), UB::Delete(_)) => false,
            (_, _) => true,
        }
    }
}

/// Reedline supported actions.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, EnumDiscriminants)]
#[strum_discriminants(doc = "This is the auto generated discriminant type for [`ReedlineEvent`]")]
#[strum_discriminants(derive(EnumIter, EnumString, VariantArray))]
#[strum_discriminants(strum(ascii_case_insensitive))]
pub enum ReedlineEvent {
    /// No op event
    None,

    /// Complete history hint (default in full)
    HistoryHintComplete,

    /// Complete a single token/word of the history hint
    HistoryHintWordComplete,

    /// Handle EndOfLine event
    ///
    /// Expected Behavior:
    ///
    /// - On empty line breaks execution to exit with [`Signal::CtrlD`]
    /// - Secondary behavior [`EditCommand::Delete`]
    CtrlD,

    /// Handle SIGTERM key input
    ///
    /// Expected behavior:
    ///
    /// Abort entry
    /// Run [`EditCommand::Clear`]
    /// Clear the current undo
    /// Bubble up [`Signal::CtrlC`]
    CtrlC,

    /// Clears the screen and sets prompt to first line
    ClearScreen,

    /// Clears the screen and the scrollback buffer
    ///
    /// Sets the prompt back to the first line
    ClearScrollback,

    /// Handle enter event
    Enter,

    /// Handle unconditional submit event
    Submit,

    /// Submit at the end of the *complete* text, otherwise newline
    SubmitOrNewline,

    /// Esc event
    #[strum_discriminants(strum(serialize = "Esc", serialize = "Escape"))]
    Esc,

    /// Mouse click event with screen coordinates
    Mouse {
        /// Column (x) position, 0-indexed from left
        column: u16,
        /// Row (y) position, 0-indexed from top
        row: u16,
        /// Which mouse button was clicked
        button: MouseButton,
    },

    /// trigger terminal resize
    Resize(u16, u16),

    /// Run these commands in the editor
    Edit(Vec<EditCommand>),

    /// Trigger full repaint
    Repaint,

    /// Navigate to the previous historic buffer
    PreviousHistory,
    /// Jump to the oldest entry in the history (GNU readline
    /// `beginning-of-history`, PgUp in its default map).
    FirstHistoryEntry,
    /// Jump to the newest entry in the history (GNU readline
    /// `end-of-history`, PgDn in its default map).
    LastHistoryEntry,

    /// Move up to the previous line, if multiline, or up into the historic buffers
    Up,

    /// Move down to the next line, if multiline, or down through the historic buffers
    Down,

    /// Move right to the next column, completion entry, or complete hint
    Right,

    /// Move left to the next column, or completion entry
    Left,

    /// Move to the start of the buffer
    ToStart,

    /// Move to the end of the buffer
    ToEnd,

    /// Navigate to the next historic buffer
    NextHistory,

    /// Search the history for a string
    SearchHistory,

    /// In vi mode multiple reedline events can be chained while parsing the
    /// command or movement characters
    Multiple(Vec<ReedlineEvent>),

    /// Test
    UntilFound(Vec<ReedlineEvent>),

    /// Trigger a menu event. It activates a menu with the event name
    Menu(String),

    /// Accept the highlighted menu item without submitting the line.
    ///
    /// Splices the selection into the buffer and closes the menu, as `Enter` does while
    /// a menu is open, but stops there. Inapplicable when no menu is active or the
    /// active one has nothing to accept, so it composes: `UntilFound` falls through to
    /// the next event, and `Multiple` can follow it with an edit, e.g. a space that
    /// accepts the completion and keeps typing.
    MenuAccept,

    /// Next element in the menu
    MenuNext,

    /// Previous element in the menu
    MenuPrevious,

    /// Moves up in the menu
    MenuUp,

    /// Moves down in the menu
    MenuDown,

    /// Moves left in the menu
    MenuLeft,

    /// Moves right in the menu
    MenuRight,

    /// Move to the next history page
    MenuPageNext,

    /// Move to the previous history page
    MenuPagePrevious,

    /// Triggers an immediate return from [`Reedline::read_line()`](crate::Reedline::read_line) with an opaque payload.
    ///
    /// Reedline does not inspect or validate the contents of this string. It is
    /// passed directly through to the caller as a [`Signal::HostCommand`].
    /// Use this to send custom instructions or serialized data from a keybinding
    /// logic back to the main application loop.
    ExecuteHostCommand(String),

    /// Open text editor
    OpenEditor,

    /// Switch the line editor to the named edit mode.
    ///
    /// The target names both the machine and the state to land in, so
    /// `SwitchMode(PromptEditMode::Helix(PromptHelixMode::Normal))` reaches
    /// helix normal from anywhere: from another helix state, from vi, or from
    /// emacs. The engine offers the target to the active machine first and
    /// then to every machine registered with
    /// [`Reedline::with_additional_edit_mode`](crate::Reedline::with_additional_edit_mode);
    /// the first to accept it becomes active. A host-defined machine is reached
    /// through the name it reports as [`PromptEditMode::Custom`].
    ///
    /// A target no machine accepts leaves everything alone and reports the
    /// event inapplicable. On its own that is a keybinding that does nothing;
    /// inside an [`UntilFound`](ReedlineEvent::UntilFound) it hands the key to
    /// the next event in the list instead. A target naming the mode already
    /// active is declined the same way, so a binding built as
    /// `UntilFound([SwitchMode(A), SwitchMode(B)])` toggles between the two.
    SwitchMode(PromptEditMode),

    /// Replays a sequence of keys as if the user had typed them.
    ///
    /// This is the mechanism GNU readline's `bind '"key": "macro"'` needs:
    /// a macro's payload is not text to insert but a byte stream of
    /// keystrokes, each dispatched through the keymap when it is
    /// *consumed* — not when the sequence was registered. A replayed key
    /// may therefore fire any binding, including another `KeySequence`;
    /// literal characters belong in the stream as
    /// `(KeyModifiers::NONE, KeyCode::Char(c))` and reach the buffer
    /// through the ordinary self-insert path, which also makes the same
    /// sequence do the right thing under vi-mode maps where letters are
    /// bound to motions.
    ///
    /// Replayed keys are queued ahead of live terminal input and consumed
    /// at the start of the next input batch — so inside a [`Multiple`], a
    /// `KeySequence` enqueues while the remaining events still run first.
    /// The queue is bounded ([`crate::Reedline::MAX_REPLAYED_KEYS`]); a
    /// self-retriggering sequence is truncated once the cap is reached.
    KeySequence(Vec<crate::edit_mode::KeyCombination>),

    /// Inserts the next key literally, bypassing its binding
    /// (readline `quoted-insert`, `C-v`/`C-q`).
    ///
    /// The key after this event is consumed as data, not dispatched:
    /// a character inserts itself (control-modified characters insert
    /// their control byte), `Enter` inserts `\r`, `Tab` inserts `\t`,
    /// and non-character keys are consumed with no insert — matching
    /// GNU's `rl_quoted_insert`, which reads one input character
    /// verbatim.
    QuotedInsert,

    /// Read one key, then move the cursor to that character's next
    /// occurrence in `direction` — GNU readline `character-search`
    /// (`C-]`, text.c:1829 `rl_char_search` / `rl_backward_char_search`,
    /// `M-C-]`). GNU's `FFIND`/`BFIND` land *on* the match and skip the
    /// character under the cursor; the move resolves to a no-op (and
    /// the event reports inapplicable) when the character isn't found.
    ///
    /// The key after this event is consumed as the search target, the
    /// same interception [`ReedlineEvent::QuotedInsert`] performs — a
    /// `Char` key supplies the target character; any other key cancels
    /// the search. Emacs default: `C-]` forward, `M-C-]` backward.
    CharSearch {
        /// Which way to search for the typed character
        direction: Direction,
    },

    /// Insert an argument from the previous history line — GNU readline
    /// `yank-last-arg` (`M-.`/`M-_`) and, with an explicit argument,
    /// `yank-nth-arg` (kill.c:603-702).
    ///
    /// `count: None` yanks the last argument (GNU `'$'`); `Some(n)`
    /// yanks argument `n`, where 0 is the command name and negatives
    /// count back from the end. A consecutive repeat undoes the
    /// previous insert and yanks the same argument from the
    /// progressively older entry (GNU's `history_skip` walk); a negative
    /// `count` on a repeat flips the walk direction, as in GNU.
    ///
    /// Caveat: GNU tokenizes the entry with full shell quoting rules
    /// (`history_arg_extract`, histexpand.c:1424); this implementation
    /// splits on whitespace, so a quoted `"two words"` counts as two
    /// arguments here.
    YankLastArg {
        /// Which argument to yank — `None` for the last
        count: Option<i32>,
    },

    /// Insert the `count`th argument of the previous history line —
    /// GNU readline `yank-nth-arg` (`M-C-y`, kill.c:644
    /// `rl_yank_nth_arg`). Unlike [`ReedlineEvent::YankLastArg`] this
    /// is single-shot: repeating it inserts the same argument again
    /// instead of walking older entries.
    YankNthArg {
        /// Which argument to yank — 0 is the command name, negatives
        /// count back from the end
        count: i32,
    },

    /// Accept the current line, and preload the *next* history entry
    /// into the following `read_line` — GNU readline
    /// `operate-and-get-next` (`C-o`, misc.c:782
    /// `rl_operate_and_get_next`).
    ///
    /// The entry preloaded is the one after the history position the
    /// line was submitted from: recalling entry `k` then pressing `C-o`
    /// loads `k+1` (which, when `k` was the newest, is the just-accepted
    /// line — GNU's `where_history()+history_base+1` produces the same).
    /// On a fresh line there is no following entry and the next prompt
    /// starts empty. The preload also positions the history cursor, so
    /// `Up`/`Down` continue from the loaded entry.
    OperateAndGetNext,

    /// Search history using the buffer up to the insertion point as the
    /// needle — GNU readline `history-search-backward` /
    /// `history-search-forward` when `anchored` (search.c:713/675), and
    /// `history-substr-search-backward` / `history-substr-search-forward`
    /// otherwise (search.c:750/732).
    ///
    /// An anchored search requires the entry to *start with* the needle
    /// and lands the cursor at the end of the needle; a substring
    /// search matches anywhere and lands the cursor at the match
    /// offset (GNU also sets `rl_mark` at the line end — not modeled
    /// here). Consecutive presses continue the same search in either
    /// direction; any other event reinitializes from the current
    /// buffer. With an empty needle the event degrades to plain
    /// previous/next-history traversal, exactly as GNU's
    /// `_rl_history_search_len == 0` falls back to
    /// `rl_get_previous_history`/`rl_get_next_history`.
    ///
    /// Not bound in the default emacs map (GNU leaves it unbound too);
    /// hosts bind it directly, e.g. to `Up`/`Down` or `M-p`/`M-n`.
    HistorySearch {
        /// Older entries (`Backward`) or newer ones (`Forward`)
        direction: Direction,
        /// `true` for an anchored prefix match (GNU `history-search-*`),
        /// `false` for a substring match (GNU `history-substr-search-*`)
        anchored: bool,
    },

    /// The keypress was absorbed as the first half of a chord the edit
    /// mode is still collecting — the `C-x` of GNU's `C-x )`, which is
    /// a prefix *keymap* in the emacs map (`emacs_keymap.c:59`,
    /// `ISKMAP emacs_ctlx_keymap`), flattened here into a one-key
    /// pending state since `Keybindings` has no prefix tables.
    ///
    /// `Pending` produces no action; the next key resolves the chord.
    /// The engine's macro recorder erases an absorbed prefix key from
    /// the recording — GNU records it, then strips the resolved
    /// sequence's whole key count when a chord reaches a macro command
    /// (`macro.c:276`, `rl_key_sequence_length`), which nets out the
    /// same for `C-x )` and additionally keeps aborted chords out of
    /// the recording.
    Pending,

    /// Begin keyboard-macro recording (GNU `rl_start_kbd_macro`,
    /// `macro.c:246` — `C-x (` in the emacs `C-x` map). Every later key
    /// press is recorded verbatim until
    /// [`EndKbdMacro`](ReedlineEvent::EndKbdMacro): keystrokes, not
    /// commands (`readline.c:907`, `_rl_add_macro_char` runs ahead of
    /// dispatch), so the recording replays through whatever keymap is
    /// live then. Aborts — GNU dings — when already recording.
    StartKbdMacro,

    /// End keyboard-macro recording and keep it as the replayable last
    /// macro (GNU `rl_end_kbd_macro`, `macro.c:270` — `C-x )`). The
    /// keys that invoked it are not part of the recording (GNU
    /// subtracts `rl_key_sequence_length`). An explicit numeric
    /// argument makes GNU execute the new macro `count - 1` more
    /// times, counting the definition as the first run (`macro.c:283`).
    EndKbdMacro,

    /// Replay the last recorded keyboard macro (GNU
    /// `rl_call_last_kbd_macro`, `macro.c:291` — `C-x e`), a numeric
    /// argument repeating it that many times. Refused while a
    /// recording is in progress — GNU's "no recursive macros" —
    /// replayed keys queue ahead of live input exactly like
    /// [`KeySequence`](ReedlineEvent::KeySequence).
    CallLastKbdMacro,

    /// Accumulate a digit into the pending numeric argument (GNU
    /// `rl_digit_argument`, `misc.c:230`): `Some(d)` is an `M-0`..`M-9`
    /// key, `None` is `M--`, which negates the argument. Fully applied
    /// at parse time — the argument is engine state and a real key
    /// never reaches a handler; GNU reads the rest of the argument
    /// inside `rl_digit_loop` (`misc.c:192`) instead.
    DigitArgument(Option<u8>),

    /// Multiply the pending numeric argument by four (GNU
    /// `rl_universal_argument`, `misc.c:262`). Unbound in the default
    /// emacs map — GNU binds `C-u` to `unix-line-discard` there
    /// (`emacs_keymap.c:56`), which stays ours. Applied at parse time
    /// like [`DigitArgument`](ReedlineEvent::DigitArgument).
    UniversalArgument,

    /// The key that terminated numeric-argument accumulation,
    /// dispatched under the argument (GNU `misc.c`
    /// `_rl_arg_dispatch:174`: every non-argument key ends the loop and
    /// is itself dispatched with `count = rl_numeric_arg *
    /// rl_arg_sign`).
    ///
    /// `explicit` mirrors `rl_explicit_arg` — set once a digit was
    /// seen, or `M--` was upgraded to `-1` by GNU's `NUM_SAWMINUS`
    /// rule (`misc.c:170`). Nothing consumes it yet; it is carried so
    /// count-aware commands (GNU's yank-last-arg, overwrite-mode) can
    /// later tell "no argument" from "the argument was 1".
    WithArg {
        /// `rl_numeric_arg * rl_arg_sign` (`readline.c:941`)
        count: i32,
        /// `rl_explicit_arg`
        explicit: bool,
        /// The terminating key's own event.
        event: Box<ReedlineEvent>,
    },

    /// GNU `overwrite-mode` (misc.c rl_overwrite_mode, bound to the
    /// Insert key): `None` toggles, `Some(on)` sets explicitly — the
    /// explicit-arg form in GNU (`arg>0` overwrites, `arg<=0` inserts).
    OverwriteMode(Option<bool>),

    /// Suspend the editor like GNU `rl_suspend`/`rl_stop` (signals.c):
    /// the terminal is deprepped, `SIGTSTP` is raised to the own
    /// process, and on resume (`SIGCONT`) the terminal is re-prepped
    /// and the line repainted — all inside `read_line`, transparent to
    /// the host. No-op on platforms without job control signals.
    Suspend,

    /// Collect a search string in-place, then run a single history
    /// search on `Enter` — GNU `noninc-search-forward` /
    /// `noninc-search-backward` (search.c:462 `noninc_search`, bound
    /// `M-n`/`M-p` in the emacs map at emacs_keymap.c:441/:443 and
    /// `/`/`?` in the vi map via `rl_vi_search`, vi_mode.c:365).
    ///
    /// While collecting, the line buffer holds the string being typed
    /// (GNU reads it into `rl_line_buffer` after saving the real line,
    /// search.c:244-247): edits apply to it normally, `Backspace` on an
    /// empty buffer aborts (the `rl_point == 0` rubout branch,
    /// search.c:331), and `C-g`/`C-c`/`Esc` abort and restore the
    /// saved line. `Enter` commits: a string starting with `^` requires
    /// a prefix match (`ANCHORED_SEARCH`, search.c:145-149), otherwise
    /// any substring matches. An empty string reuses the last search
    /// string from its last hit (`rl_point == 0`, search.c:412-425);
    /// with none saved, the commit dings. A successful search leaves
    /// the found entry in the buffer with the point at its start
    /// (`rl_point = 0`, search.c:215); a failed one restores the saved
    /// line and point.
    ///
    /// `prompt_char` is the one-character prompt GNU substitutes while
    /// reading (`_rl_make_prompt_for_search`, search.c:235 — `:`
    /// default, `/`/`?` in vi). `pattern` requests GNU's vi-command
    /// glob matching (`SF_PATTERN` → `_hs_history_patsearch`,
    /// histsearch.c:245): `*`, `?` and `[...]` classes, implicitly
    /// wrapped in `*` when unanchored.
    ///
    /// Caveat vs GNU: `_rl_search_case_fold` is not modeled — matching
    /// is always case-sensitive.
    NonIncrementalSearch {
        /// `Forward` searches newer entries (GNU `M-n`/vi `?`),
        /// `Backward` older ones (`M-p`/vi `/`)
        direction: Direction,
        /// The prompt glyph shown while the string collects
        prompt_char: char,
        /// `true` for GNU's vi `SF_PATTERN` glob matching
        pattern: bool,
    },

    /// Repeat the last non-incremental search with the saved string —
    /// GNU `noninc-forward-search-again` /
    /// `noninc-reverse-search-again` (search.c:517/:541), which vi's
    /// `n`/`N` call through `rl_vi_search_again` (vi_mode.c:348).
    /// Inapplicable — GNU dings — before the first search.
    ///
    /// `Forward` searches newer entries, `Backward` older ones, and
    /// matching is always the `SF_PATTERN` glob form GNU uses under vi
    /// command mode (vi_mode.c:355/:548).
    NonIncrementalSearchAgain {
        /// `Forward` toward newer entries (vi `N`), `Backward` toward
        /// older ones (vi `n`)
        direction: Direction,
    },

    /// Print the last recorded keyboard macro as a keyseq spelling —
    /// GNU `print-last-kbd-macro` (macro.c:309
    /// `rl_print_last_kbd_macro` → `_rl_untranslate_macro_value`,
    /// bind.c:722). The spelling uses the GNU bind forms
    /// (`\C-x`, `\M-x`, `\e`, `^?`) that `bind -p` emits; inapplicable
    /// — GNU dings — when nothing is recorded. Unbound in GNU's maps
    /// too; exposed for custom bindings.
    PrintLastKbdMacro,
}

pub enum EventStatus {
    Handled,
    Inapplicable,
    Exits(Signal),
}

/// An edit that applied is a handled event, and one that had nothing to do
/// is an inapplicable one, so a binding can fall through it.
impl From<EditCommandStatus> for EventStatus {
    fn from(status: EditCommandStatus) -> Self {
        match status {
            EditCommandStatus::Applied => EventStatus::Handled,
            EditCommandStatus::Inapplicable => EventStatus::Inapplicable,
        }
    }
}

/// A wrapper for [crossterm::event::Event].
///
/// It ensures that the given event doesn't contain [KeyEventKind::Release]
/// (which is rejected) or [KeyEventKind::Repeat] (which is converted to
/// [KeyEventKind::Press]).
pub struct ReedlineRawEvent(Event);

impl TryFrom<Event> for ReedlineRawEvent {
    type Error = ();

    fn try_from(event: Event) -> Result<Self, Self::Error> {
        match event {
            Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            }) => Err(()),
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Repeat,
                state,
            }) => Ok(Self(Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state,
            }))),
            other => Ok(Self(other)),
        }
    }
}

impl From<ReedlineRawEvent> for Event {
    fn from(event: ReedlineRawEvent) -> Self {
        event.0
    }
}
