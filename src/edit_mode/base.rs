use crate::{
    enums::{EditCommand, EventStatus, ReedlineEvent, ReedlineRawEvent},
    PromptEditMode,
};

/// Define the style of parsing for the edit events
/// Available default options:
/// - Emacs
/// - Vi
/// - Helix
pub trait EditMode: Send {
    /// Translate the given user input event into what the `LineEditor` understands
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent;

    /// What to display in the prompt indicator
    fn edit_mode(&self) -> PromptEditMode;

    /// Handles events that apply only to specific edit modes.
    ///
    /// This is also how a [`ReedlineEvent::SwitchMode`] finds its machine. The
    /// engine offers the target to the active mode and then to every standby
    /// registered with `Reedline::with_additional_edit_mode`, and activates the
    /// first one to answer `EventStatus::Handled`. So a mode has to:
    ///
    /// - answer `Handled` for every [`PromptEditMode`] it can report from
    ///   [`edit_mode`](Self::edit_mode), moving into that state and dropping
    ///   any half-typed sequence, or no binding can ever reach it;
    /// - answer `Inapplicable` for everything else *without changing state*,
    ///   since a standby that declines stays a standby and would otherwise be
    ///   left altered by a switch that went elsewhere.
    ///
    /// The engine never asks about the state the active machine already
    /// reports: that switch is declined up front as not a move. A machine
    /// still answers `Handled` for it, since as a standby it may be asked for
    /// the very state it rests in.
    ///
    /// The default declines everything, which suits a mode that is only ever
    /// the active one.
    ///
    /// Only the machine's own state changes here. Any cursor repair the flip
    /// implies is the engine's job, stated over the rest policy the mode maps
    /// to — see `Reedline::change_edit_mode`.
    fn handle_mode_specific_event(&mut self, _event: ReedlineEvent) -> EventStatus {
        EventStatus::Inapplicable
    }

    /// Told how an [`ReedlineEvent::Edit`] the engine just executed came
    /// out, so a mode whose parse-time state was staked on the edit
    /// applying can settle the stake. Vi's operator layer is the
    /// motivating case: it enters insert mode for `c<motion>` while
    /// parsing, before the motion is evaluated against the buffer; when
    /// the motion fails, the operator is cancelled in GNU (`rl_vi_domove`
    /// returning -1, vi_mode.c:1385-1396) and the insert entry has to be
    /// revoked, which only this report can trigger.
    ///
    /// `commands` are the commands that were run (after any engine
    /// rewriting) and `status` is their aggregate outcome. The report is
    /// a notification, not a query: it arrives after the fact, for every
    /// executed edit batch — including batches the mode did not author
    /// (host bindings, menu edits) — so a mode that cares must match the
    /// report against its own outstanding stake and ignore the rest.
    ///
    /// The default ignores the report, which suits modes whose state
    /// never depends on an edit's outcome.
    fn note_edit_outcome(&mut self, _commands: &[EditCommand], _status: EventStatus) {}

    /// Whether the mode is holding a half-typed multi-key sequence.
    ///
    /// The engine uses this to poll with a deadline and flush the sequence
    /// after [`Reedline::with_keyseq_timeout`](crate::Reedline::with_keyseq_timeout)
    /// expires. Modes without sequence support keep the default `false`.
    fn has_pending_sequence(&self) -> bool {
        false
    }

    /// Drop a half-typed sequence without resolving it.
    ///
    /// Called when a `read_line` starts, so a sequence abandoned by a break or
    /// a submit cannot leak into the next prompt. Modes without sequence
    /// support keep the default no-op.
    fn reset_pending_sequence(&mut self) {}

    /// Resolve a half-typed sequence after the engine's timeout expires.
    ///
    /// The mode applies its own fallback: emacs and vi insert replay the keys
    /// as single keys (self-inserting characters), vi normal replays them
    /// through its keybinding table. Returns [`ReedlineEvent::None`] when
    /// nothing is pending.
    fn flush_pending_sequence(&mut self) -> ReedlineEvent {
        ReedlineEvent::None
    }
}
