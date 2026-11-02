use std::{collections::HashMap, ffi::OsStr, mem, ops::ControlFlow, path::PathBuf};

use nu_ansi_term::{Color, Style};

#[cfg(feature = "bashisms")]
use crate::menu_functions::{parse_selection_char, ParseAction};
use crate::{enums::ReedlineRawEvent, external_printer::ExternalOutput, CursorConfig};
use {
    crate::{
        completion::{Completer, CompletionOrigin, CompletionStatus, DefaultCompleter},
        core_editor::{EditCommandStatus, Editor},
        edit_mode::{EditMode, Emacs},
        enums::{EventStatus, ReedlineEvent},
        highlighter::SimpleMatchHighlighter,
        hinter::Hinter,
        history::{
            FileBackedHistory, History, HistoryCursor, HistoryItem, HistoryItemId,
            HistoryNavigationQuery, HistorySessionId, SearchDirection, SearchFilter, SearchQuery,
        },
        painting::{
            escape_control, escape_control_keep_sgr, Painter, PainterSuspendedState, PromptLines,
            RenderSnapshot, W,
        },
        prompt::{PromptEditMode, PromptHistorySearchStatus},
        result::{ReedlineError, ReedlineErrorVariants},
        terminal_extensions::{
            bracketed_paste::BracketedPasteGuard,
            kitty::KittyProtocolGuard,
            semantic_prompt::{Osc133ClickEventsMarkers, SemanticPromptMarkers},
            tty_mode::TtyModeGuard,
        },
        utils::text_manipulation,
        AbbrExpandContext, AutoPairAction, AutoPairContext, AutoPairs, Direction, EditCommand,
        ExampleHighlighter, FindStop, Highlighter, LineBuffer, Menu, MenuEvent, MotionTarget,
        MouseButton, Prompt, PromptHistorySearch, ReedlineMenu, Signal, UndoBehavior,
        ValidationResult, Validator,
    },
    crossterm::{
        cursor::SetCursorStyle,
        event,
        event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        terminal, QueueableCommand,
    },
    std::{
        collections::VecDeque,
        fs::File,
        io,
        io::Result,
        io::Write,
        process::Command,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
        time::Instant,
        time::SystemTime,
    },
};

// The POLL_WAIT is used to specify for how long the POLL should wait for
// events, to accelerate the handling of paste or compound resize events. Having
// a POLL_WAIT of zero means that every single event is treated as soon as it
// arrives. This doesn't allow for the possibility of more than 1 event
// happening at the same time.
const POLL_WAIT: Duration = Duration::from_millis(100);
// Since a paste event is multiple `Event::Key` events happening at the same
// time, we specify how many events should be in the `crossterm_events` vector
// before it is considered a paste. 10 events is conservative enough.
const EVENTS_THRESHOLD: usize = 10;

/// Default maximum time Reedline will block on input before yielding control
/// for features that require periodic processing (e.g., external printer,
/// idle callback).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Default wait for the next key of a multi-key sequence, matching readline's
/// `keyseq-timeout`.
const DEFAULT_KEYSEQ_TIMEOUT: Duration = Duration::from_millis(500);

/// Determines if inputs should be used to extend the regular line buffer,
/// traverse the history in the standard prompt or edit the search string in the
/// reverse search
#[derive(Debug, PartialEq, Eq)]
enum InputMode {
    /// Regular input by user typing or previous insertion.
    /// Undo tracking is active
    Regular,
    /// Full reverse search mode with different prompt,
    /// editing affects the search string,
    /// suggestions are provided to be inserted in the line buffer
    HistorySearch,
    /// Hybrid mode indicating that history is walked through in the standard prompt
    /// Either bash style up/down history or fish style prefix search,
    /// Edits directly switch to [`InputMode::Regular`]
    HistoryTraversal,
    /// GNU non-incremental history search (search.c `noninc_search` —
    /// `RL_STATE_NSEARCH`): the line buffer collects the search string
    /// in-place under a one-character prompt, `Enter` runs a single
    /// search, `C-g` aborts and restores the saved line.
    /// State lives in [`Reedline::noninc_search`].
    NonIncrementalSearch,
}

/// Configuration for mouse click-to-cursor support.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MouseClickMode {
    /// Disable mouse click handling.
    #[default]
    Disabled,
    /// Enable mouse click handling without emitting OSC 133 markers.
    Enabled,
    /// Enable mouse click handling and emit OSC 133 markers with `click_events=1`.
    EnabledWithOsc133,
}

impl MouseClickMode {
    fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled | Self::EnabledWithOsc133)
    }
}

#[derive(Default)]
enum HistoryLastRun {
    Stored(HistoryItemId),
    /// Outlives its `read_line` for Up to recall, but only the call that
    /// submitted it (`current`) may update its context.
    Excluded {
        item: HistoryItem,
        current: bool,
        recalled: bool,
    },
    #[default]
    Empty,
}

impl HistoryLastRun {
    fn set_recalled(&mut self, recalled: bool) {
        if let Self::Excluded { recalled: slot, .. } = self {
            *slot = recalled;
        }
    }

    fn is_recalled(&self) -> bool {
        matches!(self, Self::Excluded { recalled: true, .. })
    }
}

/// `rl_yank_last_arg`'s repeat state (kill.c:653-672 — the function's
/// `static` locals, made per-engine): a consecutive `M-.` undoes the
/// previous insert and yanks the same argument from a progressively
/// older entry; a negative numeric arg flips the walk direction.
struct YankLastArgState {
    /// How many entries before the current history position to reach
    /// back — 0 is the immediately previous entry (GNU `history_skip`).
    history_skip: i64,
    /// Whether the first press carried an explicit argument
    /// (GNU `explicit_arg_p`): then `count` is the argument index,
    /// otherwise `'$'` (last) is yanked.
    explicit_arg: bool,
    /// The argument index the first press passed (GNU `count_passed`).
    count: i32,
    /// +1 walks older, -1 walks newer (GNU `direction`, flipped by a
    /// negative count on a repeat).
    direction: i64,
    /// Whether the previous press inserted — then a repeat first undoes
    /// it (GNU `undo_needed` → `rl_do_undo`).
    undo_needed: bool,
}

/// `rl_history_search_*`'s reinit data (search.c:648
/// `rl_history_search_reinit` — `_rl_history_search_pos`,
/// `_rl_history_search_len`, `_rl_history_search_flags`): kept while
/// consecutive `ReedlineEvent::HistorySearch` events of the same
/// `anchored` family continue the search.
struct HistoryLineSearch {
    /// Which GNU family is live — `true` for `history-search-*`
    /// (anchored prefix), `false` for `history-substr-search-*`.
    anchored: bool,
    /// The insertion point at reinit (GNU `_rl_history_search_len`):
    /// the needle is `buffer[..point]` then, an anchored match lands
    /// the point back on it, and a failed search restores it.
    search_len: usize,
}

/// The live state of a [`InputMode::NonIncrementalSearch`] — GNU's
/// `_rl_search_cxt` for `RL_SEARCH_NSEARCH` (search.c `_rl_nsearch_init`):
/// the line swapped out while the buffer collects the search string
/// (`_rl_saved_line_for_search`), the direction, the one-char prompt
/// (`_rl_make_prompt_for_search`), and the `SF_PATTERN` flag.
struct NonIncrementalSearchState {
    /// The line as it was before the search string began collecting
    /// (`_rl_saved_line_for_search`); restored on abort and before a
    /// commit runs.
    saved_buffer: String,
    /// `cxt->save_point` — restored on abort/failure.
    saved_insertion_point: usize,
    /// The glyph GNU substitutes for the prompt (`_rl_make_prompt_for_search`):
    /// `:` by default, `/`/`?` from vi command mode.
    prompt_char: char,
    /// Search direction — `Forward` toward newer entries, `Backward` older.
    direction: Direction,
    /// GNU `SF_PATTERN` (vi command mode): match with the
    /// `_hs_history_patsearch` glob instead of a plain substring.
    pattern: bool,
    /// `cxt->save_line`/`cxt->history_pos` (search.c:240-241): the
    /// history position when the search opened, where a commit on a
    /// fresh string starts from (`noninc_history_pos = save_line`,
    /// search.c:429).
    save_line: Option<HistoryItemId>,
    /// The vi mode the search was entered from — GNU reads the string
    /// through the vi-*insertion* keymap in command mode
    /// (`_rl_nsearch_init`, search.c:112) and restores the command
    /// keymap in `_rl_nsearch_fini`. `None` outside vi normal.
    saved_edit_mode: Option<PromptEditMode>,
}

/// Line editor engine
///
/// ## Example usage
/// ```no_run
/// use reedline::{Reedline, Signal, DefaultPrompt};
/// let mut line_editor = Reedline::create();
/// let prompt = DefaultPrompt::default();
///
/// let out = line_editor.read_line(&prompt).unwrap();
/// match out {
///    Signal::Success(content) => {
///        // process content
///    }
///    _ => {
///        eprintln!("Entry aborted!");
///
///    }
/// }
/// ```
pub struct Reedline {
    editor: Editor,

    // History
    history: Box<dyn History>,
    history_cursor: HistoryCursor,
    history_session_id: Option<HistorySessionId>,
    history_last_run: HistoryLastRun,
    history_exclusion_prefix: Option<String>,
    /// Last failed `history.save`, until [`Reedline::take_history_save_error`].
    history_save_error: Option<ReedlineError>,
    input_mode: InputMode,

    // State of the painter after a `ReedlineEvent::ExecuteHostCommand` was requested, used after
    // execution to decide if we can re-use the previous prompt or paint a new one.
    suspended_state: Option<PainterSuspendedState>,
    last_render_snapshot: Option<RenderSnapshot>,

    // Validator
    validator: Option<Box<dyn Validator>>,

    // Stdout
    painter: Painter,

    transient_prompt: Option<Box<dyn Prompt>>,

    // Edit Mode: Vi, Emacs
    edit_mode: Box<dyn EditMode>,
    /// Standbys: machines a [`ReedlineEvent::SwitchMode`] can swap into
    /// `edit_mode`, offered a target in registration order.
    standby_edit_modes: Vec<Box<dyn EditMode>>,

    // Provides the tab completions
    completer: Box<dyn Completer + Send>,
    quick_completions: bool,
    partial_completions: bool,
    persistent_menus: bool,
    // Completions owed to a menu activation the completer could not answer in time
    deferred_menu_completion: Option<DeferredMenuCompletion>,

    // Optional host hook that intercepts a lone `PasteSystem` paste
    // (see `crate::PasteInterceptor`). `PasteSystem` only exists under
    // `system_clipboard`, so the hook is gated on the same feature.
    #[cfg(feature = "system_clipboard")]
    paste_interceptor: Option<Arc<dyn crate::PasteInterceptor>>,

    // Optional host hook that classifies a rapid key-event stream as a paste
    // burst by timing, reclassifying embedded Enters to newlines and coalescing
    // chunks (see `crate::PasteBurstHook`).
    paste_burst: Option<Arc<dyn crate::PasteBurstHook>>,

    // Highlight the edit buffer
    highlighter: Box<dyn Highlighter>,

    // Style used for visual selection
    visual_selection_style: Style,
    /// A distinct style for the cell under the cursor inside a selection;
    /// `None` leaves the cell on `visual_selection_style`.
    visual_selection_cursor_style: Option<Style>,

    // Showcase hints based on various strategies (history, language-completion, spellcheck, etc)
    hinter: Option<Box<dyn Hinter>>,
    hide_hints: bool,

    // Use ansi coloring or not
    use_ansi_coloring: bool,

    // Automatically insert and manage configured character pairs.
    auto_pairs: Option<AutoPairs>,

    // Whether to enable mouse click-to-cursor functionality
    mouse_click_mode: MouseClickMode,

    // Current working directory as defined by the application. If set, it will
    // override the actual working directory of the process.
    cwd: Option<String>,

    // Engine Menus
    menus: Vec<ReedlineMenu>,

    abbreviations: HashMap<String, String>,

    // Text editor used to open the line buffer for editing
    buffer_editor: Option<BufferEditor>,

    // Use different cursors depending on the current edit mode
    cursor_shapes: Option<CursorConfig>,

    // Manage bracketed paste mode
    bracketed_paste: BracketedPasteGuard,

    // Manage optional kitty protocol
    kitty_protocol: KittyProtocolGuard,

    // Whether lines should be accepted immediately
    immediately_accept: bool,

    // Keystrokes queued by [`ReedlineEvent::KeySequence`], consumed at the
    // head of the next input batch before live terminal input — the
    // readline macro buffer (GNU readline.c rl_executing_macro / push_back
    // onto the input stream). Bounded by [`Self::MAX_REPLAYED_KEYS`].
    // Each entry carries its macro nesting depth (GNU MAX_MACRO_LEVEL
    // semantics): keys a live keypress enqueues start at depth 1; a
    // KeySequence inside replayed input nests one deeper, and a macro
    // that would pass MAX_MACRO_NESTING is dropped — GNU aborts the
    // same way instead of spinning on self-recursive sequences.
    pending_input: VecDeque<(Event, u16)>,

    // Depth of each event in the batch currently being parsed, in
    // order — populated when pending_input is merged ahead of live
    // input (live events are depth 0). Consumed as the batch is
    // collected and again as reedline_events are handled.
    event_replay_depths: VecDeque<u16>,

    // Already-parsed events that followed a host-command suspension in
    // the same input batch. GNU's `_rl_abort_internal` longjmps out of
    // the current command (util.c:99) but the bytes behind it are still
    // in readline's input buffer, so the next command consumes them —
    // `C-g X` leaves `X` to insert. The engine has already read those
    // keys, so they are parked here and run head-of-queue when the host
    // resumes the editor, before fresh terminal input.
    pending_events: VecDeque<(ReedlineEvent, u16)>,

    // `ReedlineEvent::QuotedInsert` armed: the next key press is consumed
    // as literal data instead of dispatching through the keymap (GNU
    // readline.c rl_quoted_insert — one input character verbatim).
    quoted_insert: bool,

    // Set when the event currently being dispatched ran at least one
    // `EditCommand` (which carries its own kill-ring bookkeeping inside
    // `Editor::run_edit_command`). `handle_event` consults it afterward:
    // a handled event that ran no edit still clears the editor's
    // `_rl_last_command_was_kill`/`rl_last_func` records (readline.c:715-716).
    edit_dispatched: bool,

    // `ReedlineEvent::CharSearch` armed: the next `Char` key press is
    // consumed as the character to find instead of dispatching through
    // the keymap (GNU text.c _rl_char_search — one input char is the
    // search target; a non-Char key cancels).
    pending_char_search: Option<Direction>,

    // `rl_last_func == rl_yank_last_arg` tracking: the state a
    // consecutive `ReedlineEvent::YankLastArg` resumes — cleared by any
    // other event (and at each `read_line` start).
    yank_last_arg: Option<YankLastArgState>,

    // The history entry the line was last recalled to, this `read_line`
    // — reedline's `where_history()` for `OperateAndGetNext` and
    // `YankLastArg`. `None` is the live line (fresh or walked back to).
    history_recall_pos: Option<HistoryItemId>,

    // `ReedlineEvent::OperateAndGetNext`'s cross-`read_line` intent
    // (GNU `saved_history_logical_offset`, misc.c:770-789): `Some(id)`
    // preloads the entry after `id`; `None` inside means the line was
    // fresh and nothing is preloaded.
    operate_and_get_next: Option<Option<HistoryItemId>>,

    // Live `rl_history_search_*`/`rl_history_substr_search_*` state —
    // present only while consecutive `ReedlineEvent::HistorySearch`
    // events continue one search (GNU's `rl_last_func` reinit checks,
    // search.c:701,737,755).
    history_line_search: Option<HistoryLineSearch>,

    // Live `RL_STATE_NSEARCH` context — `Some` only in
    // `InputMode::NonIncrementalSearch` (search.c `_rl_nscxt`).
    noninc_search: Option<NonIncrementalSearchState>,

    // The last string a non-incremental search committed (GNU's static
    // `noninc_search_string`, search.c:60): an empty-string commit and
    // the `-again` variants reuse it.
    noninc_search_string: Option<String>,

    // The history position `noninc_history_pos` (search.c:61): the
    // entry a committed search last landed on, which `-again` repeats
    // and an empty-string commit resume from. `None` is the live line.
    noninc_search_pos: Option<HistoryItemId>,

    // Copies of the quoted character a pending numeric argument asked
    // for when `QuotedInsert` was dispatched — GNU passes its count to
    // `_rl_insert_char` (text.c:1056). 1 without an argument.
    quoted_insert_count: usize,

    // Keyboard-macro recording (GNU `RL_STATE_MACRODEF` +
    // `current_macro`, macro.c): `Some` once
    // `ReedlineEvent::StartKbdMacro` has armed it — every key press is
    // appended BEFORE it is parsed (readline.c:907 `_rl_add_macro_char`
    // runs ahead of dispatch), so the recording holds keystrokes, not
    // commands, and replays through whatever keymap is live then.
    recording: Option<Vec<crate::KeyCombination>>,

    // The recording `ReedlineEvent::EndKbdMacro` last finished (GNU
    // `current_macro` after `rl_end_kbd_macro`, macro.c:270);
    // `CallLastKbdMacro` replays it through `enqueue_replayed_keys`.
    last_macro: Vec<crate::KeyCombination>,

    // Numeric argument accumulating (GNU `RL_STATE_NUMERICARG`,
    // misc.c): set by `ReedlineEvent::DigitArgument` /
    // `UniversalArgument` at parse time, spent by the next key that is
    // neither — GNU reads the argument inside `rl_digit_loop` and
    // dispatches the terminating key with the count (misc.c:174).
    pending_arg: Option<PendingArg>,

    // Nesting depth of the event currently being handled — set from the
    // pending_input tag for replayed keys, 0 for live input. A
    // KeySequence handled at depth d enqueues at d+1.
    replay_depth: u16,

    // External break signal: when set to `true`, `read_line()` will return
    // `Signal::ExternalBreak` with the current buffer contents.
    break_signal: Option<Arc<AtomicBool>>,

    // External repaint signal: when triggered, the prompt is re-evaluated
    // and repainted in place while `read_line()` is running.
    repaint_signal: Option<RepaintSignal>,

    // Maximum time to block on input before yielding control for features that
    // require periodic processing (external printer, idle callback).
    // Only used when external_printer or idle_callback is configured.
    poll_interval: Duration,

    // How long to wait for the next key of a multi-key sequence before
    // flushing the pending prefix.
    keyseq_timeout: Duration,

    // Deadline for the pending sequence, set lazily when one starts and reset
    // after every key that arrives.
    keyseq_deadline: Option<Instant>,

    external_printer: Option<Box<dyn ExternalOutput>>,

    // Callback function that is called periodically while waiting for input.
    // Useful for processing external events (e.g., GUI updates) during idle time.
    idle_callback: Option<Box<dyn FnMut() + Send>>,
}

/// A numeric argument mid-accumulation — GNU `misc.c` `rl_numeric_arg`,
/// `rl_arg_sign`, `rl_explicit_arg` and the `_rl_argcxt` bits the state
/// machine uses.
#[derive(Debug, Clone, Copy)]
struct PendingArg {
    /// `rl_numeric_arg` — magnitude only; the sign lives in `sign`.
    value: i64,
    /// `rl_arg_sign`: -1 once `M--` has negated the argument.
    sign: i64,
    /// `rl_explicit_arg`: a digit was seen. Stays unset through
    /// `universal-argument` keys, which is why `C-u 5` is 5, not 20
    /// (`misc.c:152`).
    explicit: bool,
    /// `_rl_argcxt & NUM_SAWDIGITS`: a `universal-argument` key after
    /// digits ends the argument instead of multiplying (`misc.c:121`).
    saw_digits: bool,
    /// GNU's `C-u`-after-digits branch (`misc.c:121-133`): the
    /// `universal-argument` key is ignored and the NEXT key dispatches
    /// under the count no matter what it parses to — even a digit,
    /// which then seeds a fresh argument.
    dispatch_next: bool,
}

impl PendingArg {
    fn fresh() -> Self {
        PendingArg {
            value: 1,
            sign: 1,
            explicit: false,
            saw_digits: false,
            dispatch_next: false,
        }
    }

    /// `rl_numeric_arg * rl_arg_sign` — the count a dispatched function
    /// receives (`readline.c:941`).
    fn count(&self) -> i64 {
        self.value * self.sign
    }

    /// `rl_explicit_arg`, including GNU's `M--`-is-`-1` upgrade
    /// (`misc.c:170`): a bare negative sign means an explicit `-1`.
    fn explicit(&self) -> bool {
        self.explicit || self.sign < 0
    }
}

struct BufferEditor {
    command: Command,
    temp_file: PathBuf,
    is_template: bool,
}

impl BufferEditor {
    const FILE: &str = "{file}";
    const LINE: &str = "{line}";
    const COL: &str = "{col}";

    fn new(mut command: Command, temp_file: PathBuf) -> BufferEditor {
        let mut has_file_arg = false;
        let mut is_template = false;

        for arg in command.get_args() {
            has_file_arg |= arg == temp_file.as_os_str();

            if let Some(arg) = arg.to_str() {
                has_file_arg |= arg.contains(BufferEditor::FILE);

                is_template |= arg.contains(BufferEditor::FILE)
                    || arg.contains(BufferEditor::LINE)
                    || arg.contains(BufferEditor::COL);
            }
        }

        if !has_file_arg {
            command.arg(&temp_file);
        }

        BufferEditor {
            command,
            temp_file,
            is_template,
        }
    }

    /// renders the editor command template,
    /// substituting template placeholders where present.
    pub(crate) fn render_command(&self, line_buffer: &LineBuffer) -> Command {
        let mut rendered = Command::new(self.command.get_program());

        for (key, value) in self.command.get_envs() {
            match value {
                Some(value) => rendered.env(key, value),
                None => rendered.env_remove(key),
            };
        }

        if let Some(dir) = self.command.get_current_dir() {
            rendered.current_dir(dir);
        }

        let file = self.temp_file.to_string_lossy();
        let line = (line_buffer.line() + 1).to_string();
        let col = (line_buffer.col() + 1).to_string();

        for arg in self.command.get_args().map(OsStr::to_string_lossy) {
            let arg = arg
                .replace(Self::FILE, &file)
                .replace(Self::LINE, &line)
                .replace(Self::COL, &col);

            rendered.arg(arg);
        }

        rendered
    }

    /// writes the buffer to the temp file,
    /// in preparation for spawning the buffer editor
    pub(crate) fn write_current_buffer(&self, buffer_contents: &str) -> Result<()> {
        let mut file = File::create(&self.temp_file)?;
        write!(file, "{buffer_contents}")
    }

    /// reads the buffer from the temp file,
    /// expected to be called after the buffer editor exits
    pub(crate) fn get_edited_buffer(&self) -> Result<String> {
        let mut res = std::fs::read_to_string(&self.temp_file)?;
        let content_len = res.trim_end().len();
        res.truncate(content_len);
        Ok(res)
    }
}

/// The completions the [`Menu`](ReedlineEvent::Menu) event could not decide, because the
/// completer had not answered yet.
///
/// Activating a menu immediately inspects its values twice: quick completions accept a
/// lone suggestion, and partial completions splice in the prefix the suggestions share.
/// A completer computing in the background can answer neither yet.
///
/// The snapshot pins the editor state as of the keystroke this was armed on: a result
/// landing after the user has typed on is discarded rather than rewriting the line
/// underneath them.
struct DeferredMenuCompletion {
    menu: String,
    /// The line this was armed on, stamped as a completer stamps its own results.
    origin: CompletionOrigin,
}

impl DeferredMenuCompletion {
    fn new(menu: &ReedlineMenu, editor: &Editor) -> Self {
        Self {
            menu: menu.name().to_string(),
            origin: CompletionOrigin::new(editor.get_buffer(), editor.insertion_point()),
        }
    }

    /// Whether the line is still exactly as it was when this was armed, on the same menu.
    /// Anything else means the user moved on and the decision is void.
    fn still_applies(&self, menu: &ReedlineMenu, editor: &Editor) -> bool {
        self.menu == menu.name()
            && self
                .origin
                .matches(editor.get_buffer(), editor.insertion_point())
    }
}

/// Call [`request_repaint`](RepaintSignal::request_repaint) once
/// new prompt data is ready! The next iteration of the input loop re-evaluates
/// the [`Prompt`] and redraws it without interrupting the current line edit.
#[derive(Clone, Debug, Default)]
pub struct RepaintSignal {
    flag: Arc<AtomicBool>,
}

impl RepaintSignal {
    /// Request that the prompt is re-evaluated and repainted in place.
    pub fn request_repaint(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Consume a pending repaint request.
    fn take(&self) -> bool {
        self.flag.swap(false, Ordering::Relaxed)
    }
}

impl Drop for Reedline {
    fn drop(&mut self) {
        if self.cursor_shapes.is_some() {
            let _ignore = terminal::enable_raw_mode();
            let mut stdout = std::io::stdout();
            let _ignore = stdout.queue(SetCursorStyle::DefaultUserShape);
            let _ignore = stdout.flush();
        }
        let _ignore = self.painter.show_cursor();

        // Ensures that the terminal is in a good state if we panic semigracefully
        // Calling `disable_raw_mode()` twice is fine with Linux
        let _ignore = terminal::disable_raw_mode();
    }
}

/// Mark the painter's cached prompt anchor stale around a menu running its completer,
/// which may have left the cached row pointing at content that has scrolled. See
/// [`ReedlineMenu::queries_host_completer`] for why only some completers count, and
/// #1130 for the bug.
///
/// Also skipped for a menu the same keystroke deactivated, whose queued event nothing
/// goes on to consume.
///
/// Call this from every event that reaches the completer, which is not the same set as
/// the events that change the menu's selection: `MenuNext` splices a partial completion
/// and queries, while `MenuPrevious` only moves and does not.
fn invalidate_anchor_if_host_completer_runs(menu: &ReedlineMenu, painter: &mut Painter) {
    if menu.is_active() && menu.queries_host_completer() {
        painter.invalidate_prompt_start_row();
    }
}

/// The char a key event contributes to a paste burst: a char press under the
/// modifiers that type text, AltGr's Ctrl-Alt included, since ConPTY reports
/// a pasted `@` or `{` on such layouts that way.
///
/// Release events are skipped. With the kitty keyboard enhancement every key
/// also yields a Release, which reedline's own `try_from` drops, so counting
/// both would double every char.
fn burst_char(event: &Event) -> Option<char> {
    match event {
        Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers,
            kind,
            ..
        }) if *kind != KeyEventKind::Release && crate::edit_mode::is_text_char(*modifiers) => {
            Some(*c)
        }
        _ => None,
    }
}

/// The literal character a key press contributes under quoted-insert
/// (GNU `rl_quoted_insert` — one input character verbatim): a `Char`
/// inserts itself (control-modified forms insert their control byte),
/// `Enter` is `\r`, `Tab` is `\t`, `Backspace` is DEL, `Esc` is ESC.
/// Every other key is consumed with no insert — GNU reads exactly one
/// input character and stops.
/// `_hs_history_patsearch` (histsearch.c:245): whether `pat` matches
/// `line` as a glob — `*`, `?`, `[...]` (with `!`/`^` negation and
/// `-` ranges), `\` escaping the next char. Unanchored patterns get an
/// implicit `*` on both ends, anchored (`^`-prefixed) ones only at the
/// tail (histsearch.c:265-283). A trailing unescaped `\` rejects the
/// pattern outright (histsearch.c:256-263).
fn noninc_pat_search(pat: &str, line: &str, anchored: bool) -> bool {
    let mut trailing = pat.len();
    let mut unescaped = false;
    while trailing > 0 && pat.as_bytes()[trailing - 1] == b'\\' {
        unescaped = !unescaped;
        trailing -= 1;
    }
    if unescaped {
        return false;
    }
    let pat = if anchored {
        format!("{pat}*")
    } else {
        format!("*{pat}*")
    };
    glob_match(&pat, line)
}

/// Small glob matcher for [`noninc_pat_search`] — `*`, `?`, `[...]`
/// classes with leading `!`/`^` negation, `a-z` ranges, `\` quoting.
/// Operates on chars, so multibyte text matches `?` per character.
fn glob_match(pat: &str, text: &str) -> bool {
    fn at(chars: &[char], i: usize) -> Option<char> {
        chars.get(i).copied()
    }
    fn matches(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' => (0..=t.len()).any(|i| matches(&p[1..], &t[i..])),
            '?' => !t.is_empty() && matches(&p[1..], &t[1..]),
            '\\' => p.len() > 1 && t.first() == p.get(1) && matches(&p[2..], &t[1..]),
            '[' => {
                let Some(&c) = t.first() else { return false };
                let (negate, mut i) = match at(p, 1) {
                    Some('!') | Some('^') => (true, 2),
                    _ => (false, 1),
                };
                // A leading `]` inside a class is a literal member.
                let mut matched = false;
                let mut first = true;
                loop {
                    match at(p, i) {
                        Some(']') if !first => break,
                        None => return false, // unclosed class: no match
                        _ => {}
                    }
                    first = false;
                    // `a-z` ranges; `\` escapes a class char.
                    let lo = if at(p, i) == Some('\\') && at(p, i + 1).is_some() {
                        i += 1;
                        at(p, i)
                    } else {
                        at(p, i)
                    };
                    if at(p, i + 1) == Some('-') && at(p, i + 2).is_some_and(|hi| hi != ']') {
                        let hi = at(p, i + 2).unwrap();
                        if lo.is_some_and(|lo| lo <= c && c <= hi) {
                            matched = true;
                        }
                        i += 3;
                    } else {
                        if lo == Some(c) {
                            matched = true;
                        }
                        i += 1;
                    }
                }
                if matched != negate {
                    matches(&p[i + 1..], &t[1..])
                } else {
                    false
                }
            }
            c => t.first() == Some(&c) && matches(&p[1..], &t[1..]),
        }
    }
    matches(
        &pat.chars().collect::<Vec<_>>(),
        &text.chars().collect::<Vec<_>>(),
    )
}

/// `history_tokenize` (histexpand.c:1692 → `history_tokenize_internal`
/// :1638 + `history_tokenize_word` :1483): split a command line the
/// way `!`-expansion sees words — `fielddelim` whitespace separates
/// fields, `()<>;&|` form operator tokens (with `<<`/`<<-`/`<<<`/`>>`
/// `&>`/`>&`/`|>`/`n>`/`>&n`/`<&n-` spellings), `"`/`'`/`` ` `` quote
/// with `\` escapes, `$(...)`/`<(...)`/`>(...)` and `!`/`@`/`?`/`+`
/// extended globs nest to their `)`. Quotes stay inside the tokens —
/// `history_arg_extract` slices verbatim, so `yank-last-arg` on
/// `echo "two words"` yanks `"two words"`.
fn history_tokenize(string: &str) -> Vec<String> {
    /// `fielddelim` (histexpand.c:58) — `whitespace` + `\n`.
    fn fielddelim(c: u8) -> bool {
        matches!(c, b' ' | b'\t' | b'\n' | b'\r')
    }
    const WORD_DELIMS: &[u8] = b" \t\n;&()|<>";
    const QUOTES: &[u8] = b"\"'`";
    let s = string.as_bytes();

    /// `get_word` (histexpand.c:1536): scan a word starting at
    /// `start`, honoring quotes, backslash escapes and `(...)` nests.
    fn scan_word(s: &[u8], start: usize) -> usize {
        let at = |i: usize| *s.get(i).unwrap_or(&0);
        let mut i = start;
        let mut delimiter = 0u8;
        let mut nestdelim = 0i32;
        let mut delimopen = 0u8;
        if QUOTES.contains(&at(i)) {
            delimiter = at(i);
            i += 1;
        }
        while at(i) != 0 {
            if at(i) == b'\\' && at(i + 1) == b'\n' {
                i += 2;
                continue;
            }
            // histexpand.c:1564: `\\` escapes outside `'...'`; inside
            // `"..."` GNU tests `string[i]` — the backslash itself —
            // against `slashify_in_quotes`, which is always true, so
            // the escape is unconditional there too.
            if at(i) == b'\\' && delimiter != b'\'' {
                i += 1;
                if at(i) == 0 {
                    break;
                }
                i += 1;
                continue;
            }
            if nestdelim > 0 && at(i) == delimopen {
                nestdelim += 1;
                i += 1;
                continue;
            }
            if nestdelim > 0 && at(i) == delimiter {
                nestdelim -= 1;
                if nestdelim == 0 {
                    delimiter = 0;
                }
                i += 1;
                continue;
            }
            if delimiter != 0 && at(i) == delimiter {
                delimiter = 0;
                i += 1;
                continue;
            }
            // Command/process substitution and extended globs
            // (histexpand.c:1591): `<(`, `>(`, `$(`, `!(`, `@(`, `?(`,
            // `+(` — GNU's set also has `*(` folded into `*`.
            if nestdelim == 0
                && delimiter == 0
                && matches!(at(i), b'<' | b'>' | b'$' | b'!' | b'@' | b'?' | b'+' | b'*')
                && at(i + 1) == b'('
            {
                i += 1;
                if at(i + 1) == 0 {
                    break;
                }
                delimopen = b'(';
                delimiter = b')';
                nestdelim = 1;
                i += 1;
                continue;
            }
            if delimiter == 0 && WORD_DELIMS.contains(&at(i)) {
                break;
            }
            if delimiter == 0 && QUOTES.contains(&at(i)) {
                delimiter = at(i);
            }
            i += 1;
        }
        i
    }

    /// `history_tokenize_word` (histexpand.c:1483): the token starting
    /// at `start` — one word, an operator, or a lone `(`/`)`/`\n`.
    fn tokenize_word(s: &[u8], start: usize) -> usize {
        let at = |i: usize| *s.get(i).unwrap_or(&0);
        let mut i = start;
        if matches!(at(i), b'(' | b')' | b'\n') {
            return i + 1;
        }
        if at(i).is_ascii_digit() {
            let mut j = i;
            while at(j).is_ascii_digit() {
                j += 1;
            }
            if at(j) == 0 {
                return j;
            }
            if at(j) == b'<' || at(j) == b'>' {
                i = j; // digits are a file descriptor
            } else {
                return scan_word(s, j);
            }
        }
        if matches!(at(i), b'<' | b'>' | b';' | b'&' | b'|') {
            let peek = at(i + 1);
            if peek == at(i) {
                if peek == b'<' && matches!(at(i + 2), b'-' | b'<') {
                    return i + 3; // `<<-`, `<<<`
                }
                return i + 2;
            }
            if peek == b'&' && matches!(at(i), b'>' | b'<') {
                // `<&[digits][-]`, `>&[digits][-]`
                let mut j = i + 2;
                while at(j).is_ascii_digit() {
                    j += 1;
                }
                if at(j) == b'-' {
                    j += 1;
                }
                return j;
            }
            if (peek == b'>' && at(i) == b'&') || (peek == b'|' && at(i) == b'>') {
                return i + 2; // `&>`, `>|`
            }
            if peek == b'(' && matches!(at(i), b'>' | b'<') {
                return scan_word(s, i + 2); // process substitution
            }
            return i + 1;
        }
        scan_word(s, i)
    }

    let mut result = Vec::new();
    let mut i = 0usize;
    while i < s.len() {
        while i < s.len() && fielddelim(s[i]) {
            i += 1;
        }
        if i >= s.len() {
            break;
        }
        // `history_comment_char` is `'\0'` here, GNU's unset default:
        // the comment check can never fire (histexpand.c:1656).
        let start = i;
        i = tokenize_word(s, start);
        if i == start {
            // A delimiter the word scan refused (histexpand.c:1666):
            // emit it plus its delimiter run as one token.
            i += 1;
            while i < s.len() && WORD_DELIMS.contains(&s[i]) {
                i += 1;
            }
        }
        if let Some(token) = string.get(start..i) {
            result.push(token.to_string());
        }
    }
    result
}

fn quoted_insert_char(event: &Event) -> Option<char> {
    if let Event::Key(KeyEvent {
        code,
        modifiers,
        kind,
        ..
    }) = event
    {
        if *kind == KeyEventKind::Release {
            return None;
        }
        return match code {
            KeyCode::Char(c) if modifiers.contains(KeyModifiers::CONTROL) => {
                char::from_u32((*c as u32) & 0x1f)
            }
            KeyCode::Char(c) => Some(*c),
            KeyCode::Enter => Some('\r'),
            KeyCode::Tab => Some('\t'),
            KeyCode::Backspace => Some('\u{7f}'),
            KeyCode::Esc => Some('\u{1b}'),
            _ => None,
        };
    }
    None
}

/// A key a fresh emacs / vi-insert keymap resolves to `self-insert`: a
/// printable character without a chord modifier. GNU `_rl_insert_typein`
/// (input.c:464) keeps consuming these while they wait in the input
/// buffer, inserting the run with one `rl_insert_text`; the keys after
/// the first never reach `_rl_dispatch_subseq`, so they never reach
/// `_rl_add_macro_char` either.
fn is_typein_key(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char(_),
            modifiers,
            kind,
            ..
        }) if *kind != KeyEventKind::Release && crate::edit_mode::is_text_char(*modifiers)
    )
}

/// A key press that stands for a newline in pasted text: a bare `Enter` (CR),
/// or `Ctrl-J`, which is how a raw LF arrives in raw mode.
fn is_pasted_newline(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            ..
        }) | Event::Key(KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            ..
        })
    )
}

impl Reedline {
    /// Create a new [`Reedline`] engine with a local [`History`] that is not synchronized to a file.
    #[must_use]
    pub fn create() -> Self {
        let history = Box::<FileBackedHistory>::default();
        #[cfg(not(test))]
        let painter = Painter::new(W::terminal());
        #[cfg(test)]
        let painter = Painter::new(W::sink());
        let buffer_highlighter = Box::<ExampleHighlighter>::default();
        let visual_selection_style = Style::new().on(Color::LightGray);
        let completer = Box::<DefaultCompleter>::default();
        let hinter = None;
        let validator = None;
        let edit_mode = Box::<Emacs>::default();
        let hist_session_id = None;

        Reedline {
            editor: Editor::default(),
            history,
            history_cursor: HistoryCursor::new(
                HistoryNavigationQuery::Normal(LineBuffer::default()),
                hist_session_id,
            ),
            history_session_id: hist_session_id,
            history_last_run: HistoryLastRun::Empty,
            history_exclusion_prefix: None,
            history_save_error: None,
            input_mode: InputMode::Regular,
            suspended_state: None,
            last_render_snapshot: None,
            painter,
            transient_prompt: None,
            edit_mode,
            standby_edit_modes: Vec::new(),
            completer,
            quick_completions: false,
            partial_completions: false,
            persistent_menus: false,
            deferred_menu_completion: None,
            #[cfg(feature = "system_clipboard")]
            paste_interceptor: None,
            paste_burst: None,
            highlighter: buffer_highlighter,
            visual_selection_style,
            visual_selection_cursor_style: None,
            hinter,
            hide_hints: false,
            validator,
            use_ansi_coloring: true,
            auto_pairs: None,
            mouse_click_mode: MouseClickMode::default(),
            cwd: None,
            menus: Vec::new(),
            abbreviations: HashMap::new(),
            buffer_editor: None,
            cursor_shapes: None,
            bracketed_paste: BracketedPasteGuard::default(),
            kitty_protocol: KittyProtocolGuard::default(),
            immediately_accept: false,
            pending_input: VecDeque::new(),
            event_replay_depths: VecDeque::new(),
            pending_events: VecDeque::new(),
            quoted_insert_count: 1,
            recording: None,
            last_macro: Vec::new(),
            pending_arg: None,
            quoted_insert: false,
            edit_dispatched: false,
            pending_char_search: None,
            yank_last_arg: None,
            history_recall_pos: None,
            noninc_search: None,
            noninc_search_string: None,
            noninc_search_pos: None,
            operate_and_get_next: None,
            history_line_search: None,
            replay_depth: 0,
            break_signal: None,
            repaint_signal: None,
            poll_interval: DEFAULT_POLL_INTERVAL,
            keyseq_timeout: DEFAULT_KEYSEQ_TIMEOUT,
            keyseq_deadline: None,
            external_printer: None,
            idle_callback: None,
        }
    }

    /// Get a new history session id based on the current time and the first commit datetime of reedline
    pub fn create_history_session_id() -> Option<HistorySessionId> {
        let nanos = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            Ok(n) => n.as_nanos() as i64,
            Err(_) => 0,
        };

        Some(HistorySessionId::new(nanos))
    }

    /// Toggle whether reedline enables bracketed paste to reed copied content
    ///
    /// This currently alters the behavior for multiline pastes as pasting of regular text will
    /// execute after every complete new line as determined by the [`Validator`]. With enabled
    /// bracketed paste all lines will appear in the buffer and can then be submitted with a
    /// separate enter.
    ///
    /// At this point most terminals should support it or ignore the setting of the necessary
    /// flags. For full compatibility, keep it disabled.
    pub fn use_bracketed_paste(mut self, enable: bool) -> Self {
        self.bracketed_paste.set(enable);
        self
    }

    /// Toggle whether reedline uses the kitty keyboard enhancement protocol
    ///
    /// This allows us to disambiguate more events than the traditional standard
    /// Only available with a few terminal emulators.
    /// You can check for that with [`crate::kitty_protocol_available`]
    /// `Reedline` will perform this check internally
    ///
    /// Read more: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>
    pub fn use_kitty_keyboard_enhancement(mut self, enable: bool) -> Self {
        self.kitty_protocol.set(enable);
        self
    }

    /// Return the previously generated history session id
    pub fn get_history_session_id(&self) -> Option<HistorySessionId> {
        self.history_session_id
    }

    /// Set a new history session id
    /// This should be used in situations where the user initially did not have a history_session_id
    /// and then later realized they want to have one without restarting the application.
    pub fn set_history_session_id(&mut self, session: Option<HistorySessionId>) -> Result<()> {
        self.history_session_id = session;
        Ok(())
    }

    /// A builder to include a [`Hinter`] in your instance of the Reedline engine
    /// # Example
    /// ```rust
    /// //Cargo.toml
    /// //[dependencies]
    /// //nu-ansi-term = "*"
    /// use {
    ///     nu_ansi_term::{Color, Style},
    ///     reedline::{DefaultHinter, Reedline},
    /// };
    ///
    /// let mut line_editor = Reedline::create().with_hinter(Box::new(
    ///     DefaultHinter::default()
    ///     .with_style(Style::new().italic().fg(Color::LightGray)),
    /// ));
    /// ```
    #[must_use]
    pub fn with_hinter(mut self, hinter: Box<dyn Hinter>) -> Self {
        self.hinter = Some(hinter);
        self
    }

    /// Remove current [`Hinter`]
    #[must_use]
    pub fn disable_hints(mut self) -> Self {
        self.hinter = None;
        self
    }

    /// A builder to configure the tab completion
    /// # Example
    /// ```rust
    /// // Create a reedline object with tab completions support
    ///
    /// use reedline::{DefaultCompleter, Reedline};
    ///
    /// let commands = vec![
    ///   "test".into(),
    ///   "hello world".into(),
    ///   "hello world reedline".into(),
    ///   "this is the reedline crate".into(),
    /// ];
    /// let completer = Box::new(DefaultCompleter::new_with_wordlen(commands.clone(), 2));
    ///
    /// let mut line_editor = Reedline::create().with_completer(completer);
    /// ```
    #[must_use]
    pub fn with_completer(mut self, completer: Box<dyn Completer + Send>) -> Self {
        self.completer = completer;
        self
    }

    /// Turn on quick completions. These completions will auto-select if the completer
    /// ever narrows down to a single entry.
    #[must_use]
    pub fn with_quick_completions(mut self, quick_completions: bool) -> Self {
        self.quick_completions = quick_completions;
        self
    }

    /// Install a paste interceptor. When set, a lone
    /// `EditCommand::PasteSystem` (Ctrl+Shift+V by default) calls
    /// [`PasteInterceptor::on_paste`](crate::PasteInterceptor::on_paste)
    /// instead of the default clipboard-read-and-insert, and reedline inserts
    /// whatever [`PasteAction`](crate::PasteAction) the hook returns. Requires
    /// the `system_clipboard` feature, which is what defines `PasteSystem`.
    #[cfg(feature = "system_clipboard")]
    #[must_use]
    pub fn with_paste_interceptor(mut self, interceptor: Arc<dyn crate::PasteInterceptor>) -> Self {
        self.paste_interceptor = Some(interceptor);
        self
    }

    /// Install a paste-burst timing hook. When set, the read loop feeds each
    /// just-read plain char to the hook at read time, keeps draining while
    /// [`PasteBurstHook::is_burst_active`](crate::PasteBurstHook::is_burst_active)
    /// is true, and reclassifies a bare `Enter` to an inserted newline when
    /// [`PasteBurstHook::enter_is_newline`](crate::PasteBurstHook::enter_is_newline)
    /// returns true. When unset, the read loop behaves exactly as before.
    #[must_use]
    pub fn with_paste_burst(mut self, hook: Arc<dyn crate::PasteBurstHook>) -> Self {
        self.paste_burst = Some(hook);
        self
    }

    /// Control whether the cursor crosses line boundaries on left/right motions
    /// in a block caret (vi normal/visual mode). When `true` (the default), `l`
    /// at the end of a line moves to the next line's first character and `h` at
    /// column 0 to the previous line's last; when `false`, both stop at the line
    /// edge (vim's default `h`/`l`). Has no effect on emacs or vi insert mode,
    /// whose bar caret always moves freely across lines.
    ///
    /// Scope: this steers where the **caret rests** on `h`/`l`, not how far an
    /// operator reaches. Operator motions (`d`/`c`/`y`) delete the literal
    /// grapheme span regardless of this flag, so e.g. `dl` always deletes the
    /// char under the caret and never the line break.
    #[must_use]
    pub fn with_cross_line_cursor(mut self, cross_line_cursor: bool) -> Self {
        self.editor.set_cross_line_cursor(cross_line_cursor);
        self
    }

    /// Turn on partial completions. These completions will fill the buffer with the
    /// smallest common string from all the options
    #[must_use]
    pub fn with_partial_completions(mut self, partial_completions: bool) -> Self {
        self.partial_completions = partial_completions;
        self
    }

    /// Make active menus persist while the line is edited: erasing characters
    /// or emptying the line refilters the menu instead of dismissing it.
    ///
    /// When disabled (the default), an active menu is deactivated by a backspace
    /// when quick completions are on, and by any edit that leaves the line
    /// buffer empty. A persistent menu still closes on Esc, Ctrl-C, or when a
    /// value is accepted.
    #[must_use]
    pub fn with_persistent_menus(mut self, persistent_menus: bool) -> Self {
        self.persistent_menus = persistent_menus;
        self
    }

    /// Whether ANSI coloring should be used for the current terminal.
    ///
    /// ANSI coloring requires both the Reedline configuration to enable it and
    /// terminal support; `TERM=dumb` takes precedence over `with_ansi_colors(true)`.
    /// The painter holds the terminal policy, so the colors and the escapes it
    /// skips cannot disagree.
    fn effective_ansi_coloring(&self) -> bool {
        self.use_ansi_coloring && !self.painter.term_is_dumb()
    }

    /// A builder which enables or disables the use of ansi coloring in the prompt
    /// and in the command line syntax highlighting.
    #[must_use]
    pub fn with_ansi_colors(mut self, use_ansi_coloring: bool) -> Self {
        self.use_ansi_coloring = use_ansi_coloring;
        self
    }

    /// A builder that configures automatic pair insertion for the Reedline engine.
    ///
    /// By default, auto-pairing applies at every position for the configured pairs.
    /// To suppress it in certain syntactic positions (e.g. string literals, or in the
    /// middle of a word), override [`Highlighter::should_auto_pair`].
    ///
    /// For `InsertChar`, the closer is looked up before the opener. A character that
    /// is registered as both a closer of one pair and an opener of another therefore
    /// resolves based on whether that closer currently sits at the cursor, not on the
    /// order the pairs were passed to [`AutoPairs::new`].
    ///
    /// This builder does not touch the terminal's bracketed paste setting.
    /// If bracketed paste is enabled (see [`Self::use_bracketed_paste`]), a paste is
    /// delivered as [`EditCommand::InsertString`](crate::EditCommand::InsertString),
    /// which never goes through auto-pairing. If bracketed paste is disabled, pasted
    /// characters arrive the same way as typed ones and are auto-paired like typing:
    /// pasting text that has an opener without its closer (e.g. `foo(bar`) inserts a
    /// closing character that was never in the clipboard content. For this reason it
    /// is recommended to combine `with_auto_pairs` with
    /// `use_bracketed_paste(cfg!(not(target_os = "windows")))`. On Windows,
    /// stock crossterm reads console input through
    /// the Win32 console API and has no ANSI input parser, so there is no
    /// `Event::Paste` path to enable there (see crossterm-rs/crossterm#737).
    ///
    /// Auto-pairing applies to edits in the regular line buffer. Reverse-history
    /// search edits a separate search query and are not passed through the
    /// auto-pairing machinery. Neither is typing while a menu in
    /// [`InputMode::Diff`](crate::InputMode::Diff) is open, such as a
    /// [`ListMenu`](crate::ListMenu) used for history, since that text is the
    /// menu's query.
    #[must_use]
    pub fn with_auto_pairs(mut self, auto_pairs: AutoPairs) -> Self {
        self.auto_pairs = Some(auto_pairs);
        self
    }

    /// Disable automatic pair insertion.
    #[must_use]
    pub fn disable_auto_pairs(mut self) -> Self {
        self.auto_pairs = None;
        self
    }

    /// Configure mouse click-to-cursor support.
    ///
    /// Use [`MouseClickMode::Enabled`] to handle click events when your host shell
    /// emits OSC 133 markers. Use [`MouseClickMode::EnabledWithOsc133`] to have
    /// Reedline emit OSC 133 markers with `click_events=1` so supporting terminals
    /// can send click events.
    /// See: <https://sw.kovidgoyal.net/kitty/shell-integration/#notes-for-shell-developers>
    #[must_use]
    pub fn with_mouse_click(mut self, mode: MouseClickMode) -> Self {
        self.mouse_click_mode = mode;
        if matches!(mode, MouseClickMode::EnabledWithOsc133) {
            self.painter
                .set_semantic_markers(Some(Osc133ClickEventsMarkers::boxed()));
        }
        self
    }

    /// Update current working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: Option<String>) -> Self {
        self.cwd = cwd;
        self
    }

    /// A builder that configures the highlighter for your instance of the Reedline engine
    /// # Example
    /// ```rust
    /// // Create a reedline object with highlighter support
    ///
    /// use reedline::{ExampleHighlighter, Reedline};
    ///
    /// let commands = vec![
    ///   "test".into(),
    ///   "hello world".into(),
    ///   "hello world reedline".into(),
    ///   "this is the reedline crate".into(),
    /// ];
    /// let mut line_editor =
    /// Reedline::create().with_highlighter(Box::new(ExampleHighlighter::new(commands)));
    /// ```
    #[must_use]
    pub fn with_highlighter(mut self, highlighter: Box<dyn Highlighter>) -> Self {
        self.highlighter = highlighter;
        self
    }

    /// A builder that configures the style used for visual selection
    #[must_use]
    pub fn with_visual_selection_style(mut self, style: Style) -> Self {
        self.visual_selection_style = style;
        self
    }

    /// A builder that gives the cell under the cursor its own style inside a
    /// visual selection, the way helix styles its primary cursor distinctly
    /// from the selection around it. Left unset, the cell keeps the plain
    /// selection style, which can paint a flat selection (e.g. reverse video)
    /// over the terminal cursor and hide it.
    #[must_use]
    pub fn with_visual_selection_cursor_style(mut self, style: Style) -> Self {
        self.visual_selection_cursor_style = Some(style);
        self
    }

    /// A builder which configures the history for your instance of the Reedline engine
    /// # Example
    /// ```rust,no_run
    /// // Create a reedline object with history support, including history size limits
    ///
    /// use reedline::{FileBackedHistory, Reedline};
    ///
    /// let history = Box::new(
    /// FileBackedHistory::with_file(5, "history.txt".into())
    ///     .expect("Error configuring history with file"),
    /// );
    /// let mut line_editor = Reedline::create()
    ///     .with_history(history);
    /// ```
    #[must_use]
    pub fn with_history(mut self, history: Box<dyn History>) -> Self {
        self.history = history;
        self
    }

    /// A builder which configures history exclusion for your instance of the Reedline engine
    /// # Example
    /// ```rust,no_run
    /// // Create a reedline instance with history that will *not* include commands starting with a space
    ///
    /// use reedline::{FileBackedHistory, Reedline};
    ///
    /// let history = Box::new(
    /// FileBackedHistory::with_file(5, "history.txt".into())
    ///     .expect("Error configuring history with file"),
    /// );
    /// let mut line_editor = Reedline::create()
    ///     .with_history(history)
    ///     .with_history_exclusion_prefix(Some(" ".into()));
    /// ```
    #[must_use]
    pub fn with_history_exclusion_prefix(mut self, ignore_prefix: Option<String>) -> Self {
        self.history_exclusion_prefix = ignore_prefix;
        self
    }

    /// A builder that configures the validator for your instance of the Reedline engine
    /// # Example
    /// ```rust
    /// // Create a reedline object with validator support
    ///
    /// use reedline::{DefaultValidator, Reedline};
    ///
    /// let mut line_editor =
    /// Reedline::create().with_validator(Box::new(DefaultValidator));
    /// ```
    #[must_use]
    pub fn with_validator(mut self, validator: Box<dyn Validator>) -> Self {
        self.validator = Some(validator);
        self
    }

    /// A builder that configures the alternate text editor used to edit the line buffer
    ///
    /// You are responsible for providing a file path that is unique to this reedline session
    ///
    /// # Example
    /// ```rust,no_run
    /// // Create a reedline object with vim as editor
    ///
    /// use reedline::Reedline;
    /// use std::env::temp_dir;
    /// use std::process::Command;
    ///
    /// let temp = std::env::temp_dir().join("my-random-unique.file");
    /// let mut command = Command::new("vim");
    /// // you can provide additional flags:
    /// command.arg("-p"); // open in a new vim tab
    /// // ...and the filename will be appended at the end of the command
    /// let mut line_editor = Reedline::create().with_buffer_editor(command, temp.clone());
    ///
    /// // optionally, {file}, {line}, and {col} placeholders can be used.
    /// // they will be replaced with the filename and the current cursor position,
    /// // both 1-based, with {col} counting graphemes from the line start
    /// let mut command = Command::new("hx");
    /// command.arg("{file}:{line}:{col}");
    /// let mut line_editor = Reedline::create().with_buffer_editor(command, temp.clone());
    ///
    /// // if {file} is omitted, the filename is still appended at the end,
    /// // as in the above example
    /// let mut command = Command::new("emacs");
    /// command.arg("+{line}:{col}");
    /// let mut line_editor = Reedline::create().with_buffer_editor(command, temp);
    /// ```
    ///
    /// A command with placeholders is rebuilt on every `OpenEditor` from its
    /// program, arguments, environment and working directory. Other settings on
    /// the `Command`, such as stdio redirection or `env_clear`, do not carry over.
    /// A command without placeholders is spawned as given.
    #[must_use]
    pub fn with_buffer_editor(mut self, editor: Command, temp_file: PathBuf) -> Self {
        self.buffer_editor = Some(BufferEditor::new(editor, temp_file));
        self
    }

    /// Remove the current [`Validator`]
    #[must_use]
    pub fn disable_validator(mut self) -> Self {
        self.validator = None;
        self
    }

    /// Set a different prompt to be used after submitting each line
    #[must_use]
    pub fn with_transient_prompt(mut self, transient_prompt: Box<dyn Prompt>) -> Self {
        self.transient_prompt = Some(transient_prompt);
        self
    }

    /// A builder that configures semantic prompt markers for terminal integration.
    ///
    /// This enables semantic prompt support for terminals that support it, such as Ghostty.
    /// Use `Osc133Markers::boxed()` for standard terminal support or `Osc633Markers::boxed()`
    /// for VS Code integrated terminal support.
    #[must_use]
    pub fn with_semantic_markers(
        mut self,
        markers: Option<Box<dyn SemanticPromptMarkers>>,
    ) -> Self {
        self.painter.set_semantic_markers(markers);
        self
    }

    /// A builder which configures the edit mode for your instance of the Reedline engine
    #[must_use]
    pub fn with_edit_mode(mut self, edit_mode: Box<dyn EditMode>) -> Self {
        self.edit_mode = edit_mode;
        self
    }

    /// A builder that registers another edit mode as a standby, which a
    /// [`ReedlineEvent::SwitchMode`] can activate. The mode given to
    /// [`with_edit_mode`](Self::with_edit_mode) stays active until then, and
    /// the mode a switch replaces becomes a standby in turn.
    ///
    /// Registering appends, so a host that rebuilds its modes on every prompt
    /// clears them with [`clear_edit_modes`](Self::clear_edit_modes) first.
    #[must_use]
    pub fn with_additional_edit_mode(mut self, edit_mode: Box<dyn EditMode>) -> Self {
        self.standby_edit_modes.push(edit_mode);
        self
    }

    /// A builder that clears the standby edit modes registered with
    /// [`with_additional_edit_mode`](Self::with_additional_edit_mode), leaving
    /// the active one alone.
    #[must_use]
    pub fn clear_edit_modes(mut self) -> Self {
        self.standby_edit_modes.clear();
        self
    }

    /// A builder that appends a menu to the engine
    #[must_use]
    pub fn with_menu(mut self, menu: ReedlineMenu) -> Self {
        self.menus.push(menu);
        self
    }

    /// A builder that clears the list of menus added to the engine
    #[must_use]
    pub fn clear_menus(mut self) -> Self {
        self.menus = Vec::new();
        self
    }

    /// A builder that adds abbreviations to the Reedline engine
    ///
    /// Overwrites any existing abbreviations with the same key.
    ///
    /// Note, by default abbreviations are expanded everywhere. To suppress expansion in certain
    /// syntactic positions (e.g. string literals), override [`Highlighter::should_expand_abbr`].
    pub fn with_abbreviations(mut self, abbreviations: HashMap<String, String>) -> Self {
        self.abbreviations.extend(abbreviations);
        self
    }

    /// A builder that adds the history item id
    #[must_use]
    pub fn with_history_session_id(mut self, session: Option<HistorySessionId>) -> Self {
        self.history_session_id = session;
        self
    }

    /// A builder that enables reedline changing the cursor shape based on the current edit mode.
    /// The current implementation sets the cursor shape when drawing the prompt.
    /// Do not use this if the cursor shape is set elsewhere, e.g. in the terminal settings or by ansi escape sequences.
    pub fn with_cursor_config(mut self, cursor_shapes: CursorConfig) -> Self {
        self.cursor_shapes = Some(cursor_shapes);
        self
    }

    /// A builder that configures whether reedline should immediately accept the input.
    pub fn with_immediately_accept(mut self, immediately_accept: bool) -> Self {
        self.immediately_accept = immediately_accept;
        self
    }

    /// A builder that configures an external break signal.
    ///
    /// When the [`AtomicBool`] is set to `true` by an external thread,
    /// [`Reedline::read_line()`] will return [`Signal::ExternalBreak`] with the
    /// current buffer contents. The flag is automatically reset to `false`
    /// after being consumed.
    pub fn with_break_signal(mut self, signal: Arc<AtomicBool>) -> Self {
        self.break_signal = Some(signal);
        self
    }

    /// Get a [`RepaintSignal`] handle that can trigger an in-place repaint of
    /// the prompt from another thread while [`Reedline::read_line()`] is
    /// running, avoiding interfering with current line edit.
    pub fn repaint_signal(&mut self) -> RepaintSignal {
        // The handle is created lazily on the first call; subsequent calls return clones
        self.repaint_signal
            .get_or_insert_with(RepaintSignal::default)
            .clone()
    }

    /// Returns the corresponding expected prompt style for the given edit mode
    pub fn prompt_edit_mode(&self) -> PromptEditMode {
        self.edit_mode.edit_mode()
    }

    /// Output the complete [`History`] chronologically with numbering to the terminal
    pub fn print_history(&mut self) -> Result<()> {
        let history: Vec<_> = self
            .history
            .search(SearchQuery::everything(SearchDirection::Forward, None))?;

        for (i, entry) in history.iter().enumerate() {
            self.print_line(&format!("{}\t{}", i, entry.command_line))?;
        }
        Ok(())
    }

    /// Output the complete [`History`] for this session, chronologically with numbering to the terminal
    pub fn print_history_session(&mut self) -> Result<()> {
        let history: Vec<_> = self.history.search(SearchQuery::everything(
            SearchDirection::Forward,
            self.get_history_session_id(),
        ))?;

        for (i, entry) in history.iter().enumerate() {
            self.print_line(&format!("{}\t{}", i, entry.command_line))?;
        }
        Ok(())
    }

    /// Print the history session id
    pub fn print_history_session_id(&mut self) -> Result<()> {
        println!("History Session Id: {:?}", self.get_history_session_id());
        Ok(())
    }

    /// Toggle between having a history that uses the history session id and one that does not
    pub fn toggle_history_session_matching(
        &mut self,
        session: Option<HistorySessionId>,
    ) -> Result<()> {
        self.history_session_id = match self.get_history_session_id() {
            Some(_) => None,
            None => session,
        };
        Ok(())
    }

    /// Read-only view of the history
    pub fn history(&self) -> &dyn History {
        &*self.history
    }

    /// Mutable view of the history
    pub fn history_mut(&mut self) -> &mut dyn History {
        &mut *self.history
    }

    /// Update the underlying [`History`] to/from disk
    pub fn sync_history(&mut self) -> std::io::Result<()> {
        // TODO: check for interactions in the non-submitting events
        self.history.sync()
    }

    /// Check if the last [`Reedline::read_line`] submitted a command.
    ///
    /// The context lasts until the next `read_line` starts. A call that returns
    /// anything but a non-empty [`Signal::Success`] leaves it unset, and calling
    /// [`Self::update_last_command_context`] then fails with a "No command run" error.
    pub fn has_last_command_context(&self) -> bool {
        matches!(
            self.history_last_run,
            HistoryLastRun::Stored(_) | HistoryLastRun::Excluded { current: true, .. }
        )
    }

    /// update the last history item with more information
    pub fn update_last_command_context(
        &mut self,
        f: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> crate::Result<()> {
        match mem::take(&mut self.history_last_run) {
            HistoryLastRun::Excluded {
                item,
                current: true,
                recalled,
            } => {
                self.history_last_run = HistoryLastRun::Excluded {
                    item: f(item),
                    current: true,
                    recalled,
                };
                Ok(())
            }
            HistoryLastRun::Stored(r) => {
                self.history_last_run = HistoryLastRun::Stored(r);
                self.history.update(r, f)
            }
            stale @ (HistoryLastRun::Excluded { current: false, .. } | HistoryLastRun::Empty) => {
                self.history_last_run = stale;
                Err(ReedlineError(ReedlineErrorVariants::OtherHistoryError(
                    "No command run",
                )))
            }
        }
    }

    /// Take the error of the last failed history save, if any.
    ///
    /// [`read_line`](Self::read_line) still returns the line when the [`History`]
    /// refuses to store it; the entry is then treated like an excluded one.
    /// Cleared on read, set at most once per `read_line`.
    pub fn take_history_save_error(&mut self) -> Option<ReedlineError> {
        self.history_save_error.take()
    }

    /// Wait for input and provide the user with a specified [`Prompt`].
    ///
    /// Returns a [`std::io::Result`] in which the `Err` type is [`std::io::Result`]
    /// and the `Ok` variant wraps a [`Signal`] which handles user inputs.
    pub fn read_line(&mut self, prompt: &dyn Prompt) -> Result<Signal> {
        terminal::enable_raw_mode()?;
        self.bracketed_paste.enter();
        self.kitty_protocol.enter();

        let result = self.read_line_helper(prompt);
        if result.is_err() {
            // The error is what to report, not a failure to show.
            let _ignore = self.painter.show_cursor();
        }

        self.bracketed_paste.exit();
        self.kitty_protocol.exit();
        terminal::disable_raw_mode()?;
        result
    }

    /// Returns the current insertion point of the input buffer.
    pub fn current_insertion_point(&self) -> usize {
        self.editor.insertion_point()
    }

    /// Returns the current contents of the input buffer.
    pub fn current_buffer_contents(&self) -> &str {
        self.editor.get_buffer()
    }

    /// Returns the current selection range of the input buffer, otherwise None.
    pub fn current_selection(&self) -> Option<(usize, usize)> {
        self.editor.get_selection()
    }

    /// Writes `msg` to the terminal with a following carriage return and newline,
    /// its control characters shown rather than sent
    fn print_line(&mut self, msg: &str) -> Result<()> {
        self.painter.paint_line(&escape_control(msg))
    }

    /// Clear the screen by printing enough whitespace to start the prompt or
    /// other output back at the first line of the terminal.
    pub fn clear_screen(&mut self) -> Result<()> {
        self.painter.clear_screen()?;
        self.painter.show_cursor()
    }

    /// Clear the screen and the scrollback buffer of the terminal
    pub fn clear_scrollback(&mut self) -> Result<()> {
        self.painter.clear_scrollback()?;
        self.painter.show_cursor()
    }

    /// Consume a pending external repaint request, returning whether one was
    /// pending. Any number of requests since the last check collapse into one.
    fn take_repaint_request(&self) -> bool {
        self.repaint_signal
            .as_ref()
            .is_some_and(RepaintSignal::take)
    }

    /// Whether the input loop must poll with a timeout instead of blocking
    /// indefinitely, so external triggers (break signal, repaint signal,
    /// external printer, idle callback) are noticed while waiting for input.
    fn input_needs_polling(&self) -> bool {
        #[allow(unused_mut)] // Dependent on feature flags
        let mut poll = self.break_signal.is_some()
            || self
                .repaint_signal
                .as_ref()
                .is_some_and(|sig| Arc::strong_count(&sig.flag) > 1);

        {
            poll |= self.external_printer.is_some();
        }

        poll |= self.idle_callback.is_some();

        poll
    }

    /// Reset the per-call state at the start of a `read_line`, before anything
    /// can fail.
    ///
    /// Returns the state of a suspended editor (ExecuteHostCommand or
    /// ExternalBreak) for the painter to resume from.
    fn begin_read(&mut self) -> Option<PainterSuspendedState> {
        self.hide_hints = false;
        self.keyseq_deadline = None;
        // A sequence abandoned by a break, Ctrl-C or submit belongs to the
        // previous call, not this prompt.
        self.edit_mode.reset_pending_sequence();

        // The last command context belongs to the previous call. Only a
        // submit in this call may set it again, so a host command, Ctrl-C
        // or an empty Enter can't reach back and update an older entry.
        // An excluded item stays, since Up still recalls it.
        if let HistoryLastRun::Excluded { current, .. } = &mut self.history_last_run {
            *current = false;
        } else {
            self.history_last_run = HistoryLastRun::Empty;
        }

        // Repaint requests raised while no read_line was active are stale:
        // the fresh prompt painted below already reflects the latest state.
        self.take_repaint_request();

        // Per-line GNU command tracking resets: a new line sits at the
        // live (unsaved) history position — `where_history()` is fresh —
        // and no previous command exists for `rl_last_func` chains.
        self.history_recall_pos = None;
        self.yank_last_arg = None;
        self.history_line_search = None;
        // A read that ended mid-collection (host command, suspend)
        // must not carry the string into the next line — GNU clears
        // `RL_STATE_NSEARCH` on teardown.
        self.noninc_search = None;
        if self.input_mode == InputMode::NonIncrementalSearch {
            self.input_mode = InputMode::Regular;
        }

        self.suspended_state.take()
    }

    /// Helper implementing the logic for [`Reedline::read_line()`] to be wrapped
    /// in a `raw_mode` context.
    fn read_line_helper(&mut self, prompt: &dyn Prompt) -> Result<Signal> {
        let suspended_state = self.begin_read();
        // A `C-o` from the previous line preloads the entry after its
        // history position before the first paint (GNU
        // `_rl_internal_startup_hook` → `set_saved_history`, misc.c).
        self.apply_operate_and_get_next();
        self.painter
            .initialize_prompt_position(suspended_state.as_ref())?;
        self.repaint(prompt)?;

        loop {
            // Call idle callback if set (for processing external events like GUI updates)
            if let Some(ref mut callback) = self.idle_callback {
                callback();
                // The callback owns stdout while it runs and may have
                // written or moved the cursor. Re-verify the anchor on
                // the next paint.
                self.painter.invalidate_prompt_start_row();
            }

            if let Some(ref signal) = self.break_signal {
                if signal.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    let buffer = self.editor.get_buffer().to_string();
                    self.input_mode = InputMode::Regular;
                    self.last_render_snapshot = None;
                    self.suspended_state = Some(self.painter.state_before_suspension());
                    self.editor.reset_undo_stack();
                    return Ok(Signal::ExternalBreak(buffer));
                }
            }

            if self.take_repaint_request() {
                self.repaint(prompt)?;
            }

            if let Some(printer) = self.external_printer.as_mut() {
                let messages = printer.drain()?;
                if !messages.is_empty() {
                    self.painter.print_external_message(messages)?;
                    self.repaint(prompt)?;
                }
            }

            // Determine if we need to poll (non-blocking) or can block on input.
            // We need polling if external_printer or idle_callback is configured,
            // using the shared poll_interval for the timeout.
            let status = self.completer.poll_completion();
            // Anything BUT idle means work is in flight. We need to keep polling.
            let completer_pending = status != CompletionStatus::Idle;

            if status == CompletionStatus::Ready {
                self.settle_completions(prompt)?;
            }

            let mut events: Vec<Event> = vec![];

            if !self.immediately_accept {
                if self.pending_input.is_empty() && self.pending_events.is_empty() {
                    let keyseq_wait = self.keyseq_remaining();
                    if self.input_needs_polling() || completer_pending || keyseq_wait.is_some() {
                        // With a sequence pending, wake at its deadline even when
                        // the ordinary poll interval is longer.
                        let timeout = keyseq_wait
                            .map_or(self.poll_interval, |wait| wait.min(self.poll_interval));
                        if event::poll(timeout)? {
                            events.push(crossterm::event::read()?);
                        } else if keyseq_wait.is_some()
                            && self.keyseq_remaining().is_some_and(|wait| wait.is_zero())
                        {
                            // No key before the sequence timeout: flush the prefix.
                            match self.flush_pending_keyseq(prompt)? {
                                ControlFlow::Break(signal) => return Ok(signal),
                                ControlFlow::Continue(()) => continue,
                            }
                        }
                    } else {
                        // Block until we receive an event
                        events.push(crossterm::event::read()?);
                    }
                }

                // Receive all events in the queue without blocking. Will stop when
                // a line of input is completed.
                while !Self::line_input_completed(&events) && event::poll(Duration::from_millis(0))?
                {
                    events.push(crossterm::event::read()?);
                }

                // If we believe there's text pasting or resizing going on, batch
                // more events at the cost of a slight delay.
                if events.len() > EVENTS_THRESHOLD
                    || events.iter().any(|e| matches!(e, Event::Resize(_, _)))
                {
                    while !Self::line_input_completed(&events) && event::poll(POLL_WAIT)? {
                        events.push(crossterm::event::read()?);
                    }
                }

                // Paste-burst oracle. Feed the just-collected plain chars to the
                // burst detector (a multi-char poll(0) batch is itself a paste
                // signal — normal typing yields ~1 char per read-loop iteration),
                // then, while a real burst is coalescing, keep draining the queue
                // past the `line_input_completed()`/`EVENTS_THRESHOLD` stops so a multi-line
                // paste lands in one batch. Fully gated on an installed hook, so
                // the no-hook path above is byte-for-byte unchanged.
                if let Some(hook) = self.paste_burst.clone() {
                    for c in events.iter().filter_map(burst_char) {
                        hook.on_char(c);
                    }
                    if hook.is_burst_active() {
                        loop {
                            if event::poll(hook.poll_timeout())? {
                                let event = crossterm::event::read()?;
                                if let Some(c) = burst_char(&event) {
                                    hook.on_char(c);
                                }
                                events.push(event);
                            } else {
                                // Idle: the burst has settled, but do NOT reset
                                // the detector here — `process_input_batch` below
                                // still needs the live burst flag to reclassify
                                // this batch's embedded Enters as newlines. The
                                // reset happens once after the batch is processed
                                // (see `settle_paste_burst`).
                                break;
                            }
                        }
                    }
                }
            }

            // Process the batch unconditionally: in `immediately_accept` mode
            // `events` stays empty, but `process_input_batch` still pushes the
            // synthetic `Submit` and returns the buffer. Gating this call behind
            // `!immediately_accept` would spin the loop forever.
            // A key that arrived starts the next per-key timeout window; an
            // empty iteration must keep the deadline it already has.
            let had_events = !events.is_empty();
            let batch_result = self.process_input_batch(prompt, events)?;
            if had_events {
                self.keyseq_deadline = None;
            }
            self.settle_paste_burst();
            if let ControlFlow::Break(signal) = batch_result {
                return Ok(signal);
            }
        }
    }

    /// Returns true if the last collected event ends a line of input and the
    /// batch can be handed to the hosting application.
    ///
    /// Besides a plain `Enter`, a `Ctrl+J` (what a piped LF parses to in raw
    /// mode) also ends a line: without this, several LF-terminated lines that
    /// arrive in one drain merge into a single batch, and every line after the
    /// first is silently dropped when the batch breaks on the first submit.
    ///
    /// `Ctrl+J` is `Enter` in every insert keymap (emacs, vi and helix
    /// insert). In the other vi and helix modes it is unbound, so ending the
    /// drain there only adds a loop iteration and the line is unaffected.
    fn line_input_completed(events: &[Event]) -> bool {
        if let Some(event) = events.last() {
            matches!(
                event,
                Event::Key(KeyEvent {
                    code: KeyCode::Enter,
                    modifiers: KeyModifiers::NONE,
                    ..
                }) | Event::Key(KeyEvent {
                    code: KeyCode::Char('j'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                })
            )
        } else {
            false
        }
    }

    /// Reset the paste-burst detector once the batch that held a burst has been
    /// processed, which releases the burst flag before the next independent line
    /// (so a later human Enter submits instead of being absorbed).
    ///
    /// A batch without a burst does not reset it: the detector tells typing from
    /// a paste by its own clock. Some event sources hand over a single event per
    /// loop iteration, and resetting after each of those batches would keep the
    /// detector from ever counting up to its threshold.
    ///
    /// Kept out of the input loop so it is reachable from tests, which cannot
    /// drive the loop itself.
    fn settle_paste_burst(&self) {
        if let Some(hook) = &self.paste_burst {
            if hook.is_burst_active() {
                hook.settle();
            }
        }
    }

    /// Fold a finished completion request into the active menu, and honor the completions
    /// owed from when the request was made.
    ///
    /// Called when the completer reports [`CompletionStatus::Ready`]. Kept out of the
    /// input loop so it is reachable from tests, which cannot drive the loop itself.
    fn settle_completions(&mut self, prompt: &dyn Prompt) -> Result<()> {
        let Some(menu_index) = self.menus.iter().position(|menu| menu.is_active()) else {
            // No menu to answer to, so nothing can still be owed.
            self.deferred_menu_completion = None;
            return Ok(());
        };

        // The request that just finished was host code, which may have scrolled the
        // terminal while it ran in the background — after the keystroke that
        // dispatched it had already re-verified the anchor. `update_values` below
        // can also dispatch another request for a line that moved on. Either way
        // the repaint at the end of this settle must not trust the cached row.
        invalidate_anchor_if_host_completer_runs(&self.menus[menu_index], &mut self.painter);

        let menu = &mut self.menus[menu_index];
        // The request this menu was waiting on finished, so repopulate it.
        menu.update_values(
            &mut self.editor,
            self.completer.as_mut(),
            self.history.as_ref(),
        );
        let still_provisional = menu.results_are_provisional();

        let owed = self
            .deferred_menu_completion
            .as_ref()
            .is_some_and(|deferred| deferred.still_applies(menu, &self.editor));

        // Values were just refreshed above, so the menu must not re-fetch them.
        let accept_lone_value =
            owed && !still_provisional && self.decide_menu_completion(menu_index, true);

        // Spent once a final answer had its say, so it cannot act on a later one.
        // Provisional results decided nothing, so the arm outlives them.
        if !still_provisional {
            self.deferred_menu_completion = None;
        }

        if accept_lone_value {
            // With a menu active this replaces in the buffer and deactivates, rather than
            // submitting the line.
            self.handle_editor_event(prompt, ReedlineEvent::Enter)?;
        }

        // One paint for every outcome, since painting the menu and then the accepted or
        // extended line would flicker on each completion.
        self.repaint(prompt)
    }

    /// The completion an opening menu applies to the line: a lone suggestion is accepted
    /// outright, otherwise the prefix the suggestions share is spliced in. Returns whether
    /// the caller should accept that lone suggestion.
    ///
    /// Both the [`Menu`](ReedlineEvent::Menu) event and the deferred replay in
    /// [`settle_completions`](Self::settle_completions) decide this, and they have to
    /// decide it identically. `values_updated` says whether the caller already refreshed
    /// the menu's values, so they are not fetched twice.
    fn decide_menu_completion(&mut self, menu_index: usize, values_updated: bool) -> bool {
        let menu = &mut self.menus[menu_index];

        let accept_lone_value = if self.quick_completions && menu.can_quick_complete() {
            if !values_updated {
                menu.update_values(
                    &mut self.editor,
                    self.completer.as_mut(),
                    self.history.as_ref(),
                );
            }
            // Accepting a lone *stale* value is refused downstream, since its span
            // belongs to another line, so the menu would close over a completion that
            // never happened.
            menu.get_values().len() == 1 && !menu.results_are_provisional()
        } else {
            false
        };

        if !accept_lone_value && self.partial_completions {
            menu.can_partially_complete(
                values_updated || self.quick_completions,
                &mut self.editor,
                self.completer.as_mut(),
                self.history.as_ref(),
            );
        }

        accept_lone_value
    }

    /// Upper bound on the replayed-key queue
    /// ([`ReedlineEvent::KeySequence`]): a self-retriggering sequence keeps
    /// enqueuing until the cap drops the overflow, so a runaway macro
    /// terminates instead of spinning the input loop forever.
    pub const MAX_REPLAYED_KEYS: usize = 4096;

    /// Queue a replayed key sequence (the [`ReedlineEvent::KeySequence`]
    /// payload) as synthetic `Press` input ahead of live terminal input.
    /// [`Self::MAX_MACRO_NESTING`]: GNU `macro.c MAX_MACRO_LEVEL` —
    /// a sequence nesting deeper than this is dropped rather than run.
    pub const MAX_MACRO_NESTING: u16 = 16;

    /// Keys beyond [`Self::MAX_REPLAYED_KEYS`] pending are dropped; a
    /// sequence emitted deeper than [`Self::MAX_MACRO_NESTING`] aborts
    /// whole, like GNU's nesting guard.
    fn enqueue_replayed_keys(&mut self, keys: Vec<crate::KeyCombination>, depth: u16) {
        if depth > Self::MAX_MACRO_NESTING {
            return;
        }
        let room = Self::MAX_REPLAYED_KEYS.saturating_sub(self.pending_input.len());
        self.pending_input.extend(
            keys.into_iter()
                .take(room)
                .map(|key| (Event::Key(KeyEvent::new(key.key_code, key.modifier)), depth)),
        );
    }

    /// GNU `misc.c` `_rl_arg_overflow` (misc.c:80): an argument that
    /// grows past a million dings and discards the accumulation.
    const MAX_NUMERIC_ARG: i64 = 1_000_000;

    /// Feed a `DigitArgument` key into the accumulation — `Some(d)` an
    /// `M-0`..`M-9` digit, `None` the `M--` sign. Mirrors
    /// `_rl_arg_dispatch` (misc.c:116-183); a fresh argument when none
    /// was pending is `rl_digit_argument` seeding the loop with its
    /// own key (misc.c:230).
    fn arg_digit(&mut self, digit: Option<u8>) {
        let mut arg = self.pending_arg.unwrap_or_else(PendingArg::fresh);
        match digit {
            Some(d) => {
                // misc.c:152 — a digit replaces the pending value
                // unless earlier digits already made it explicit
                // (`rl_explicit_arg` stays unset through `C-u`s, so
                // `C-u 5` is 5, not 20).
                arg.value = if arg.explicit {
                    arg.value.saturating_mul(10).saturating_add(i64::from(d))
                } else {
                    i64::from(d)
                };
                arg.explicit = true;
                arg.saw_digits = true;
            }
            None => {
                // misc.c:159 — `M--` negates: GNU sets `rl_arg_sign`
                // while `rl_explicit_arg` is unset, and after digits
                // re-dispatches the `M--` binding itself (misc.c:174).
                // Toggling the sign of the accumulation is the closest
                // read of that re-dispatch that stays finite.
                arg.sign = -arg.sign;
            }
        }
        self.pending_arg = (arg.value <= Self::MAX_NUMERIC_ARG).then_some(arg);
    }

    /// `rl_universal_argument` (misc.c:262): multiply by four while no
    /// digits are down; after digits GNU ignores the key entirely and
    /// dispatches the NEXT one under the count (misc.c:121-133), which
    /// `dispatch_next` arms.
    fn arg_universal(&mut self) {
        let mut arg = self.pending_arg.unwrap_or_else(PendingArg::fresh);
        if arg.saw_digits {
            arg.dispatch_next = true;
        } else {
            arg.value = arg.value.saturating_mul(4);
        }
        self.pending_arg = (arg.value <= Self::MAX_NUMERIC_ARG).then_some(arg);
    }

    /// Wrap the terminating key's event with the spent argument — the
    /// `_rl_arg_dispatch` else-branch (misc.c:174-182).
    fn with_arg(arg: PendingArg, event: ReedlineEvent) -> ReedlineEvent {
        ReedlineEvent::WithArg {
            count: i32::try_from(arg.count()).unwrap_or(i32::MAX),
            explicit: arg.explicit(),
            event: Box::new(event),
        }
    }

    /// `rl_start_kbd_macro` (macro.c:246): arm the recorder. An
    /// explicit argument is GNU's append mode (macro.c:253-258): the
    /// previous macro is replayed first and keeps recording, so the
    /// new definition heads with it — here the replayed keys append to
    /// the seed as they dispatch, exactly like GNU's resumed
    /// `current_macro`. Aborts — GNU dings — when already recording,
    /// leaving the recorder armed.
    fn start_kbd_macro(&mut self, explicit: bool) {
        if self.recording.is_some() {
            return;
        }
        let seed = if explicit && !self.last_macro.is_empty() {
            self.enqueue_replayed_keys(self.last_macro.clone(), self.replay_depth + 1);
            self.last_macro.clone()
        } else {
            Vec::new()
        };
        self.recording = Some(seed);
    }

    /// `rl_end_kbd_macro` (macro.c:270): stash the recording as
    /// `last_macro`. Returns false — GNU aborts — when not recording.
    fn end_kbd_macro(&mut self) -> bool {
        match self.recording.take() {
            Some(keys) => {
                self.last_macro = keys;
                true
            }
            None => false,
        }
    }

    /// `rl_call_last_kbd_macro` (macro.c:291): push the recording into
    /// input `count` times. Refused — GNU dings "no recursive macros"
    /// — while recording, or with nothing recorded yet.
    fn call_last_kbd_macro(&mut self, count: i64) {
        if self.recording.is_some() || self.last_macro.is_empty() {
            return;
        }
        for _ in 0..count {
            self.enqueue_replayed_keys(self.last_macro.clone(), self.replay_depth + 1);
        }
    }

    /// Repeat the parse loop's macro-recording corrections for an event
    /// nested inside a [`ReedlineEvent::Multiple`].
    ///
    /// The parse loop records every raw key before its mode resolves it and
    /// then erases the keys that invoke a control: a chord's `Pending` half
    /// and the `EndKbdMacro`/refused `CallLastKbdMacro` key are not part of
    /// the recording (GNU subtracts `rl_key_sequence_length` at
    /// macro.c:278 and erases the refused key at macro.c:298). A mode that
    /// flushed a parked chord wraps those events in `Multiple`, where the
    /// top-level corrections never run, so apply them here before the
    /// engine dispatches the nested event.
    fn note_nested_chord_key(&mut self, event: &ReedlineEvent) {
        let chord_key = matches!(
            event,
            ReedlineEvent::Pending | ReedlineEvent::EndKbdMacro | ReedlineEvent::CallLastKbdMacro
        );
        if !chord_key || self.recording.is_none() {
            return;
        }
        if let Some(recording) = self.recording.as_mut() {
            recording.pop();
        }
        if matches!(event, ReedlineEvent::CallLastKbdMacro) {
            // "no recursive macros" spends the refused key's argument
            // too (macro.c:298).
            self.pending_arg = None;
        }
    }

    /// Show the current completions in a menu without touching the line.
    ///
    /// The readline `possible-completions` counterpart: candidates are displayed
    /// only, never spliced in. Neither a lone suggestion nor the shared prefix
    /// reaches the buffer, whatever `quick_completions` and `partial_completions`
    /// say, because this event deliberately bypasses
    /// [`decide_menu_completion`](Self::decide_menu_completion).
    ///
    /// An open menu is refreshed in place; otherwise the first registered menu is
    /// activated to draw the list. With no menu there is nowhere to list them, and
    /// a final empty result leaves nothing to show, so both report inapplicable.
    fn list_possible_completions(&mut self) -> EventStatus {
        let menu_index = self
            .menus
            .iter()
            .position(|menu| menu.is_active())
            .or_else(|| (!self.menus.is_empty()).then_some(0));

        let Some(menu_index) = menu_index else {
            return EventStatus::Inapplicable;
        };

        let was_active = self.menus[menu_index].is_active();
        if !was_active {
            // `true` because the values are refreshed right below; this keeps
            // the paint cycle from asking the completer a second time.
            self.menus[menu_index].menu_event(MenuEvent::Activate(true));
            invalidate_anchor_if_host_completer_runs(&self.menus[menu_index], &mut self.painter);
        }

        self.menus[menu_index].update_values(
            &mut self.editor,
            self.completer.as_mut(),
            self.history.as_ref(),
        );

        // A pending answer has no values yet but does not mean "no candidates":
        // keep the menu open so settling can fill it. A final empty answer has
        // nothing to show, so undo an activation made just for it.
        let empty = self.menus[menu_index].get_values().is_empty();
        if empty && !self.menus[menu_index].results_are_provisional() {
            if !was_active {
                self.menus[menu_index].menu_event(MenuEvent::Deactivate);
            }
            return EventStatus::Inapplicable;
        }

        EventStatus::Handled
    }

    /// Insert every completion at the cursor, separated by spaces.
    ///
    /// The readline `insert-completions` counterpart: the word the completer
    /// reports is replaced by the whole list, each entry followed by a space, as
    /// one undoable edit. An empty result changes nothing.
    fn insert_all_completions(&mut self) -> EventStatus {
        let buffer = self.editor.get_buffer().to_string();
        let position = self.editor.completion_point();
        let Some(suggestions) = self
            .completer
            .complete(&buffer, position)
            .into_shared()
            .filter(|suggestions| !suggestions.is_empty())
        else {
            return EventStatus::Inapplicable;
        };

        // Every suggestion describes the same word; readline deletes that word
        // and writes the list in its place. Clamp in case a completer reported
        // a span for a longer line.
        let span = suggestions[0].span;
        let start = span.start.min(buffer.len());
        let end = span.end.min(buffer.len()).max(start);

        let mut replacement = String::new();
        for suggestion in suggestions.iter() {
            replacement.push_str(&suggestion.value);
            replacement.push(' ');
        }

        self.editor.clear_selection();
        self.editor.edit_buffer(
            |line_buffer| {
                line_buffer.replace_range(start..end, &replacement);
                line_buffer.set_insertion_point(start + replacement.len());
            },
            UndoBehavior::CreateUndoPoint,
        );
        if self.input_mode == InputMode::HistoryTraversal {
            self.input_mode = InputMode::Regular;
        }

        // The menu was filtering a word that is no longer on the line, and any
        // completion owed to an earlier keystroke no longer describes it either.
        self.deactivate_menus();

        EventStatus::Handled
    }

    fn process_input_batch(
        &mut self,
        prompt: &dyn Prompt,
        mut events: Vec<Event>,
    ) -> Result<ControlFlow<Signal>> {
        // Replayed keys dispatch before live input, like readline's macro
        // buffer sitting at the head of the input stream.
        if !self.pending_input.is_empty() {
            let mut replayed = std::mem::take(&mut self.pending_input);
            replayed.extend(events.into_iter().map(|event| (event, 0u16)));
            let (merged, depths): (Vec<Event>, VecDeque<u16>) = replayed.into_iter().unzip();
            events = merged;
            self.event_replay_depths = depths;
        }
        // Convert `Event` into `ReedlineEvent`. Also, fuse consecutive
        // `ReedlineEvent::EditCommand` into one. Also, if there're multiple
        // `ReedlineEvent::Resize`, only keep the last one.
        let mut reedline_events: Vec<(ReedlineEvent, u16)> = vec![];
        // A detected paste burst is handled as ONE unit, bypassing the per-event
        // parse below. Routing each pasted char through `parse_event` is fragile:
        // any event that maps to a non-`Edit` reedline event (a `/` slash-menu
        // trigger, a menu/history action, an un-reclassified Enter) hits the
        // fusing branch that FLUSHES the accumulated raw edits mid-loop
        // (`std::mem::take`), committing raw pasted text to the buffer before a
        // placeholder can replace it — the raw-text-leak + stray-newline bug.
        // Instead, collect the whole burst's content directly (chars + embedded
        // newlines, in order) and emit exactly one `InsertString`: a placeholder
        // from the host, or the raw text if the host declines to reference-ify
        // it. Fully gated on `burst_batch` (only true when a hook is installed
        // AND a real burst was detected), so normal typing and the no-hook path
        // are byte-for-byte unchanged.
        let burst_batch = self
            .paste_burst
            .as_ref()
            .is_some_and(|h| h.is_burst_active());
        // A resize is kept whichever arm parses the batch: the burst drain
        // pushes every event it reads, so a resize arriving mid-paste lands in
        // the burst batch too and must not be swallowed there.
        let mut resize = None;
        if burst_batch {
            // Macro recording sees the paste's keystrokes before the
            // burst coalesces them away — GNU records pasted bytes into
            // the macro the same way (kill.c:721).
            if let Some(recording) = self.recording.as_mut() {
                for event in &events {
                    if let Event::Key(KeyEvent {
                        code,
                        modifiers,
                        kind,
                        ..
                    }) = event
                    {
                        if *kind != KeyEventKind::Release {
                            recording.push(crate::KeyCombination {
                                modifier: *modifiers,
                                key_code: *code,
                            });
                        }
                    }
                }
            }
            let mut coalesced = String::new();
            // Every Enter drained into this batch is coalesced as an embedded
            // newline, never a submit. An Enter only reaches this loop by
            // being drained while `is_burst_active` stayed latched true, which
            // only happens inside the poll-timeout idle window that keeps the
            // burst coalescing (see the drain loop above) — i.e. it arrived at
            // machine paste speed, not from a human keypress. A real human
            // submit Enter, typed after the paste, lands past that idle
            // window: the drain loop has already stopped and `settle` has
            // already run by the time it is read, so it starts the *next*
            // batch instead, where it is handled as an ordinary submit.
            self.event_replay_depths.clear();
            for event in &events {
                if let Some(c) = burst_char(event) {
                    coalesced.push(c);
                    continue;
                }
                if is_pasted_newline(event) {
                    coalesced.push('\n');
                    continue;
                }
                match event {
                    // A pasted tab is part of the text. Left to the catch-all
                    // below it would be dropped from the insert.
                    Event::Key(KeyEvent {
                        code: KeyCode::Tab,
                        modifiers: KeyModifiers::NONE,
                        kind: KeyEventKind::Press,
                        ..
                    }) => {
                        coalesced.push('\t');
                    }
                    Event::Resize(x, y) => resize = Some((*x, *y)),
                    // Release events and any other keys are paste artifacts here.
                    _ => {}
                }
            }
            if !coalesced.is_empty() {
                let insert = self
                    .paste_burst
                    .clone()
                    .and_then(|h| h.resolve_burst(&coalesced))
                    .unwrap_or(coalesced);
                reedline_events.push((
                    ReedlineEvent::Edit(vec![EditCommand::InsertString(insert)]),
                    0,
                ));
            }
        } else {
            let mut edits = vec![];
            // Whether the previous live key was a `self-insert` character
            // in the same typeahead run. GNU `_rl_insert_typein`
            // (input.c:464) consumes a contiguous run of them as one
            // command, so only the run's first key is dispatched — and
            // therefore recorded (`_rl_add_macro_char`, readline.c:907).
            // Replayed macro keys are read one at a time from
            // `rl_executing_macro`, never through the ibuffer, so the run
            // never spans them. The run is approximated by the key's
            // shape (GNU tests `map[key] == rl_insert`); a plain
            // character rebound to another command is the one case where
            // the two disagree.
            let mut typein_run = false;
            for event in events {
                self.replay_depth = self.event_replay_depths.pop_front().unwrap_or(0);
                let typein_key = self.replay_depth == 0 && is_typein_key(&event);
                // Macro recording runs ahead of everything, the way
                // GNU's `_rl_add_macro_char` appends every dispatched
                // key before dispatching it (readline.c:907): the
                // recording holds keystrokes — including replayed ones,
                // which dispatch again — so it replays through whatever
                // keymap is live then.
                if let Some(recording) = self.recording.as_mut() {
                    if let Event::Key(KeyEvent {
                        code,
                        modifiers,
                        kind,
                        ..
                    }) = event
                    {
                        if kind != KeyEventKind::Release && !(typein_run && typein_key) {
                            recording.push(crate::KeyCombination {
                                modifier: modifiers,
                                key_code: code,
                            });
                        }
                    }
                }
                typein_run = typein_key;
                // Reclassify a newline key (bare `Enter`, or `Ctrl-J` for a raw
                // LF) that the oracle judges paste-embedded into a newline even
                // when a full burst was NOT detected (a short fast paste, e.g.
                // `aa\nbb`, whose lines never reach the burst char threshold),
                // so it does not submit mid-paste. Only a `Press` is
                // reclassified (the kitty enhancement also emits a Release); a
                // Release is rejected downstream by `ReedlineRawEvent::try_from`
                // and inserts nothing.
                if let Some(hook) = &self.paste_burst {
                    if is_pasted_newline(&event) && hook.enter_is_newline() {
                        edits.push(EditCommand::InsertNewline);
                        continue;
                    }
                }
                // Quoted-insert bypasses the keymap: the armed flag turns
                // the next key press into its literal character (GNU
                // rl_quoted_insert). Anything else is consumed and gone.
                // A numeric argument repeats the insert — GNU passes the
                // count to `_rl_insert_char` (text.c:1056).
                if self.quoted_insert {
                    self.quoted_insert = false;
                    let count = std::mem::replace(&mut self.quoted_insert_count, 1);
                    if let Some(c) = quoted_insert_char(&event) {
                        for _ in 0..count {
                            edits.push(EditCommand::InsertChar(c));
                        }
                    }
                    // The quoted key was read by `rl_quoted_insert`, not
                    // by a typeahead run: the next key starts a new one.
                    typein_run = false;
                    continue;
                }
                // Char-search consumes one key the same way
                // quoted-insert does (GNU text.c _rl_char_search —
                // `_rl_bracketed_read_key` reads exactly one input
                // character): a `Char` press becomes a `Find` move that
                // lands ON the match (FFIND/BFIND); any other key
                // cancels the search.
                if let Some(direction) = self.pending_char_search.take() {
                    if let Event::Key(KeyEvent {
                        code: KeyCode::Char(c),
                        kind,
                        ..
                    }) = event
                    {
                        if kind != KeyEventKind::Release {
                            edits.push(EditCommand::Move(MotionTarget::Find {
                                ch: c,
                                direction,
                                stop: FindStop::On,
                            }));
                        }
                    }
                    // Char-search read this key itself, so the next one
                    // is not part of the same `rl_insert` run.
                    typein_run = false;
                    continue;
                }
                if let Ok(raw) = ReedlineRawEvent::try_from(event) {
                    match self.edit_mode.parse_event(raw) {
                        // A chord's prefix half produced no event — the
                        // `C-x` of `C-x )`. It is not part of a macro
                        // recording either: GNU strips the resolved
                        // sequence's keys at `rl_end_kbd_macro`
                        // (macro.c:276); erasing the prefix as it arrives
                        // nets out the same and keeps aborted chords out.
                        ReedlineEvent::Pending => {
                            if let Some(recording) = self.recording.as_mut() {
                                recording.pop();
                            }
                            // The popped key was not a recorded
                            // self-insert: the next live character starts
                            // a fresh typeahead run.
                            typein_run = false;
                        }
                        // Numeric-argument keys accumulate in place and
                        // never reach the queue — GNU reads them inside
                        // `rl_digit_loop` (misc.c:192). `dispatch_next`
                        // is GNU's `C-u`-after-digits branch (misc.c:121):
                        // it makes even a digit terminate.
                        ReedlineEvent::DigitArgument(digit)
                            if !self.pending_arg.is_some_and(|arg| arg.dispatch_next) =>
                        {
                            self.arg_digit(digit);
                            // GNU's `(arg: N) ` shows while the
                            // argument accumulates (misc.c
                            // `_rl_arg_getchar`): a Repaint makes the
                            // accumulation key itself paint it.
                            reedline_events.push((ReedlineEvent::Repaint, self.replay_depth));
                        }
                        ReedlineEvent::UniversalArgument
                            if !self.pending_arg.is_some_and(|arg| arg.dispatch_next) =>
                        {
                            self.arg_universal();
                            reedline_events.push((ReedlineEvent::Repaint, self.replay_depth));
                        }
                        // Macro controls are engine state, applied at
                        // parse time like QuotedInsert below — and ahead
                        // of the argument wrap, since GNU dispatches
                        // them WITH the count: `rl_start_kbd_macro`
                        // reads `rl_explicit_arg` (macro.c:253) and
                        // `rl_end_kbd_macro` runs the new macro
                        // `count - 1` times (macro.c:283).
                        ReedlineEvent::StartKbdMacro => {
                            let explicit =
                                self.pending_arg.take().is_some_and(|arg| arg.explicit());
                            self.start_kbd_macro(explicit);
                            // The invoking `(` is a command, not a
                            // self-insert; the next character starts a
                            // fresh typeahead run.
                            typein_run = false;
                        }
                        ReedlineEvent::EndKbdMacro => {
                            // The keys that invoked end are not part of
                            // the recording — GNU subtracts
                            // `rl_key_sequence_length` (macro.c:276);
                            // the `C-x` half was already erased by
                            // `Pending` above.
                            if let Some(recording) = self.recording.as_mut() {
                                recording.pop();
                            }
                            typein_run = false;
                            let count = self.pending_arg.take().map_or(0, |arg| arg.count());
                            if self.end_kbd_macro() && count > 0 {
                                self.call_last_kbd_macro(count - 1);
                            }
                        }
                        ReedlineEvent::CallLastKbdMacro => {
                            if self.recording.is_some() {
                                // "no recursive macros" (macro.c:296):
                                // refused, and the invoking key is
                                // erased from the recording.
                                if let Some(recording) = self.recording.as_mut() {
                                    recording.pop();
                                }
                                typein_run = false;
                                self.pending_arg = None;
                            } else {
                                let count = self.pending_arg.take().map_or(1, |arg| arg.count());
                                self.call_last_kbd_macro(count);
                            }
                        }
                        ReedlineEvent::Edit(edit) => {
                            match self.pending_arg.take() {
                                // The terminating key dispatches under
                                // the count (misc.c:174): flush what
                                // accumulated before it, then queue the
                                // wrapped edit.
                                Some(arg) => {
                                    if !edits.is_empty() {
                                        reedline_events.push((
                                            ReedlineEvent::Edit(std::mem::take(&mut edits)),
                                            self.replay_depth,
                                        ));
                                    }
                                    reedline_events.push((
                                        Self::with_arg(arg, ReedlineEvent::Edit(edit)),
                                        self.replay_depth,
                                    ));
                                }
                                None => edits.extend(edit),
                            }
                        }
                        ReedlineEvent::Resize(x, y) => resize = Some((x, y)),
                        // Quoted-insert is fully applied at parse
                        // time: fast typists deliver the trigger and
                        // the next key in one batch, and the next
                        // event is consumed right below. Pushing the
                        // event would let its handler re-arm a flag
                        // the batch already spent. GNU passes its
                        // count to `_rl_insert_char` (text.c:1056).
                        ReedlineEvent::QuotedInsert => {
                            if !edits.is_empty() {
                                reedline_events.push((
                                    ReedlineEvent::Edit(std::mem::take(&mut edits)),
                                    self.replay_depth,
                                ));
                            }
                            let count = self.pending_arg.take().map_or(1, |arg| arg.count());
                            self.quoted_insert = true;
                            self.quoted_insert_count = usize::try_from(count).unwrap_or(0);
                        }
                        event => {
                            if !edits.is_empty() {
                                reedline_events.push((
                                    ReedlineEvent::Edit(std::mem::take(&mut edits)),
                                    self.replay_depth,
                                ));
                            }
                            // A pending argument spends itself on any
                            // other key (misc.c:174).
                            let event = match self.pending_arg.take() {
                                Some(arg) => Self::with_arg(arg, event),
                                None => event,
                            };
                            if let ReedlineEvent::CharSearch { direction } = event {
                                self.pending_char_search = Some(direction);
                            } else if let ReedlineEvent::KeySequence(keys) = event {
                                // Applied at parse time where the source's
                                // nesting depth is known; a replayed macro
                                // nests one level deeper (GNU macro.c
                                // MAX_MACRO_LEVEL).
                                self.enqueue_replayed_keys(keys, self.replay_depth + 1);
                            } else {
                                reedline_events.push((event, self.replay_depth));
                            }
                        }
                    }
                }
            }
            if !edits.is_empty() {
                reedline_events.push((ReedlineEvent::Edit(edits), self.replay_depth));
            }
        }
        if let Some((x, y)) = resize {
            reedline_events.push((ReedlineEvent::Resize(x, y), 0));
        }
        // The synthetic `Submit` of `immediately_accept` mode is pushed after
        // both arms: nothing is read in that mode, so `events` is empty and the
        // burst arm would insert nothing, but a hook that reports
        // `is_burst_active()` before anything was fed to it must still not
        // keep `read_line` spinning without a submit.
        if self.immediately_accept {
            reedline_events.push((ReedlineEvent::Submit, 0));
        }

        self.process_reedline_events(prompt, reedline_events)
    }

    /// Run a batch of already-parsed events, repaint if anything changed, and
    /// report an exit. Shared by the input loop and the key-sequence timeout.
    fn process_reedline_events(
        &mut self,
        prompt: &dyn Prompt,
        reedline_events: Vec<(ReedlineEvent, u16)>,
    ) -> Result<ControlFlow<Signal>> {
        // The mode machine has parsed this batch, so the rest policy it
        // declares is now final. Relay it to the editor before running the
        // emitted commands so a command a mode transition issued (e.g. the
        // Esc→normal grapheme step-back) resolves under the new policy. This
        // does not commit the cursor — the commands settle it, and the
        // pre-paint `set_edit_mode` below still clamps no-command switches.
        self.editor.sync_edit_mode(self.edit_mode.edit_mode());

        // Handle reedline events. Events parked by an earlier host-command
        // suspension run first, in their original order, ahead of this
        // batch's fresh input (GNU's input buffer keeps bytes queued behind
        // `rl_abort`).
        let mut queue: VecDeque<(ReedlineEvent, u16)> = std::mem::take(&mut self.pending_events);
        queue.extend(reedline_events);
        let mut need_repaint = false;
        while let Some((event, depth)) = queue.pop_front() {
            self.replay_depth = depth;
            match self.handle_event(prompt, event)? {
                EventStatus::Exits(signal) => {
                    // Check if we are merely suspended (to process an ExecuteHostCommand event)
                    // or if we're about to quit the editor.
                    if self.suspended_state.is_none() {
                        // We are about to quit the editor, move the cursor below the input
                        // area, for external commands or new read_line call
                        self.painter.move_cursor_to_end()?;
                    }
                    if matches!(&signal, Signal::HostCommand(_)) {
                        // A host command suspends the editor and the host
                        // resumes it with another `read_line`. GNU's abort
                        // longjmps out of the command but leaves the rest
                        // of the input buffer intact, so the keys that
                        // arrived in the same batch must still run.
                        self.pending_events.extend(queue.drain(..));
                    }
                    // A clear earlier in this batch left it hidden and no
                    // paint follows. The exit is what to report, so a failed
                    // show must not turn it into an error.
                    let _ignore = self.painter.show_cursor();
                    return Ok(ControlFlow::Break(signal));
                }
                EventStatus::Handled => {
                    need_repaint = true;
                }
                EventStatus::Inapplicable => {
                    // Nothing changed, no need to repaint
                }
            }
        }
        // A command-less mode transition adopts a new rest policy via
        // `sync_edit_mode` but emits nothing to commit the cursor. Force the
        // settle (and a repaint) so it doesn't stay unsettled until the next
        // command.
        if self.editor.policy_unsettled() {
            need_repaint = true;
        }
        if need_repaint {
            // Sync the editor's edit mode before painting so the cursor is
            // normalized under the current rest policy. A mode change that
            // bypasses the command path (e.g. Esc → Vi normal) otherwise
            // wouldn't clamp until the next command, painting the cursor past
            // the last grapheme for a frame.
            let mode = self.edit_mode.edit_mode();
            self.editor.set_edit_mode(mode);
            self.repaint(prompt)?;
        }
        Ok(ControlFlow::Continue(()))
    }

    /// GNU `rl_stop` (signals.c) — suspend inside `read_line`: deprep
    /// the terminal, raise `SIGTSTP`, and on `SIGCONT` re-prep and
    /// repaint so the edit continues exactly where it stopped. On
    /// platforms without job-control signals this is an inapplicable
    /// no-op.
    fn suspend(&mut self, prompt: &dyn Prompt) -> Result<EventStatus> {
        #[cfg(unix)]
        {
            // Same suspend/resume shape `ExecuteHostCommand` uses:
            // remember the painter state, leave raw mode, stop, and on
            // resume re-prep + full repaint.
            self.suspended_state = Some(self.painter.state_before_suspension());
            self.painter.move_cursor_to_end()?;
            let _ignore = terminal::disable_raw_mode();
            let _ignore = rustix::process::kill_process(
                rustix::process::getpid(),
                rustix::process::Signal::TSTP,
            );
            // Resumed (SIGCONT): restore the editor's terminal state.
            let _ignore = terminal::enable_raw_mode();
            self.repaint(prompt)?;
            Ok(EventStatus::Handled)
        }
        #[cfg(not(unix))]
        {
            let _ = prompt;
            Ok(EventStatus::Inapplicable)
        }
    }

    /// Time left before a pending multi-key sequence is flushed, or `None`
    /// when nothing is pending.
    ///
    /// The deadline starts with the first pending key and is reset after every
    /// key that arrives, so the timeout is the wait for the *next* key of the
    /// sequence, matching readline's `keyseq-timeout`.
    fn keyseq_remaining(&mut self) -> Option<Duration> {
        if !self.edit_mode.has_pending_sequence() {
            self.keyseq_deadline = None;
            return None;
        }
        let deadline = *self
            .keyseq_deadline
            .get_or_insert_with(|| Instant::now() + self.keyseq_timeout);
        Some(deadline.saturating_duration_since(Instant::now()))
    }

    /// Flush a pending sequence whose deadline has passed.
    fn flush_pending_keyseq(&mut self, prompt: &dyn Prompt) -> Result<ControlFlow<Signal>> {
        self.keyseq_deadline = None;
        let event = self.edit_mode.flush_pending_sequence();
        if event == ReedlineEvent::None {
            return Ok(ControlFlow::Continue(()));
        }
        self.process_reedline_events(prompt, vec![(event, 0)])
    }

    fn handle_event(&mut self, prompt: &dyn Prompt, event: ReedlineEvent) -> Result<EventStatus> {
        // A replayed key sequence is input, not an action: it queues at the
        // head of the next input batch and resolves through whatever keymap
        // is live then — the same in every input mode.
        if let ReedlineEvent::KeySequence(keys) = event {
            self.enqueue_replayed_keys(keys, self.replay_depth + 1);
            return Ok(EventStatus::Handled);
        }
        if event == ReedlineEvent::Suspend {
            return self.suspend(prompt);
        }
        // The terminating key of a numeric argument is dispatched under
        // the count (GNU `_rl_arg_dispatch`, misc.c:174). Our events take
        // no count, so repetition stands in for it: the event runs
        // |count| times, stopping at the first non-`Handled` status — a
        // submit still exits once, an `UntilFound` miss is not retried.
        // GNU hands functions the SIGNED count; direction-aware commands
        // (negative self-insert deleting backward, misc.c consumers
        // reading `rl_explicit_arg`) are not expressed yet — the signed
        // `count` and `explicit` ride along for them.
        if let ReedlineEvent::WithArg { count, event, .. } = event {
            if count == 0 {
                // An explicit `M-0` dispatches with count 0.
                return Ok(EventStatus::Handled);
            }
            // Argument keys dispatched under a count run once (GNU's
            // `rl_digit_argument` ignores its count, misc.c:230) — a
            // `C-u`-terminated argument can seed the next one.
            let reps = match *event {
                ReedlineEvent::DigitArgument(_) | ReedlineEvent::UniversalArgument => 1,
                _ => count.unsigned_abs(),
            };
            let mut status = EventStatus::Handled;
            for _ in 0..reps {
                status = self.handle_event(prompt, (*event).clone())?;
                if !matches!(status, EventStatus::Handled) {
                    break;
                }
            }
            return Ok(status);
        }
        self.edit_dispatched = false;
        // Macro controls and argument keys are engine state — a real
        // key press reaches here only through a `Multiple`, a `WithArg`
        // dispatch, or a programmatic push, so they apply without the
        // parse-time keystroke bookkeeping (recording pops, arg spend).
        match event {
            ReedlineEvent::StartKbdMacro => {
                self.start_kbd_macro(false);
                return Ok(EventStatus::Handled);
            }
            ReedlineEvent::EndKbdMacro => {
                return Ok(if self.end_kbd_macro() {
                    EventStatus::Handled
                } else {
                    EventStatus::Inapplicable
                });
            }
            ReedlineEvent::CallLastKbdMacro => {
                if self.recording.is_some() || self.last_macro.is_empty() {
                    return Ok(EventStatus::Inapplicable);
                }
                self.call_last_kbd_macro(1);
                return Ok(EventStatus::Handled);
            }
            ReedlineEvent::DigitArgument(digit) => {
                self.arg_digit(digit);
                return Ok(EventStatus::Handled);
            }
            ReedlineEvent::UniversalArgument => {
                self.arg_universal();
                return Ok(EventStatus::Handled);
            }
            ReedlineEvent::PrintLastKbdMacro => {
                // `rl_print_last_kbd_macro` (macro.c:309): ding when no
                // recording exists; otherwise print the keyseq spelling
                // above the prompt like GNU's `rl_crlf` + `printf`.
                if self.last_macro.is_empty() {
                    return Ok(EventStatus::Inapplicable);
                }
                self.painter.print_external_message(vec![
                    crate::edit_mode::key_sequence_spelling(&self.last_macro),
                ])?;
                return Ok(EventStatus::Handled);
            }
            _ => {}
        }
        let status = if self.input_mode == InputMode::HistorySearch {
            self.handle_history_search_event(event)
        } else if self.input_mode == InputMode::NonIncrementalSearch {
            self.handle_noninc_search_event(event)
        } else {
            self.handle_editor_event(prompt, event)
        };
        // readline.c:715-716: the charloop clears `_rl_last_command_was_kill`
        // after every dispatched command that didn't rearm it. An `Edit`
        // batch arms the bookkeeping itself inside `Editor::run_edit_command`;
        // a handled event that ran no `EditCommand` never reaches the editor,
        // so its kill-chain and `rl_last_func` records are broken here —
        // GNU invalidates `M-y` on any intervening command, not only edits.
        if !self.edit_dispatched
            && matches!(status, Ok(EventStatus::Handled | EventStatus::Exits(_)))
        {
            self.editor.note_non_edit_dispatch();
        }
        status
    }

    fn handle_history_search_event(&mut self, event: ReedlineEvent) -> io::Result<EventStatus> {
        match event {
            ReedlineEvent::UntilFound(events) => {
                for event in events {
                    match self.handle_history_search_event(event)? {
                        EventStatus::Inapplicable => {
                            // Try again with the next event handler
                        }
                        success => {
                            return Ok(success);
                        }
                    }
                }
                // No candidate applied, so nothing changed: report that, which
                // also lets an enclosing `UntilFound` keep trying.
                Ok(EventStatus::Inapplicable)
            }
            ReedlineEvent::CtrlD => {
                if self.editor.is_empty() {
                    self.input_mode = InputMode::Regular;
                    self.editor.reset_undo_stack();
                    Ok(EventStatus::Exits(Signal::CtrlD))
                } else {
                    self.run_history_commands(&[EditCommand::Delete])?;
                    Ok(EventStatus::Handled)
                }
            }
            ReedlineEvent::CtrlC => {
                self.input_mode = InputMode::Regular;
                Ok(EventStatus::Exits(Signal::CtrlC))
            }
            ReedlineEvent::ClearScreen => {
                self.painter.clear_screen()?;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::ClearScrollback => {
                self.painter.clear_scrollback()?;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::Enter
            | ReedlineEvent::HistoryHintComplete
            | ReedlineEvent::Submit
            | ReedlineEvent::SubmitOrNewline => {
                if let Some(string) = self.history_cursor.string_at_cursor() {
                    // GNU `_rl_isearch_fini` (isearch.c:300-304) leaves
                    // point at `cxt->sline_index` — the start of the
                    // matched substring — not at the end of the recalled
                    // line. Reverse isearch scans a fresh history line
                    // from its right edge (isearch.c:854
                    // `sline_len - search_string_index`), so the last
                    // occurrence is the one it landed on.
                    let point = match self.history_cursor.get_navigation() {
                        HistoryNavigationQuery::SubstringSearch(needle) if !needle.is_empty() => {
                            string.rfind(&needle).unwrap_or(string.len())
                        }
                        _ => string.len(),
                    };
                    self.editor
                        .set_buffer(string, UndoBehavior::CreateUndoPoint);
                    let point = point.min(self.editor.get_buffer().len());
                    self.editor.edit_buffer(
                        |buf| buf.set_insertion_point(point),
                        UndoBehavior::MoveCursor,
                    );
                    self.editor.commit_cursor();
                }
                // Accepting an isearch match leaves the history position
                // on the found entry (GNU's isearch lands `where_history`
                // there too), which `OperateAndGetNext`/`YankLastArg` read.
                self.history_recall_pos = self.history_cursor.id_at_cursor();

                self.input_mode = InputMode::Regular;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::ExecuteHostCommand(host_command) => {
                self.last_render_snapshot = None;
                self.suspended_state = Some(self.painter.state_before_suspension());
                Ok(EventStatus::Exits(Signal::HostCommand(host_command)))
            }
            ReedlineEvent::Edit(commands) => {
                self.run_history_commands(&commands)?;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::Mouse {
                column,
                row,
                button,
            } => {
                if button == MouseButton::Left {
                    self.handle_mouse_click(column, row)?;
                }
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::Resize(width, height) => {
                self.last_render_snapshot = None;
                self.painter.handle_resize(width, height);
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::Repaint => {
                // A handled Event causes a repaint
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::PreviousHistory | ReedlineEvent::Up | ReedlineEvent::SearchHistory => {
                self.history_cursor.back(self.history.as_ref())?;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::NextHistory | ReedlineEvent::Down => {
                self.history_cursor.forward(self.history.as_ref())?;
                // Hacky way to ensure that we don't fall of into failed search going forward
                if self.history_cursor.string_at_cursor().is_none() {
                    self.history_cursor.back(self.history.as_ref())?;
                }
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::FirstHistoryEntry => {
                if self.history_cursor.jump_to_oldest(self.history.as_ref())? {
                    Ok(EventStatus::Handled)
                } else {
                    Ok(EventStatus::Inapplicable)
                }
            }
            ReedlineEvent::LastHistoryEntry => {
                if self.history_cursor.past_newest() {
                    Ok(EventStatus::Handled)
                } else {
                    Ok(EventStatus::Inapplicable)
                }
            }
            ReedlineEvent::Esc => {
                self.input_mode = InputMode::Regular;
                Ok(EventStatus::Handled)
            }
            // TODO: Check if events should be handled
            ReedlineEvent::Right
            | ReedlineEvent::Left
            | ReedlineEvent::ToStart
            | ReedlineEvent::ToEnd
            | ReedlineEvent::KeySequence(_)
            | ReedlineEvent::QuotedInsert
            | ReedlineEvent::CharSearch { .. }
            | ReedlineEvent::YankLastArg { .. }
            | ReedlineEvent::YankNthArg { .. }
            | ReedlineEvent::OperateAndGetNext
            | ReedlineEvent::HistorySearch { .. }
            | ReedlineEvent::Pending
            | ReedlineEvent::StartKbdMacro
            | ReedlineEvent::EndKbdMacro
            | ReedlineEvent::CallLastKbdMacro
            | ReedlineEvent::DigitArgument(_)
            | ReedlineEvent::UniversalArgument
            | ReedlineEvent::WithArg { .. }
            | ReedlineEvent::OverwriteMode(_)
            | ReedlineEvent::NonIncrementalSearch { .. }
            | ReedlineEvent::NonIncrementalSearchAgain { .. }
            | ReedlineEvent::PrintLastKbdMacro
            | ReedlineEvent::Multiple(_)
            | ReedlineEvent::None
            | ReedlineEvent::HistoryHintWordComplete
            | ReedlineEvent::OpenEditor
            | ReedlineEvent::Menu(_)
            | ReedlineEvent::MenuAccept
            | ReedlineEvent::MenuNext
            | ReedlineEvent::MenuPrevious
            | ReedlineEvent::MenuUp
            | ReedlineEvent::MenuDown
            | ReedlineEvent::MenuLeft
            | ReedlineEvent::MenuRight
            | ReedlineEvent::MenuPageNext
            | ReedlineEvent::MenuPagePrevious
            | ReedlineEvent::Suspend
            | ReedlineEvent::PossibleCompletions
            | ReedlineEvent::InsertCompletions
            | ReedlineEvent::SwitchMode(_) => Ok(EventStatus::Inapplicable),
        }
    }

    /// The [`InputMode::NonIncrementalSearch`] machine — GNU's
    /// `_rl_nsearch_dispatch` (search.c:295): the buffer is the search
    /// string, `Enter` commits, `C-g`/`C-c`/`Esc` abort and restore,
    /// `Backspace` on an empty buffer aborts, ordinary edits apply.
    fn handle_noninc_search_event(&mut self, event: ReedlineEvent) -> io::Result<EventStatus> {
        match event {
            ReedlineEvent::UntilFound(events) | ReedlineEvent::Multiple(events) => {
                for event in events {
                    match self.handle_noninc_search_event(event)? {
                        EventStatus::Inapplicable => {}
                        success => {
                            return Ok(success);
                        }
                    }
                }
                Ok(EventStatus::Inapplicable)
            }
            ReedlineEvent::Enter | ReedlineEvent::Submit | ReedlineEvent::SubmitOrNewline => {
                // `_rl_nsearch_dosearch` (search.c:398): take the buffer
                // as the string, restore the saved line, then run the
                // one search. An empty string reuses the last one from
                // its last position (the `rl_point == 0` branch).
                let Some(state) = self.noninc_search.take() else {
                    self.input_mode = InputMode::Regular;
                    return Ok(EventStatus::Handled);
                };
                let string = self.editor.get_buffer().to_string();
                self.input_mode = InputMode::Regular;
                let (string, from) = if string.is_empty() {
                    match self.noninc_search_string.clone() {
                        Some(saved) => (saved, self.noninc_search_pos),
                        // No saved string — `_rl_unsave_saved_search_line`
                        // + `rl_ding` (search.c:415-424).
                        None => {
                            self.restore_noninc_saved_line(state);
                            return Ok(EventStatus::Inapplicable);
                        }
                    }
                } else {
                    self.noninc_search_string = Some(string.clone());
                    (string, state.save_line)
                };
                let (direction, pattern) = (state.direction, state.pattern);
                self.restore_noninc_saved_line(state);
                self.noninc_dosearch(&string, direction, pattern, from)
            }
            ReedlineEvent::CtrlC | ReedlineEvent::Esc => {
                // `_rl_nsearch_abort` (search.c:270): saved line and
                // point back, prompt restored. `C-g` reaches this arm as
                // `Esc` — GNU's abort key, not a `Redo` edit.
                if let Some(state) = self.noninc_search.take() {
                    self.restore_noninc_saved_line(state);
                }
                self.input_mode = InputMode::Regular;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::Edit(commands) => {
                // Rubout at `rl_point == 0` aborts (search.c:331).
                if self.editor.insertion_point() == 0 && commands.contains(&EditCommand::Backspace)
                {
                    if let Some(state) = self.noninc_search.take() {
                        self.restore_noninc_saved_line(state);
                    }
                    self.input_mode = InputMode::Regular;
                    return Ok(EventStatus::Handled);
                }
                self.run_edit_commands(&commands);
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::QuotedInsert => {
                self.quoted_insert = true;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::CtrlD => {
                // `C-d` exits only on an empty search string, like the
                // isearch mode's `CtrlD` arm; with text it deletes.
                if self.editor.is_empty() {
                    if let Some(state) = self.noninc_search.take() {
                        self.restore_noninc_saved_line(state);
                    }
                    self.input_mode = InputMode::Regular;
                    Ok(EventStatus::Exits(Signal::CtrlD))
                } else {
                    self.run_edit_commands(&[EditCommand::Delete]);
                    Ok(EventStatus::Handled)
                }
            }
            ReedlineEvent::ExecuteHostCommand(host_command) => {
                self.last_render_snapshot = None;
                self.suspended_state = Some(self.painter.state_before_suspension());
                Ok(EventStatus::Exits(Signal::HostCommand(host_command)))
            }
            ReedlineEvent::Resize(width, height) => {
                self.last_render_snapshot = None;
                self.painter.handle_resize(width, height);
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::Repaint => Ok(EventStatus::Handled),
            ReedlineEvent::ClearScreen => {
                self.painter.clear_screen()?;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::ClearScrollback => {
                self.painter.clear_scrollback()?;
                Ok(EventStatus::Handled)
            }
            // Anything else GNU would read as a literal input byte has
            // no reedline form; the event is ignored, as a key with no
            // dispatch case is in `_rl_nsearch_dispatch`.
            _ => Ok(EventStatus::Inapplicable),
        }
    }

    /// `_rl_nsearch_init` (search.c:230): save the line, clear the
    /// buffer to read the search string into it, swap the prompt for
    /// the one-char `pchar`, set `RL_STATE_NSEARCH`.
    fn enter_noninc_search(&mut self, direction: Direction, prompt_char: char, pattern: bool) {
        self.noninc_search = Some(NonIncrementalSearchState {
            saved_buffer: self.editor.get_buffer().to_string(),
            saved_insertion_point: self.editor.insertion_point(),
            prompt_char,
            direction,
            pattern,
            save_line: self.history_recall_pos,
            saved_edit_mode: None,
        });
        self.editor
            .set_buffer(String::new(), UndoBehavior::CreateUndoPoint);
        self.input_mode = InputMode::NonIncrementalSearch;
        // Under vi command mode GNU reads the string through the
        // vi-insertion keymap (`_rl_nsearch_init`, search.c:112), so
        // typed characters insert rather than parse as commands. The
        // saved mode comes back on the way out.
        if let PromptEditMode::Vi(mode) = self.edit_mode.edit_mode() {
            if mode != crate::PromptViMode::Insert {
                self.noninc_search.as_mut().unwrap().saved_edit_mode =
                    Some(PromptEditMode::Vi(mode));
                self.change_edit_mode(ReedlineEvent::SwitchMode(PromptEditMode::Vi(
                    crate::PromptViMode::Insert,
                )));
            }
        }
    }

    /// `_rl_unsave_saved_search_line` (search.c:287 /
    /// `_rl_nsearch_abort`): the line the search displaced is put back,
    /// with its saved point.
    fn restore_noninc_saved_line(&mut self, state: NonIncrementalSearchState) {
        self.editor
            .set_buffer(state.saved_buffer, UndoBehavior::CreateUndoPoint);
        let point = state.saved_insertion_point;
        self.editor.edit_buffer(
            move |buf| buf.set_insertion_point(point.min(buf.len())),
            UndoBehavior::MoveCursor,
        );
        self.editor.commit_cursor();
        // `_rl_nsearch_fini`: the command-mode keymap comes back.
        if let Some(mode) = state.saved_edit_mode {
            self.change_edit_mode(ReedlineEvent::SwitchMode(mode));
        }
    }

    /// `noninc_dosearch` (search.c:179): find `string` in history one
    /// entry past `from` in `direction` — a leading `^` anchors the
    /// match to the entry start (search.c:145-149), `pattern` switches
    /// to GNU's `SF_PATTERN` glob (`_hs_history_patsearch`,
    /// histsearch.c:245). On a hit the entry becomes the buffer with
    /// the point at its start (`rl_point = 0`, `rl_mark = rl_end`,
    /// search.c:214-219) and `noninc_history_pos` updates; on a miss
    /// nothing changes (GNU leaves the line as restored and dings).
    fn noninc_dosearch(
        &mut self,
        string: &str,
        direction: Direction,
        pattern: bool,
        from: Option<HistoryItemId>,
    ) -> io::Result<EventStatus> {
        if string.is_empty() {
            return Ok(EventStatus::Inapplicable);
        }
        // Forward from the live line has nowhere to go
        // (`noninc_history_pos + 1` past the newest entry).
        if direction == Direction::Forward && from.is_none() {
            return Ok(EventStatus::Inapplicable);
        }
        let (anchored, needle) = match string.strip_prefix('^') {
            Some(rest) => (true, rest),
            None => (false, string),
        };
        let results = self.history.search(SearchQuery {
            direction: match direction {
                Direction::Backward => SearchDirection::Backward,
                Direction::Forward => SearchDirection::Forward,
            },
            start_time: None,
            end_time: None,
            start_id: from,
            end_id: None,
            limit: None,
            filter: SearchFilter::anything(self.get_history_session_id()),
        })?;
        let found = results.iter().find(|item| {
            // `pos + dir` in GNU is strict — `Forward` here includes
            // `from` itself, so skip it (the `-again` calls restart on
            // the last hit, search.c:192).
            if direction == Direction::Forward && item.id == from {
                return false;
            }
            if pattern {
                noninc_pat_search(needle, &item.command_line, anchored)
            } else if anchored {
                item.command_line.starts_with(needle)
            } else {
                item.command_line.contains(needle)
            }
        });
        match found {
            Some(item) => {
                let item = item.clone();
                self.noninc_search_pos = item.id;
                self.history_recall_pos = item.id;
                self.editor
                    .set_buffer(item.command_line, UndoBehavior::HistoryNavigation);
                // `rl_point = 0; rl_mark = rl_end` (search.c:217-219).
                let len = self.editor.get_buffer().len();
                self.editor.edit_buffer(
                    move |buf| {
                        buf.set_insertion_point(len);
                        buf.set_mark();
                        buf.set_insertion_point(0);
                    },
                    UndoBehavior::MoveCursor,
                );
                self.editor.commit_cursor();
                Ok(EventStatus::Handled)
            }
            None => Ok(EventStatus::Inapplicable),
        }
    }

    /// `rl_noninc_*_search_again` (search.c:517/:541): repeat the last
    /// search with the saved string from `noninc_history_pos` — the vi
    /// `n`/`N` path (`rl_vi_search_again`, vi_mode.c:348), which GNU
    /// always runs under `SF_PATTERN`. Ding before the first search.
    fn noninc_search_again(&mut self, direction: Direction) -> io::Result<EventStatus> {
        let Some(string) = self.noninc_search_string.clone() else {
            return Ok(EventStatus::Inapplicable);
        };
        self.noninc_dosearch(&string, direction, true, self.noninc_search_pos)
    }

    fn handle_editor_event(
        &mut self,
        prompt: &dyn Prompt,
        event: ReedlineEvent,
    ) -> io::Result<EventStatus> {
        // GNU's `rl_last_func` chains: `rl_yank_last_arg`'s repeat-walk
        // (kill.c:654) and `rl_history_search_*`'s reinit guards
        // (search.c:701,737,755) both resume only while the previous
        // command was the same one — anything else resets the state.
        // Nested `Multiple`/`UntilFound` events pass through here
        // individually, which matches GNU treating each command as the
        // last func.
        if !matches!(event, ReedlineEvent::YankLastArg { .. }) {
            self.yank_last_arg = None;
        }
        if !matches!(event, ReedlineEvent::HistorySearch { .. }) {
            self.history_line_search = None;
        }
        match event {
            ReedlineEvent::PossibleCompletions => Ok(self.list_possible_completions()),
            ReedlineEvent::InsertCompletions => Ok(self.insert_all_completions()),
            ReedlineEvent::Menu(name) => {
                if self.active_menu().is_none() {
                    if let Some(index) = self.menus.iter().position(|menu| menu.name() == name) {
                        self.menus[index].menu_event(MenuEvent::Activate(self.quick_completions));
                        invalidate_anchor_if_host_completer_runs(
                            &self.menus[index],
                            &mut self.painter,
                        );

                        if self.decide_menu_completion(index, false) {
                            return self.handle_editor_event(prompt, ReedlineEvent::Enter);
                        }

                        // A final answer already had its say above, so only a
                        // provisional one leaves anything owed. With neither option set
                        // nothing queried the completer, so this reads the default
                        // `false` and the menu paints as it always has.
                        if self.menus[index].results_are_provisional() {
                            self.deferred_menu_completion = Some(DeferredMenuCompletion::new(
                                &self.menus[index],
                                &self.editor,
                            ));
                        }

                        return Ok(EventStatus::Handled);
                    }
                }
                Ok(EventStatus::Inapplicable)
            }
            ReedlineEvent::MenuAccept => {
                // Where a menu's claim on a keypress is decided, for the submit
                // events that offer it one as much as for a binding that wants
                // the accept alone. A menu declines the keypress unless it can
                // splice something: with no suggestions there is nothing to
                // take, and a *stale* value is refused downstream because its
                // span belongs to another line, so accepting one would close
                // the menu over a completion that never happened. Declining
                // leaves the key to reach the line instead of appearing dead.
                match self.menus.iter_mut().find(|menu| menu.is_active()) {
                    Some(menu)
                        if !menu.get_values().is_empty() && !menu.results_are_provisional() =>
                    {
                        menu.replace_in_buffer(&mut self.editor);
                        menu.menu_event(MenuEvent::Deactivate);
                        Ok(EventStatus::Handled)
                    }
                    _ => Ok(EventStatus::Inapplicable),
                }
            }
            ReedlineEvent::MenuNext => {
                if let Some(menu) = self.menus.iter_mut().find(|menu| menu.is_active()) {
                    // The second route to the lone-value accept, so it carries the same
                    // provisional guard as `decide_menu_completion`: accepting a lone
                    // *stale* value is refused downstream, but the `Enter` would still
                    // deactivate the menu, closing it over a completion that never
                    // happened.
                    if menu.get_values().len() == 1
                        && menu.can_quick_complete()
                        && !menu.results_are_provisional()
                    {
                        self.handle_editor_event(prompt, ReedlineEvent::Enter)
                    } else {
                        if self.partial_completions {
                            menu.can_partially_complete(
                                self.quick_completions,
                                &mut self.editor,
                                self.completer.as_mut(),
                                self.history.as_ref(),
                            );
                            invalidate_anchor_if_host_completer_runs(menu, &mut self.painter);
                        }
                        menu.menu_event(MenuEvent::NextElement);
                        Ok(EventStatus::Handled)
                    }
                } else {
                    Ok(EventStatus::Inapplicable)
                }
            }
            ReedlineEvent::MenuPrevious => {
                self.active_menu()
                    .map_or(Ok(EventStatus::Inapplicable), |menu| {
                        menu.menu_event(MenuEvent::PreviousElement);
                        Ok(EventStatus::Handled)
                    })
            }
            ReedlineEvent::MenuUp => {
                self.active_menu()
                    .map_or(Ok(EventStatus::Inapplicable), |menu| {
                        menu.menu_event(MenuEvent::MoveUp);
                        Ok(EventStatus::Handled)
                    })
            }
            ReedlineEvent::MenuDown => {
                self.active_menu()
                    .map_or(Ok(EventStatus::Inapplicable), |menu| {
                        menu.menu_event(MenuEvent::MoveDown);
                        Ok(EventStatus::Handled)
                    })
            }
            ReedlineEvent::MenuLeft => {
                self.active_menu()
                    .map_or(Ok(EventStatus::Inapplicable), |menu| {
                        menu.menu_event(MenuEvent::MoveLeft);
                        Ok(EventStatus::Handled)
                    })
            }
            ReedlineEvent::MenuRight => {
                self.active_menu()
                    .map_or(Ok(EventStatus::Inapplicable), |menu| {
                        menu.menu_event(MenuEvent::MoveRight);
                        Ok(EventStatus::Handled)
                    })
            }
            // These two spell out `active_menu()`, since that borrows all of `self` and
            // the painter has to stay reachable alongside the menu.
            ReedlineEvent::MenuPageNext => {
                match self.menus.iter_mut().find(|menu| menu.is_active()) {
                    Some(menu) => {
                        menu.menu_event(MenuEvent::NextPage);
                        invalidate_anchor_if_host_completer_runs(menu, &mut self.painter);
                        Ok(EventStatus::Handled)
                    }
                    None => Ok(EventStatus::Inapplicable),
                }
            }
            ReedlineEvent::MenuPagePrevious => {
                match self.menus.iter_mut().find(|menu| menu.is_active()) {
                    Some(menu) => {
                        menu.menu_event(MenuEvent::PreviousPage);
                        invalidate_anchor_if_host_completer_runs(menu, &mut self.painter);
                        Ok(EventStatus::Handled)
                    }
                    None => Ok(EventStatus::Inapplicable),
                }
            }
            ReedlineEvent::KeySequence(keys) => {
                // Nested inside `Multiple`: queues for the next input
                // batch — the Multiple's remaining events still run
                // first (documented on the variant).
                self.enqueue_replayed_keys(keys, self.replay_depth + 1);
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::QuotedInsert => {
                self.quoted_insert = true;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::CharSearch { direction } => {
                // Reached when the event is dispatched outside an input
                // batch (e.g. from a `Multiple` handled event-side);
                // the batch parse path arms this flag directly.
                self.pending_char_search = Some(direction);
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::YankLastArg { count } => self.yank_last_arg(count),
            ReedlineEvent::YankNthArg { count } => self.yank_nth_arg(count),
            ReedlineEvent::OperateAndGetNext => {
                // Record the entry *after* the current history position
                // for the next `read_line` to preload (GNU
                // `saved_history_logical_offset`, misc.c:786), then run
                // the line like `Submit` (GNU `rl_newline` first).
                self.operate_and_get_next = Some(self.history_recall_pos);
                self.handle_editor_event(prompt, ReedlineEvent::Submit)
            }
            ReedlineEvent::HistorySearch {
                direction,
                anchored,
            } => self.history_search_line(direction, anchored),
            ReedlineEvent::NonIncrementalSearch {
                direction,
                prompt_char,
                pattern,
            } => {
                self.enter_noninc_search(direction, prompt_char, pattern);
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::NonIncrementalSearchAgain { direction } => {
                self.noninc_search_again(direction)
            }
            ReedlineEvent::OverwriteMode(on) => {
                // GNU rl_overwrite_mode: arg>0 sets, arg<=0 clears,
                // no arg toggles.
                let next = on.unwrap_or(!self.editor.overwrite());
                self.editor.set_overwrite(next);
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::HistoryHintComplete => Ok(self.accept_history_hint(true)),
            ReedlineEvent::HistoryHintWordComplete => Ok(self.accept_history_hint(false)),
            ReedlineEvent::Esc => {
                self.deactivate_menus();
                self.editor.clear_selection();
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::CtrlD => {
                if self.editor.is_empty() {
                    self.editor.reset_undo_stack();
                    Ok(EventStatus::Exits(Signal::CtrlD))
                } else {
                    self.run_edit_commands(&[EditCommand::Delete]);
                    Ok(EventStatus::Handled)
                }
            }
            ReedlineEvent::CtrlC => {
                self.deactivate_menus();
                self.run_edit_commands(&[EditCommand::Clear]);
                self.editor.reset_undo_stack();
                Ok(EventStatus::Exits(Signal::CtrlC))
            }
            ReedlineEvent::ClearScreen => {
                self.deactivate_menus();
                self.painter.clear_screen()?;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::ClearScrollback => {
                self.deactivate_menus();
                self.painter.clear_scrollback()?;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::Enter => {
                if self.menu_took_the_key(prompt)? {
                    return Ok(EventStatus::Handled);
                }
                #[cfg(feature = "bashisms")]
                if let Some(event) = self.parse_bang_command() {
                    return self.handle_editor_event(prompt, event);
                }
                if let Some(event) = self.try_expand_abbreviation_at_cursor(true) {
                    self.handle_editor_event(prompt, event)?;
                }

                let buffer = self.editor.get_buffer().to_string();
                match self.validator.as_mut().map(|v| v.validate(&buffer)) {
                    None | Some(ValidationResult::Complete) => Ok(self.submit_buffer(prompt)?),
                    Some(ValidationResult::Incomplete) => {
                        self.run_edit_commands(&[EditCommand::InsertNewline]);

                        Ok(EventStatus::Handled)
                    }
                }
            }
            ReedlineEvent::Submit => {
                if self.menu_took_the_key(prompt)? {
                    return Ok(EventStatus::Handled);
                }
                #[cfg(feature = "bashisms")]
                if let Some(event) = self.parse_bang_command() {
                    return self.handle_editor_event(prompt, event);
                }
                if let Some(event) = self.try_expand_abbreviation_at_cursor(true) {
                    self.handle_editor_event(prompt, event)?;
                }

                Ok(self.submit_buffer(prompt)?)
            }
            ReedlineEvent::SubmitOrNewline => {
                if self.menu_took_the_key(prompt)? {
                    return Ok(EventStatus::Handled);
                }
                #[cfg(feature = "bashisms")]
                if let Some(event) = self.parse_bang_command() {
                    return self.handle_editor_event(prompt, event);
                }
                if let Some(event) = self.try_expand_abbreviation_at_cursor(true) {
                    self.handle_editor_event(prompt, event)?;
                }

                let cursor_position_in_buffer = self.editor.insertion_point();
                let buffer = self.editor.get_buffer().to_string();
                if cursor_position_in_buffer < buffer.len() {
                    self.run_edit_commands(&[EditCommand::InsertNewline]);
                    return Ok(EventStatus::Handled);
                }
                match self.validator.as_mut().map(|v| v.validate(&buffer)) {
                    None | Some(ValidationResult::Complete) => Ok(self.submit_buffer(prompt)?),
                    Some(ValidationResult::Incomplete) => {
                        self.run_edit_commands(&[EditCommand::InsertNewline]);

                        Ok(EventStatus::Handled)
                    }
                }
            }
            ReedlineEvent::ExecuteHostCommand(host_command) => {
                self.last_render_snapshot = None;
                self.suspended_state = Some(self.painter.state_before_suspension());
                Ok(EventStatus::Exits(Signal::HostCommand(host_command)))
            }
            ReedlineEvent::Edit(commands) => {
                // Intercept a lone `PasteSystem` when a paste interceptor
                // is installed. The hook reads the clipboard itself and decides
                // what to insert (a reference placeholder, the raw text, or
                // nothing) — bypassing the default clipboard-read-and-insert.
                // Only a lone `PasteSystem` command is intercepted; any other
                // edit (or a compound batch) falls through to the normal path
                // unchanged. `PasteSystem` only exists under `system_clipboard`,
                // so gate the whole interception on that feature. The text the
                // hook returns is run as an ordinary `InsertString` through the
                // rest of this arm, so an open menu sees the edit exactly as it
                // would for a plain `PasteSystem`; a `Noop` changes nothing and
                // is done here.
                #[cfg(feature = "system_clipboard")]
                let commands = match (commands.as_slice(), self.paste_interceptor.clone()) {
                    ([EditCommand::PasteSystem], Some(interceptor)) => match interceptor.on_paste()
                    {
                        crate::PasteAction::InsertText(s) => vec![EditCommand::InsertString(s)],
                        crate::PasteAction::Noop => return Ok(EventStatus::Handled),
                    },
                    _ => commands,
                };

                let status = self.run_edit_commands_with_status(&commands);

                // Ahead of everything below, which would otherwise ask a
                // completer about a word the user has already left, or return
                // through the abbreviation expansion without ever looking.
                if status == EditCommandStatus::Applied {
                    self.close_a_menu_past_its_word(&commands);
                }

                // Check if a space was just inserted and try to expand abbreviations
                if status == EditCommandStatus::Applied
                    && matches!(commands.first(), Some(EditCommand::InsertChar(' ')))
                {
                    if let Some(event) = self.try_expand_abbreviation_at_cursor(false) {
                        return self.handle_editor_event(prompt, event);
                    }
                }

                if let Some(menu) = self.menus.iter_mut().find(|menu| menu.is_active()) {
                    let quick_complete = self.quick_completions && menu.can_quick_complete();
                    // Dismissing a menu handles the event even when the cursor didn't move.
                    if quick_complete
                        && !self.persistent_menus
                        && matches!(
                            commands.first(),
                            Some(EditCommand::Backspace)
                                | Some(EditCommand::BackspaceWord)
                                | Some(EditCommand::MoveToLineStart { select: false })
                        )
                    {
                        menu.menu_event(MenuEvent::Deactivate);
                        return Ok(EventStatus::Handled);
                    }
                    if status == EditCommandStatus::Inapplicable {
                        return Ok(EventStatus::Inapplicable);
                    }
                    if quick_complete {
                        menu.menu_event(MenuEvent::Edit(self.quick_completions));
                        invalidate_anchor_if_host_completer_runs(menu, &mut self.painter);
                        menu.update_values(
                            &mut self.editor,
                            self.completer.as_mut(),
                            self.history.as_ref(),
                        );
                        if let Some(&EditCommand::Complete) = commands.first() {
                            if menu.get_values().len() == 1 {
                                return self.handle_editor_event(prompt, ReedlineEvent::Enter);
                            } else if self.partial_completions
                                && menu.can_partially_complete(
                                    self.quick_completions,
                                    &mut self.editor,
                                    self.completer.as_mut(),
                                    self.history.as_ref(),
                                )
                            {
                                return Ok(EventStatus::Handled);
                            }
                        }
                    }
                    if !self.persistent_menus && self.editor.line_buffer().get_buffer().is_empty() {
                        menu.menu_event(MenuEvent::Deactivate);
                    } else {
                        menu.menu_event(MenuEvent::Edit(self.quick_completions));
                        invalidate_anchor_if_host_completer_runs(menu, &mut self.painter);
                    }
                }
                Ok(status.into())
            }
            ReedlineEvent::OpenEditor => self.open_editor().map(|_| EventStatus::Handled),
            ReedlineEvent::Resize(width, height) => {
                self.last_render_snapshot = None;
                self.painter.handle_resize(width, height);
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::Repaint => {
                // A handled Event causes a repaint
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::PreviousHistory => {
                self.previous_history()?;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::NextHistory => {
                self.next_history()?;
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::FirstHistoryEntry => self.first_history(),
            ReedlineEvent::LastHistoryEntry => Ok(self.last_history()),
            ReedlineEvent::Up => self.up_command(),
            ReedlineEvent::Down => self.down_command(),
            ReedlineEvent::Left | ReedlineEvent::Right => {
                let command = if event == ReedlineEvent::Left {
                    EditCommand::MoveLeft { select: false }
                } else {
                    EditCommand::MoveRight { select: false }
                };
                Ok(self.run_edit_commands_with_status(&[command]).into())
            }
            ReedlineEvent::ToStart | ReedlineEvent::ToEnd => {
                let initial_cursor = self.editor.line_buffer().cursor();
                // Keep these aliases outside the edit-command path: they do not
                // create undo points or end history traversal.
                if event == ReedlineEvent::ToStart {
                    self.editor.move_to_start(false);
                } else {
                    self.editor.move_to_end(false);
                }
                // Compare after settling under the rest policy, including vi's
                // block caret resting on the last grapheme rather than past it.
                self.editor.commit_cursor();
                Ok(if self.editor.line_buffer().cursor() == initial_cursor {
                    EventStatus::Inapplicable
                } else {
                    EventStatus::Handled
                })
            }
            ReedlineEvent::SearchHistory => {
                self.enter_history_search();
                Ok(EventStatus::Handled)
            }
            ReedlineEvent::Multiple(events) => {
                let mut latest_signal = EventStatus::Inapplicable;
                let mut rest = events.into_iter();
                while let Some(event) = rest.next() {
                    // A mode that flushes a parked chord hands the prefix
                    // and the current key back as one `Multiple` — niu's
                    // wrapper does this for `C-x`. The parse loop's
                    // recording corrections only run for top-level
                    // events, so repeat them here (see
                    // [`Self::note_nested_chord_key`]).
                    self.note_nested_chord_key(&event);
                    match self.handle_event(prompt, event)? {
                        EventStatus::Handled => {
                            latest_signal = EventStatus::Handled;
                        }
                        EventStatus::Inapplicable => {
                            // NO OP
                        }
                        EventStatus::Exits(signal) => {
                            // A host command only suspends the editor: the
                            // rest of this `Multiple` is still queued input
                            // (GNU's input buffer survives `rl_abort`), so
                            // park it for the resumed read instead of
                            // dropping it.
                            if matches!(&signal, Signal::HostCommand(_)) {
                                let depth = self.replay_depth;
                                self.pending_events.extend(rest.map(|event| (event, depth)));
                            }
                            return Ok(EventStatus::Exits(signal));
                        }
                    }
                }

                Ok(latest_signal)
            }
            ReedlineEvent::UntilFound(events) => {
                for event in events {
                    match self.handle_editor_event(prompt, event)? {
                        EventStatus::Inapplicable => {
                            // Try again with the next event handler
                        }
                        success => {
                            return Ok(success);
                        }
                    }
                }
                // No candidate applied, so nothing changed: report that, which
                // also lets an enclosing `UntilFound` keep trying.
                Ok(EventStatus::Inapplicable)
            }
            ReedlineEvent::SwitchMode(_) => Ok(self.change_edit_mode(event)),
            ReedlineEvent::Mouse {
                column,
                row,
                button,
            } => {
                if button == MouseButton::Left {
                    self.handle_mouse_click(column, row)?;
                }
                Ok(EventStatus::Handled)
            }
            // Everything below is applied at parse time or intercepted in
            // `handle_event` before the mode dispatch — reaching this arm
            // means a programmatic call, where inapplicable is honest.
            ReedlineEvent::None
            | ReedlineEvent::Pending
            | ReedlineEvent::StartKbdMacro
            | ReedlineEvent::EndKbdMacro
            | ReedlineEvent::CallLastKbdMacro
            | ReedlineEvent::DigitArgument(_)
            | ReedlineEvent::UniversalArgument
            | ReedlineEvent::Suspend
            | ReedlineEvent::PrintLastKbdMacro
            | ReedlineEvent::WithArg { .. } => Ok(EventStatus::Inapplicable),
        }
    }

    /// Route a `SwitchMode` event to the machine that accepts it, make that
    /// machine the active one, then repair the cursor the flip left behind.
    ///
    /// Routing: a target naming the state the active machine already reports
    /// is not a move, so it is declined before any machine is asked. Otherwise
    /// the active machine is offered the target first, then the standbys in
    /// registration order; the first to accept is swapped in. When none
    /// accepts nothing changes either. Both `Inapplicable` answers let an
    /// enclosing `UntilFound` keep trying, which is what makes
    /// `UntilFound([SwitchMode(A), SwitchMode(B)])` a toggle.
    ///
    /// Repair: a machine's own transitions emit their repairs as events, the way `i`
    /// collapses the selection on the way into helix insert. An event-driven
    /// flip never reaches that path, so the repair has to happen here. What
    /// bites is a selection arriving in a mode that has none: a block policy
    /// rests as a min-width-1 selection and a bar caret can carry a
    /// shift-selection, and either way `insert_char` and every vi operator
    /// consume a live selection, so the first keystroke would eat text. It
    /// collapses onto the caret, where the machines' own `Esc` leaves it.
    ///
    /// Stated over the `RestPolicy` being entered rather than per machine, so
    /// emacs, both insert modes and vi normal are one rule instead of four
    /// cases, and a future machine inherits it.
    fn change_edit_mode(&mut self, event: ReedlineEvent) -> EventStatus {
        if let ReedlineEvent::SwitchMode(target) = &event {
            if *target == self.edit_mode.edit_mode() {
                return EventStatus::Inapplicable;
            }
        }

        let mut status = self.edit_mode.handle_mode_specific_event(event.clone());
        if matches!(status, EventStatus::Inapplicable) {
            // Offering is not a query: the standby that accepts has already
            // moved into the target state, and `EditMode` asks the ones that
            // decline to stay as they were.
            for standby in &mut self.standby_edit_modes {
                if let EventStatus::Handled = standby.handle_mode_specific_event(event.clone()) {
                    std::mem::swap(&mut self.edit_mode, standby);
                    status = EventStatus::Handled;
                    break;
                }
            }
        }

        let after = self.edit_mode.edit_mode();
        if matches!(status, EventStatus::Handled) && !after.rest_policy().is_block() {
            // Collapse first, while the editor still holds the mode being left
            // and so knows where its caret shows. Adopting the new policy
            // afterwards leaves the settle to the pre-paint commit; a commit
            // under the block policy being left would re-widen the point.
            self.editor.clear_selection();
            self.editor.sync_edit_mode(after);
        }
        status
    }

    fn handle_mouse_click(&mut self, column: u16, row: u16) -> Result<()> {
        let snapshot = match &self.last_render_snapshot {
            Some(snapshot) => snapshot,
            None => return Ok(()),
        };
        if self.input_mode != InputMode::Regular || self.menus.iter().any(|m| m.is_active()) {
            return Ok(());
        }
        let buffer = self.editor.get_buffer();
        if let Some(offset) = self.painter.screen_to_buffer_offset(snapshot, column, row) {
            if buffer.is_char_boundary(offset) {
                self.editor.edit_buffer(
                    |buf| buf.set_insertion_point(offset),
                    UndoBehavior::MoveCursor,
                );
            }
        }
        Ok(())
    }

    fn active_menu(&mut self) -> Option<&mut ReedlineMenu> {
        self.menus.iter_mut().find(|menu| menu.is_active())
    }

    fn deactivate_menus(&mut self) {
        // Nothing is left to complete into.
        self.deferred_menu_completion = None;
        self.menus
            .iter_mut()
            .for_each(|menu| menu.menu_event(MenuEvent::Deactivate));
    }

    fn previous_history(&mut self) -> io::Result<()> {
        self.history_last_run.set_recalled(false);
        if self.input_mode != InputMode::HistoryTraversal {
            self.input_mode = InputMode::HistoryTraversal;
            self.history_cursor = HistoryCursor::new(
                self.get_history_navigation_based_on_line_buffer(),
                self.get_history_session_id(),
            );

            if matches!(self.history_last_run, HistoryLastRun::Excluded { .. }) {
                self.history_last_run.set_recalled(true);
            }
        }

        if !self.history_last_run.is_recalled() {
            // On `Err` the next press retries on the fresh cursor; no rollback.
            self.history_cursor.back(self.history.as_ref())?;
        }
        self.update_buffer_from_history();
        self.editor.move_to_start(false);
        self.editor.move_to_line_end(false);
        // History navigation positions the cursor outside the command path, so
        // settle it under the rest policy (vi-normal must not rest past the line).
        self.editor.commit_cursor();
        self.editor
            .update_undo_state(UndoBehavior::HistoryNavigation);
        Ok(())
    }

    /// Jump to the oldest history entry — GNU `beginning-of-history`
    /// (`rl_beginning_of_history`, misc.c), which walks `previous-history`
    /// back to the start and dings when there is no entry to show. So it
    /// is inapplicable from the oldest entry and when nothing matches the
    /// navigation query, and composes in `UntilFound`.
    fn first_history(&mut self) -> io::Result<EventStatus> {
        if self.input_mode != InputMode::HistoryTraversal {
            self.history_cursor = HistoryCursor::new(
                self.get_history_navigation_based_on_line_buffer(),
                self.get_history_session_id(),
            );
        }
        if !self.history_cursor.jump_to_oldest(self.history.as_ref())? {
            return Ok(EventStatus::Inapplicable);
        }

        self.input_mode = InputMode::HistoryTraversal;
        // The jump lands on a stored entry, never on the recalled last run.
        self.history_last_run.set_recalled(false);
        self.update_buffer_from_history();
        self.editor.move_to_start(false);
        self.editor.move_to_line_end(false);
        self.editor.commit_cursor();
        self.editor
            .update_undo_state(UndoBehavior::HistoryNavigation);
        Ok(EventStatus::Handled)
    }

    /// Leave the history for the line that was being typed — GNU
    /// `end-of-history` (`rl_end_of_history` → `rl_maybe_unsave_line`,
    /// misc.c), which dings when no history navigation saved a line. So it
    /// is inapplicable outside history traversal.
    fn last_history(&mut self) -> EventStatus {
        if self.input_mode != InputMode::HistoryTraversal {
            return EventStatus::Inapplicable;
        }

        self.history_cursor.past_newest();
        self.history_last_run.set_recalled(false);
        self.input_mode = InputMode::Regular;
        self.update_buffer_from_history();
        self.editor.move_to_end(false);
        self.editor.commit_cursor();
        self.editor
            .update_undo_state(UndoBehavior::HistoryNavigation);
        EventStatus::Handled
    }

    fn next_history(&mut self) -> io::Result<()> {
        if self.input_mode != InputMode::HistoryTraversal {
            self.input_mode = InputMode::HistoryTraversal;
            self.history_cursor = HistoryCursor::new(
                self.get_history_navigation_based_on_line_buffer(),
                self.get_history_session_id(),
            );
        }

        if self.history_last_run.is_recalled() {
            self.history_last_run.set_recalled(false);
        } else {
            let cursor_was_on_item = self.history_cursor.string_at_cursor().is_some();
            self.history_cursor.forward(self.history.as_ref())?;

            if cursor_was_on_item
                && self.history_cursor.string_at_cursor().is_none()
                && matches!(self.history_last_run, HistoryLastRun::Excluded { .. })
            {
                self.history_last_run.set_recalled(true);
            }
        }

        if self.history_cursor.string_at_cursor().is_none() && !self.history_last_run.is_recalled()
        {
            self.input_mode = InputMode::Regular;
        }
        self.update_buffer_from_history();
        self.editor.move_to_end(false);
        // See `previous_history`: settle the out-of-band cursor under the policy.
        self.editor.commit_cursor();
        self.editor
            .update_undo_state(UndoBehavior::HistoryNavigation);
        Ok(())
    }

    /// Enable the search and navigation through the history from the line buffer prompt
    ///
    /// Enables either prefix search with output in the line buffer or simple traversal
    fn get_history_navigation_based_on_line_buffer(&self) -> HistoryNavigationQuery {
        if self.editor.is_empty() || !self.editor.is_cursor_at_buffer_end() {
            // Perform bash-style basic up/down entry walking
            HistoryNavigationQuery::Normal(
                // Hack: Tight coupling point to be able to restore previously typed input
                self.editor.line_buffer().clone(),
            )
        } else {
            // Prefix search like found in fish, zsh, etc.
            // Search string is set once from the current buffer
            // Current setup (code in other methods)
            // Continuing with typing will leave the search
            // but next invocation of this method will start the next search
            let buffer = self.editor.get_buffer().to_string();
            HistoryNavigationQuery::PrefixSearch(buffer)
        }
    }

    /// Switch into reverse history search mode
    ///
    /// This mode uses a separate prompt and handles keybindings slightly differently!
    fn enter_history_search(&mut self) {
        self.history_cursor = HistoryCursor::new(
            HistoryNavigationQuery::SubstringSearch("".to_string()),
            self.get_history_session_id(),
        );
        self.input_mode = InputMode::HistorySearch;
    }

    /// The command line of the history entry `skip` entries before the
    /// current history position — `rl_yank_nth_arg_internal`'s
    /// `previous_history()` chain (kill.c:613-617): `skip` of 0 is the
    /// immediately previous entry (the newest one at the live line).
    ///
    /// Returns `Ok(None)` when fewer than `skip + 1` entries precede the
    /// position — GNU's `previous_history()` returns `NULL` there and
    /// the caller dings.
    fn history_line_at_skip(&self, skip: i64) -> Result<Option<HistoryItem>> {
        let results = self.history.search(SearchQuery {
            direction: SearchDirection::Backward,
            start_time: None,
            end_time: None,
            start_id: self.history_recall_pos,
            end_id: None,
            limit: Some(skip + 1),
            filter: SearchFilter::anything(self.get_history_session_id()),
        })?;
        Ok(if results.len() as i64 == skip + 1 {
            results.into_iter().last()
        } else {
            None
        })
    }

    /// Insert one argument of a history entry — the shared tail of
    /// `yank_last_arg`/`yank_nth_arg`, GNU `history_arg_extract` +
    /// `rl_insert_text` (kill.c:628-644).
    ///
    /// `arg` is `None` for `'$'` (the last token); `Some(n)` indexes
    /// with 0 = command name, negatives counting back from the end —
    /// the same indexing `history_arg_extract(first, last, string)`
    /// uses (histexpand.c:1424).
    ///
    /// The tokens come from [`history_tokenize`], the port of GNU's
    /// tokenizer (`history_tokenize`, histexpand.c:1692): quoting and
    /// operators are honored, and tokens keep their quote characters —
    /// `!$` on `echo "two words"` yanks `"two words"`.
    fn yank_history_arg(&mut self, entry: &HistoryItem, arg: Option<i32>) -> EventStatus {
        let tokens = history_tokenize(&entry.command_line);
        let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
        let len = tokens.len() as i64;
        let index = match arg {
            // `'$'` is the last argument (kill.c:676 passes it through
            // when no explicit arg was given).
            None => len - 1,
            Some(n) if n >= 0 => n as i64,
            Some(n) => len + n as i64 - 1,
        };
        if index < 0 || index >= len {
            // `!arg || !*arg` → `rl_ding` (kill.c:631).
            return EventStatus::Inapplicable;
        }
        self.run_edit_commands(&[EditCommand::InsertString(
            tokens[index as usize].to_string(),
        )]);
        EventStatus::Handled
    }

    /// GNU `yank-last-arg` (`M-.`/`M-_`, kill.c:649 `rl_yank_last_arg`):
    /// yank an argument of the previous history line; a consecutive
    /// repeat undoes the previous yank and walks `history_skip`
    /// progressively older entries.
    fn yank_last_arg(&mut self, count: Option<i32>) -> Result<EventStatus> {
        let continuing = self.yank_last_arg.is_some();
        let mut state = self.yank_last_arg.take().unwrap_or(YankLastArgState {
            history_skip: 0,
            explicit_arg: count.is_some(),
            count: count.unwrap_or(0),
            direction: 1,
            undo_needed: false,
        });

        if continuing {
            if state.undo_needed {
                // GNU `rl_do_undo` of the insert group
                // (kill.c:663-665) — our `InsertString` is always its
                // own undo point, so one `Undo` removes it.
                self.run_edit_commands(&[EditCommand::Undo]);
                state.undo_needed = false;
            }
            if count.is_some_and(|c| c < 0) {
                state.direction = -state.direction;
            }
            state.history_skip += state.direction;
            if state.history_skip < 0 {
                state.history_skip = 0;
            }
        }

        let status = match self.history_line_at_skip(state.history_skip)? {
            Some(entry) => self.yank_history_arg(&entry, state.explicit_arg.then_some(state.count)),
            // `entry == 0` → `rl_ding` (kill.c:621). A failed press
            // still consumes the skip step, as in GNU
            // (`undo_needed` stays false but `history_skip` advanced).
            None => EventStatus::Inapplicable,
        };
        state.undo_needed = matches!(status, EventStatus::Handled);
        self.yank_last_arg = Some(state);
        Ok(status)
    }

    /// GNU `yank-nth-arg` (`M-C-y`, kill.c:644 `rl_yank_nth_arg` →
    /// `rl_yank_nth_arg_internal` with `history_skip` 0): insert the
    /// `count`th argument of the previous history line. Single-shot —
    /// unlike `yank_last_arg` it never walks.
    fn yank_nth_arg(&mut self, count: i32) -> Result<EventStatus> {
        Ok(match self.history_line_at_skip(0)? {
            Some(entry) => self.yank_history_arg(&entry, Some(count)),
            None => EventStatus::Inapplicable,
        })
    }

    /// `rl_operate_and_get_next`'s startup hook (misc.c:770-789
    /// `set_saved_history`): on the `read_line` after a `C-o`, preload
    /// the entry after the history position the submitted line was at,
    /// and seed the history cursor so `Up`/`Down` continue from it.
    fn apply_operate_and_get_next(&mut self) {
        let Some(target) = self.operate_and_get_next.take() else {
            return;
        };
        // `None` inside: the submitted line sat at the live position —
        // the next entry number doesn't exist yet (GNU
        // `where_history()+history_base+1` past the end), so the new
        // line stays empty.
        let Some(after) = target else { return };
        let next = self
            .history
            .search(SearchQuery {
                direction: SearchDirection::Forward,
                start_time: None,
                end_time: None,
                // `start_id` is exclusive — the first entry strictly
                // after `after`, which survives the accepted command's
                // own `save` growing the history (GNU's absolute history
                // number gives the same durability).
                start_id: Some(after),
                end_id: None,
                limit: Some(1),
                filter: SearchFilter::anything(self.get_history_session_id()),
            })
            .ok()
            .and_then(|mut results| results.pop());
        if let Some(item) = next {
            self.history_recall_pos = item.id;
            self.editor
                .set_buffer(item.command_line.clone(), UndoBehavior::CreateUndoPoint);
            self.history_cursor = HistoryCursor::new(
                HistoryNavigationQuery::Normal(LineBuffer::default()),
                self.get_history_session_id(),
            );
            self.history_cursor.set_current(item);
            self.input_mode = InputMode::HistoryTraversal;
        }
    }

    /// GNU `history-search-backward`/`forward` (search.c:713/675) when
    /// `anchored`, `history-substr-search-backward`/`forward`
    /// (search.c:750/732) otherwise: search history for entries
    /// matching the buffer up to the insertion point.
    fn history_search_line(&mut self, direction: Direction, anchored: bool) -> Result<EventStatus> {
        let search_dir = match direction {
            Direction::Backward => SearchDirection::Backward,
            Direction::Forward => SearchDirection::Forward,
        };
        // The `rl_last_func` reinit guards (search.c:701,737,755): only
        // a consecutive press of the *same* family continues the search
        // position; a first press or a family switch reinitializes from
        // the current buffer.
        let continuing = self
            .history_line_search
            .as_ref()
            .is_some_and(|s| s.anchored == anchored);
        if !continuing {
            let search_len = self.editor.insertion_point();
            let needle = self.editor.get_buffer()[..search_len].to_string();
            self.history_line_search = Some(HistoryLineSearch {
                anchored,
                search_len,
            });
            if needle.is_empty() {
                // `_rl_history_search_len == 0` degrades to plain
                // previous/next-history (search.c:708,720).
                match search_dir {
                    SearchDirection::Backward => self.previous_history()?,
                    SearchDirection::Forward => self.next_history()?,
                }
                return Ok(EventStatus::Handled);
            }
            let query = if anchored {
                HistoryNavigationQuery::PrefixSearch(needle)
            } else {
                HistoryNavigationQuery::SubstringSearch(needle)
            };
            // The cursor carries the search (query + position);
            // `input_mode` is deliberately not HistoryTraversal so a
            // following plain Up/Down starts a fresh walk from the
            // buffer, as GNU's `get_previous_history` does after a
            // `history-search-*` command.
            self.history_cursor = HistoryCursor::new(query, self.get_history_session_id());
        }
        match search_dir {
            SearchDirection::Backward => self.history_cursor.back(self.history.as_ref())?,
            SearchDirection::Forward => self.history_cursor.forward(self.history.as_ref())?,
        }

        if let Some(found) = self.history_cursor.string_at_cursor() {
            self.history_recall_pos = self.history_cursor.id_at_cursor();
            // GNU's point placement (search.c:635-644): anchored lands
            // on `_rl_history_search_len`; substring lands on the match
            // offset `newcol` (GNU also sets `rl_mark` to the line end,
            // which reedline has no analog for).
            let point = if anchored {
                self.history_line_search
                    .as_ref()
                    .map_or(0, |s| s.search_len)
            } else {
                match self.history_cursor.get_navigation() {
                    HistoryNavigationQuery::SubstringSearch(needle) => {
                        found.find(&needle).unwrap_or(found.len())
                    }
                    _ => found.len(),
                }
            };
            self.editor
                .set_buffer(found, UndoBehavior::HistoryNavigation);
            let point = point.min(self.editor.get_buffer().len());
            self.editor.edit_buffer(
                |buf| buf.set_insertion_point(point),
                UndoBehavior::MoveCursor,
            );
            self.editor.commit_cursor();
            Ok(EventStatus::Handled)
        } else {
            // Search failed — GNU `rl_ding`s and restores the point to
            // `_rl_history_search_len` while the buffer keeps the last
            // found line (search.c:626, `temp == 0` arm).
            if let Some(state) = &self.history_line_search {
                let point = state.search_len.min(self.editor.get_buffer().len());
                self.editor.edit_buffer(
                    |buf| buf.set_insertion_point(point),
                    UndoBehavior::MoveCursor,
                );
                self.editor.commit_cursor();
            }
            Ok(EventStatus::Inapplicable)
        }
    }

    /// Dispatches the applicable [`EditCommand`] actions for editing the history search string.
    ///
    /// Only modifies internal state, does not perform regular output!
    fn run_history_commands(&mut self, commands: &[EditCommand]) -> io::Result<()> {
        for command in commands {
            match command {
                EditCommand::InsertChar(c) => {
                    let navigation = self.history_cursor.get_navigation();
                    if let HistoryNavigationQuery::SubstringSearch(mut substring) = navigation {
                        substring.push(*c);
                        self.history_cursor = HistoryCursor::new(
                            HistoryNavigationQuery::SubstringSearch(substring),
                            self.get_history_session_id(),
                        );
                    } else {
                        self.history_cursor = HistoryCursor::new(
                            HistoryNavigationQuery::SubstringSearch(String::from(*c)),
                            self.get_history_session_id(),
                        );
                    }
                    self.history_cursor.back(self.history.as_mut())?;
                }
                EditCommand::Backspace => {
                    let navigation = self.history_cursor.get_navigation();

                    if let HistoryNavigationQuery::SubstringSearch(substring) = navigation {
                        let new_substring = text_manipulation::remove_last_grapheme(&substring);

                        self.history_cursor = HistoryCursor::new(
                            HistoryNavigationQuery::SubstringSearch(new_substring.to_string()),
                            self.get_history_session_id(),
                        );
                        self.history_cursor.back(self.history.as_mut())?
                    }
                }
                _ => {
                    self.input_mode = InputMode::Regular;
                }
            }
        }
        Ok(())
    }

    /// Set the buffer contents for history traversal/search in the standard prompt
    ///
    /// When using the up/down traversal or fish/zsh style prefix search update the main line buffer accordingly.
    /// Not used for the separate modal reverse search!
    fn update_buffer_from_history(&mut self) {
        // Track the history position the buffer now shows — reedline's
        // `where_history()` for `OperateAndGetNext`/`YankLastArg`.
        // `None` means the live line (walked past the newest entry).
        self.history_recall_pos = self.history_cursor.id_at_cursor();
        if self.history_last_run.is_recalled() {
            if let HistoryLastRun::Excluded { item, .. } = &self.history_last_run {
                self.editor
                    .set_buffer(item.command_line.clone(), UndoBehavior::HistoryNavigation);
            }
            return;
        }

        match self.history_cursor.get_navigation() {
            HistoryNavigationQuery::Normal(original) => {
                if let Some(buffer_to_paint) = self.history_cursor.string_at_cursor() {
                    self.editor
                        .set_buffer(buffer_to_paint, UndoBehavior::HistoryNavigation);
                } else {
                    // Hack
                    self.editor
                        .set_line_buffer(original, UndoBehavior::HistoryNavigation);
                }
            }
            HistoryNavigationQuery::PrefixSearch(prefix)
            | HistoryNavigationQuery::SubstringSearch(prefix) => {
                let buffer = self.history_cursor.string_at_cursor().unwrap_or(prefix);
                self.editor
                    .set_buffer(buffer, UndoBehavior::HistoryNavigation);
            }
        }
    }

    /// Executes [`EditCommand`] actions by modifying the internal state appropriately. Does not output itself.
    pub fn run_edit_commands(&mut self, commands: &[EditCommand]) {
        self.run_edit_commands_with_status(commands);
    }

    fn run_edit_commands_with_status(&mut self, commands: &[EditCommand]) -> EditCommandStatus {
        let status = self.apply_edit_commands(commands);
        if status == EditCommandStatus::Applied && self.input_mode == InputMode::HistoryTraversal {
            self.input_mode = InputMode::Regular;
        }
        status
    }

    /// [`run_edit_commands`](Self::run_edit_commands) without ending history
    /// traversal, for the engine's own line moves inside a recalled entry.
    fn apply_edit_commands(&mut self, commands: &[EditCommand]) -> EditCommandStatus {
        // Adopt the current edit mode's rest policy so these commands resolve
        // under it (e.g. block-caret selection geometry) — but *without*
        // committing the cursor first. A commit here would apply the policy's
        // resting rule (e.g. `OnGrapheme` pulling an at-end point back) before
        // the commands run, double-stepping a mode-transition backstep like the
        // vi `Esc`→normal `MoveLeft`. The commands settle the cursor themselves,
        // and the pre-paint `set_edit_mode` makes the final commit.
        self.editor.sync_edit_mode(self.edit_mode.edit_mode());

        // Run every command, but report the edit as applied if any command applied.
        // This mirrors `Multiple`, where any handled event wins.
        let mut status = EditCommandStatus::Inapplicable;

        // The editor's kill-ring bookkeeping (`_rl_last_command_was_kill`,
        // `rl_last_func`) updates per command inside `run_edit_command`;
        // record that this dispatch did reach it so `handle_event` doesn't
        // clear the records afterward.
        if !commands.is_empty() {
            self.edit_dispatched = true;
        }

        for command in commands {
            if let Some(command) = self.auto_pair_command(command) {
                if self.editor.run_edit_command(&command) == EditCommandStatus::Applied {
                    status = EditCommandStatus::Applied;
                }
                continue;
            }

            if self.editor.run_edit_command(command) == EditCommandStatus::Applied {
                status = EditCommandStatus::Applied;
            }
        }

        status
    }

    fn auto_pair_command(&self, command: &EditCommand) -> Option<EditCommand> {
        let auto_pairs = self.auto_pairs.as_ref()?;

        // A menu in `Diff` mode reads what is typed after it opened as its
        // query, not as code, so a pair would end up in the search.
        let query_menu_open = self.menus.iter().any(|menu| {
            menu.is_active()
                && menu.settings().effective_input_mode() == crate::menu::InputMode::Diff
        });
        if query_menu_open {
            return None;
        }

        // Resolve which auto-pair action (if any) `command` would trigger, along
        // with the pair it acts on and the `EditCommand` that would replace it.
        // The search order matters: for `InsertChar`, closers are checked before
        // openers, so a character that is configured as both a closer of one pair
        // and an opener of another resolves based on whether the cursor currently
        // sits on the closer, not on the order pairs were registered in.
        let (pair, action, converted) = match command {
            EditCommand::InsertChar(ch) => {
                let closer_at_cursor = auto_pairs
                    .closing_pair(*ch)
                    .filter(|(_, close)| self.editor.is_auto_pair_closer_at_cursor(*close));

                if let Some(pair) = closer_at_cursor {
                    (
                        pair,
                        AutoPairAction::SkipExistingCloser,
                        EditCommand::MoveRight { select: false },
                    )
                } else if let Some((open, close)) = auto_pairs.opening_pair(*ch) {
                    (
                        (open, close),
                        AutoPairAction::Open,
                        EditCommand::InsertPair { open, close },
                    )
                } else {
                    return None;
                }
            }
            EditCommand::Backspace => {
                let pair = auto_pairs.pairs().find(|(open, close)| {
                    self.editor.is_empty_auto_pair_at_cursor(*open, *close)
                })?;
                (
                    pair,
                    AutoPairAction::BackspacePair,
                    EditCommand::BackspacePair {
                        open: pair.0,
                        close: pair.1,
                    },
                )
            }
            _ => return None,
        };

        // Give the highlighter a chance to veto the action before committing to
        // it. All three actions pass through this same gate: returning `false`
        // means "run the original command verbatim", handled by the caller
        // treating `None` from this function as a pass-through.
        let buffer = self.editor.get_buffer();
        let insertion_point = self.editor.insertion_point();
        let selection = self.editor.get_selection().map(|(start, end)| start..end);
        let context = AutoPairContext::new(buffer, insertion_point, pair, selection, action);

        if self.highlighter.should_auto_pair(&context) {
            Some(converted)
        } else {
            None
        }
    }

    /// Move the cursor up a line, or from the first line walk back into
    /// history. Reports `Inapplicable` when there was nowhere to go: the
    /// cursor on the first line and history already at its oldest entry, or
    /// empty. An honest report is what lets a binding built on
    /// [`ReedlineEvent::UntilFound`] fall through to its next event, as
    /// `Left` and `Right` already do at the edges of the line.
    fn up_command(&mut self) -> io::Result<EventStatus> {
        // If we're at the top, then:
        if self.editor.is_cursor_at_first_line() {
            // If we're at the top, move to previous history
            self.walk_and_report(Self::previous_history)
        } else {
            // Through `apply_edit_commands` so the cursor settles under the mode's
            // rest policy — a bare `editor.move_line_up` skips the commit boundary,
            // leaving a vi-normal caret past the last grapheme on a short line.
            Ok(self
                .apply_edit_commands(&[EditCommand::MoveLineUp { select: false }])
                .into())
        }
    }

    /// Move the cursor down a line, or from the last line walk forward through
    /// history. Reports `Inapplicable` when there was nowhere to go: the cursor
    /// on the last line of the line being typed, with nothing newer below it.
    /// See [`up_command`](Self::up_command).
    fn down_command(&mut self) -> io::Result<EventStatus> {
        // If we're at the bottom, then:
        if self.editor.is_cursor_at_last_line() {
            // If we're at the bottom, move to next history
            self.walk_and_report(Self::next_history)
        } else {
            // See `up_command`: settle under the rest policy via the commit boundary.
            Ok(self
                .apply_edit_commands(&[EditCommand::MoveLineDown { select: false }])
                .into())
        }
    }

    /// Run a history walk and report whether it moved anything: the buffer's
    /// text, or the cursor within it. The walk itself says nothing, since at
    /// either end of history it is a no-op by design, so the answer is read
    /// off the editor before and after.
    fn walk_and_report(
        &mut self,
        walk: fn(&mut Self) -> io::Result<()>,
    ) -> io::Result<EventStatus> {
        let text_before = self.editor.get_buffer().to_string();
        let cursor_before = self.editor.insertion_point();
        walk(self)?;
        let unmoved = self.editor.get_buffer() == text_before
            && self.editor.insertion_point() == cursor_before;
        Ok(if unmoved {
            EventStatus::Inapplicable
        } else {
            EventStatus::Handled
        })
    }

    /// Checks if hints should be displayed and are able to be completed
    fn hints_active(&self) -> bool {
        !self.hide_hints && matches!(self.input_mode, InputMode::Regular)
    }

    /// Accept the history hint, all of it when `whole`, else its next word.
    /// `Handled` only when a non-empty hint applies: hints active, no menu
    /// open, and the cursor either at the buffer end or, for a hinter that
    /// continues the text before the cursor, followed only by auto-pair
    /// closers the hint closes.
    ///
    /// At the buffer end the text is appended, positioned past the last
    /// grapheme first: a block caret (vi normal) rests *on* it, so a plain
    /// insert would split it. Before trailing closers the text goes in at the
    /// cursor: the closers it closes, as the highlighter says, are replaced by
    /// the ones it contains, and the others stay after the cursor.
    fn accept_history_hint(&mut self, whole: bool) -> EventStatus {
        let Some(hinter) = self.hinter.as_ref() else {
            return EventStatus::Inapplicable;
        };
        let full = hinter.complete_hint();
        let accepted = if whole {
            full.clone()
        } else {
            hinter.next_hint_token()
        };
        if !self.hints_active() || accepted.is_empty() || self.active_menu().is_some() {
            return EventStatus::Inapplicable;
        }

        // Checked before the buffer end: a block caret resting on the last
        // closer counts as at the end, but the text belongs before it.
        if let Some(closers) = self.trailing_closers_for_hint().map(str::to_owned) {
            let offsets = closer_offsets_for_hint(
                self.highlighter.as_ref(),
                self.editor.get_buffer(),
                self.editor.insertion_point(),
                &full,
                &closers,
            );
            // A partial acceptance must be a prefix of the hint to know which
            // closers it has reached.
            let Some(offsets) = offsets.filter(|_| full.starts_with(&accepted)) else {
                return EventStatus::Inapplicable;
            };
            let closed = offsets.iter().filter(|&&at| at < accepted.len()).count();
            let left_over = closers
                .char_indices()
                .nth(closed)
                .map_or("", |(index, _)| &closers[index..]);
            self.editor.sync_edit_mode(self.edit_mode.edit_mode());
            self.editor.replace_to_line_end(&accepted, left_over);
            return EventStatus::Handled;
        }

        if self.editor.is_cursor_at_buffer_end() {
            self.editor.prepare_append_at_buffer_end();
            self.run_edit_commands(&[EditCommand::InsertString(accepted)]);
            EventStatus::Handled
        } else {
            EventStatus::Inapplicable
        }
    }

    /// The text after the cursor when it is nothing but closers of the
    /// configured auto-pairs, like the `)` in `(gs|)`.
    fn trailing_auto_pair_closers(&self) -> Option<&str> {
        let auto_pairs = self.auto_pairs.as_ref()?;
        let after = &self.editor.get_buffer()[self.editor.insertion_point()..];
        let only_closers = after.chars().all(|c| auto_pairs.closing_pair(c).is_some());
        (!after.is_empty() && only_closers).then_some(after)
    }

    /// The trailing auto-pair closers a history hint may stand in for: those
    /// of [`Self::trailing_auto_pair_closers`], when the hinter continues the
    /// text before the cursor and nothing else claims the cursor area, which a
    /// selection or an open menu does.
    fn trailing_closers_for_hint(&self) -> Option<&str> {
        let hinter = self.hinter.as_ref()?;
        let free = hinter.continues_text_before_cursor()
            && self.hints_active()
            && !self.menus.iter().any(|menu| menu.is_active())
            && !self.editor.cursor_is_a_selection();
        free.then(|| self.trailing_auto_pair_closers()).flatten()
    }

    /// Repaint of either the buffer or the parts for reverse history search
    fn repaint(&mut self, prompt: &dyn Prompt) -> io::Result<()> {
        // Repainting
        if self.input_mode == InputMode::HistorySearch {
            self.history_search_paint(prompt)
        } else {
            self.buffer_paint(prompt)
        }
    }

    /// Expands an abbreviation at the word before the cursor, if any exists
    ///
    /// Calls [`Highlighter::should_expand_abbr`] with [`AbbrExpandContext::WordAbbreviation`]
    /// to decide whether expansion is permitted at the cursor position
    fn try_expand_abbreviation_at_cursor(&mut self, submitted: bool) -> Option<ReedlineEvent> {
        let buffer = self.editor.get_buffer();
        let cursor_position_in_buffer = self.editor.insertion_point();
        if cursor_position_in_buffer == 0 {
            return None;
        }

        let (offset, suffix) = match submitted {
            true => (0, ""),   // expand on <enter>
            false => (1, " "), // expand on <space>
        };

        // `offset` is a raw byte count (0 on <enter>, 1 on <space>), so
        // `cursor_position_in_buffer - offset` can land inside a multi-byte
        // UTF-8 char sitting just before the cursor (e.g. pasted CJK text).
        // Floor it down to the nearest char boundary before slicing to avoid
        // a panic.
        let word_end =
            crate::menu_functions::floor_char_boundary(buffer, cursor_position_in_buffer - offset);
        let prefix = &buffer[..word_end];
        let word_start = prefix
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(0); // byte offset of word start

        if word_start >= word_end {
            // The first char in the buffer is a space or there are consecutive spaces
            return None;
        }

        if submitted
            && buffer[word_end..]
                .chars()
                .next()
                .is_some_and(|ch| !ch.is_whitespace())
        {
            // The cursor is in the middle of a word, e.g. "hello|world"
            return None;
        }

        if !self.highlighter.should_expand_abbr(
            buffer,
            word_start,
            AbbrExpandContext::WordAbbreviation,
        ) {
            return None;
        }

        let word = &buffer[word_start..word_end];
        if let Some(expansion) = self.abbreviations.get(word) {
            return Some(ReedlineEvent::Edit(vec![
                EditCommand::MoveToPosition {
                    position: word_start,
                    select: false,
                },
                EditCommand::MoveToPosition {
                    // Select through the cursor, not just the end of the word, so
                    // the triggering space (already inserted on a <space> expansion)
                    // is replaced rather than left beside the inserted suffix.
                    position: cursor_position_in_buffer,
                    select: true,
                },
                EditCommand::InsertString(format!("{}{}", expansion, suffix)),
            ]));
        }

        None
    }

    #[cfg(feature = "bashisms")]
    /// Parses the ! command to replace entries from the history
    fn parse_bang_command(&mut self) -> Option<ReedlineEvent> {
        let buffer = self.editor.get_buffer();
        let parsed = parse_selection_char(buffer, '!');
        let parsed_prefix = parsed.prefix.unwrap_or_default().to_string();
        let parsed_marker = parsed.marker.unwrap_or_default().to_string();

        if let Some(last) = parsed.remainder.chars().last() {
            if last != ' ' {
                return None;
            }
        }

        if !self.highlighter.should_expand_abbr(
            buffer,
            parsed.remainder.len(),
            AbbrExpandContext::BangExpansion,
        ) {
            return None;
        }

        let history_result = parsed
            .index
            .zip(parsed.marker)
            .and_then(|(index, indicator)| match parsed.action {
                ParseAction::LastCommand => self
                    .history
                    .search(SearchQuery {
                        direction: SearchDirection::Backward,
                        start_time: None,
                        end_time: None,
                        start_id: None,
                        end_id: None,
                        limit: Some(1), // fetch the latest one entries
                        filter: SearchFilter::anything(self.get_history_session_id()),
                    })
                    .unwrap_or_else(|_| Vec::new())
                    .get(index.saturating_sub(1))
                    .map(|history| {
                        (
                            parsed.remainder.len(),
                            indicator.len(),
                            history.command_line.clone(),
                        )
                    }),
                ParseAction::BackwardSearch => self
                    .history
                    .search(SearchQuery {
                        direction: SearchDirection::Backward,
                        start_time: None,
                        end_time: None,
                        start_id: None,
                        end_id: None,
                        limit: Some(index as i64), // fetch the latest n entries
                        filter: SearchFilter::anything(self.get_history_session_id()),
                    })
                    .unwrap_or_else(|_| Vec::new())
                    .get(index.saturating_sub(1))
                    .map(|history| {
                        (
                            parsed.remainder.len(),
                            indicator.len(),
                            history.command_line.clone(),
                        )
                    }),
                ParseAction::BackwardPrefixSearch => {
                    let history_search_by_session = self
                        .history
                        .search(SearchQuery::last_with_prefix_and_cwd(
                            parsed.prefix.unwrap_or_default().to_string(),
                            self.cwd.clone().unwrap_or_else(|| {
                                std::env::current_dir()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string()
                            }),
                            self.get_history_session_id(),
                        ))
                        .unwrap_or_else(|_| Vec::new())
                        .get(index.saturating_sub(1))
                        .map(|history| {
                            (
                                parsed.remainder.len(),
                                parsed_prefix.len() + parsed_marker.len(),
                                history.command_line.clone(),
                            )
                        });
                    // If we don't find any history searching by session id, then let's
                    // search everything, otherwise use the result from the session search
                    if history_search_by_session.is_none() {
                        self.history
                            .search(SearchQuery::last_with_prefix(
                                parsed_prefix.clone(),
                                self.get_history_session_id(),
                            ))
                            .unwrap_or_else(|_| Vec::new())
                            .get(index.saturating_sub(1))
                            .map(|history| {
                                (
                                    parsed.remainder.len(),
                                    parsed_prefix.len() + parsed_marker.len(),
                                    history.command_line.clone(),
                                )
                            })
                    } else {
                        history_search_by_session
                    }
                }
                ParseAction::ForwardSearch => self
                    .history
                    .search(SearchQuery {
                        direction: SearchDirection::Forward,
                        start_time: None,
                        end_time: None,
                        start_id: None,
                        end_id: None,
                        limit: Some((index + 1) as i64), // fetch the oldest n entries
                        filter: SearchFilter::anything(self.get_history_session_id()),
                    })
                    .unwrap_or_else(|_| Vec::new())
                    .get(index)
                    .map(|history| {
                        (
                            parsed.remainder.len(),
                            indicator.len(),
                            history.command_line.clone(),
                        )
                    }),
                ParseAction::LastToken => self
                    .history
                    .search(SearchQuery::last_with_search(SearchFilter::anything(
                        self.get_history_session_id(),
                    )))
                    .unwrap_or_else(|_| Vec::new())
                    .first()
                    //BUGBUG: This returns the wrong results with paths with spaces in them
                    .and_then(|history| history.command_line.split_whitespace().next_back())
                    .map(|token| (parsed.remainder.len(), indicator.len(), token.to_string())),
            });

        if let Some((start, size, history)) = history_result {
            let edits = vec![
                EditCommand::MoveToPosition {
                    position: start,
                    select: false,
                },
                EditCommand::ReplaceChars(size, history),
            ];

            Some(ReedlineEvent::Edit(edits))
        } else {
            None
        }
    }

    fn open_editor(&mut self) -> Result<()> {
        let Some(buffer_editor) = &mut self.buffer_editor else {
            return Ok(());
        };

        buffer_editor.write_current_buffer(self.editor.get_buffer())?;

        // Capture the prompt's screen range so that an editor
        // that leaves the cursor untouched (e.g. an editor that
        // uses the alternate screen only) re-uses the existing
        // prompt rows instead of starting a new prompt a row
        // below the old one.
        let suspended_state = self.painter.state_before_suspension();
        {
            // Put back the raw mode the editor found once it exits,
            // whatever mode it leaves behind. Dropped at the end of
            // this block, before the position query below.
            let _tty_mode = TtyModeGuard::save();

            let mut child = if buffer_editor.is_template {
                buffer_editor
                    .render_command(self.editor.line_buffer())
                    .spawn()?
            } else {
                buffer_editor.command.spawn()?
            };

            // The child owns the tty now; invalidate eagerly so
            // any `?` early-return below still leaves the
            // painter in a safe state.
            self.painter.invalidate_prompt_start_row();
            child.wait()?;
        }

        // On the success path, re-initialize position and size
        // (covers a resize-during-editor with no SIGWINCH). If
        // the editor moved the cursor out of the prompt's rows
        // (it printed output), a fresh prompt starts below that
        // output. On query failure, the eager invalidate above
        // is our floor — losing the size refresh is acceptable;
        // losing the user's edited buffer below is not.
        let _ = self
            .painter
            .initialize_prompt_position(Some(&suspended_state));

        let res = buffer_editor.get_edited_buffer()?;

        self.editor.set_buffer(res, UndoBehavior::CreateUndoPoint);

        Ok(())
    }

    /// Repaint logic for the history reverse search
    ///
    /// Overwrites the prompt indicator and highlights the search string
    /// separately from the result buffer.
    fn history_search_paint(&mut self, prompt: &dyn Prompt) -> Result<()> {
        let use_ansi_coloring = self.effective_ansi_coloring();
        let navigation = self.history_cursor.get_navigation();

        if let HistoryNavigationQuery::SubstringSearch(substring) = navigation {
            let status =
                if !substring.is_empty() && self.history_cursor.string_at_cursor().is_none() {
                    PromptHistorySearchStatus::Failing
                } else {
                    PromptHistorySearchStatus::Passing
                };

            // The prompt passes through raw, but the search term is typed or
            // pasted, not the host's.
            let prompt_history_search =
                PromptHistorySearch::new(status, escape_control(&substring).into_owned());

            let res_string = self.history_cursor.string_at_cursor().unwrap_or_default();

            // Highlight matches
            let res_string = if use_ansi_coloring {
                let match_highlighter = SimpleMatchHighlighter::new(substring);
                let styled = match_highlighter.highlight(&res_string, 0);
                styled.render_simple()
            } else {
                escape_control(&res_string).into_owned()
            };

            let lines = PromptLines::new(
                prompt,
                self.prompt_edit_mode(),
                Some(prompt_history_search),
                &res_string,
                "",
                "",
            );

            self.painter.repaint_buffer(
                prompt,
                &lines,
                self.prompt_edit_mode(),
                None,
                use_ansi_coloring,
                &self.cursor_shapes,
            )?;
        } else {
            // Nothing to paint, but a commit just before may have left its
            // frame open and its bytes unsent, waiting for this repaint.
            self.painter.finish_frame()?;
        }

        Ok(())
    }

    /// Triggers a full repaint including the prompt parts
    ///
    /// Includes the highlighting and hinting calls.
    fn buffer_paint(&mut self, prompt: &dyn Prompt) -> Result<()> {
        let use_ansi_coloring = self.effective_ansi_coloring();
        let cursor_position_in_buffer = self.editor.insertion_point();
        let buffer_to_paint = self.editor.get_buffer();

        let mut styled_text = self
            .highlighter
            .highlight(buffer_to_paint, cursor_position_in_buffer);
        if let Some((from, to)) = self.editor.get_selection() {
            // With a cursor-cell style configured, the head cell gets it and
            // the selection style covers the rest of the range.
            match self
                .visual_selection_cursor_style
                .zip(self.editor.selection_head_cell())
            {
                Some((cursor_style, (cell_start, cell_end))) => {
                    if from < cell_start {
                        styled_text.style_range(from, cell_start, self.visual_selection_style);
                    }
                    styled_text.style_range(cell_start, cell_end, cursor_style);
                    if cell_end < to {
                        styled_text.style_range(cell_end, to, self.visual_selection_style);
                    }
                }
                None => styled_text.style_range(from, to, self.visual_selection_style),
            }
        }

        let (before_cursor, after_cursor) = styled_text.render_around_insertion_point(
            cursor_position_in_buffer,
            prompt,
            use_ansi_coloring,
            self.painter.semantic_markers(),
        );

        // With only auto-pair closers after the cursor, as in `(gs|)`, a
        // hinter that continues the text before it hints from that text and
        // its hint is drawn in place of the closers. A hint the highlighter
        // does not see closing them would drop them on accept, so none shows.
        let trailing_closers = self.trailing_closers_for_hint().map(str::to_owned);
        let hinted_line = match trailing_closers {
            Some(_) => &buffer_to_paint[..cursor_position_in_buffer],
            None => buffer_to_paint,
        };

        // Decided from the hint this paint looked up, never a stale one: the
        // paint that submit makes with hints hidden must draw the closers.
        let mut hint_replaces_closers = false;
        // Escaped here rather than in each hinter, so a host's own hinter is
        // covered too. SGR is kept only while coloring is on: hinters style
        // their own output, and with coloring off any SGR came from history.
        let hint: String = if self.hints_active() {
            self.hinter.as_mut().map_or_else(String::new, |hinter| {
                let hint = hinter.handle(
                    hinted_line,
                    cursor_position_in_buffer,
                    self.history.as_ref(),
                    use_ansi_coloring,
                    &self.cwd.clone().unwrap_or_else(|| {
                        std::env::current_dir()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    }),
                );
                let hint = if use_ansi_coloring {
                    escape_control_keep_sgr(&hint).into_owned()
                } else {
                    escape_control(&hint).into_owned()
                };
                let Some(closers) = trailing_closers.as_deref() else {
                    return hint;
                };
                // The raw hint, not the escaped one: the closers' offsets
                // index what accepting inserts.
                let full = hinter.complete_hint();
                hint_replaces_closers = !full.is_empty()
                    && closer_offsets_for_hint(
                        self.highlighter.as_ref(),
                        buffer_to_paint,
                        cursor_position_in_buffer,
                        &full,
                        closers,
                    )
                    .is_some();
                if hint_replaces_closers {
                    hint
                } else {
                    String::new()
                }
            })
        } else {
            String::new()
        };

        // Needs to add return carriage to newlines because when not in raw mode
        // some OS don't fully return the carriage

        let mut lines = if hint_replaces_closers {
            // The exit path reprints what follows the cursor, which is still
            // the closers, not the hint drawn over them.
            PromptLines::new(
                prompt,
                self.prompt_edit_mode(),
                None,
                &before_cursor,
                "",
                &hint,
            )
            .with_exit_after_cursor(&after_cursor)
        } else {
            PromptLines::new(
                prompt,
                self.prompt_edit_mode(),
                None,
                &before_cursor,
                &after_cursor,
                &hint,
            )
        };

        // Updating the working details of the active menu
        for menu in self.menus.iter_mut() {
            if menu.is_active() {
                // A menu still waiting on its first answer stays off screen, so a Tab
                // resolving to one suggestion never draws a menu it takes away again.
                if menu.is_visible() {
                    lines.prompt_indicator = menu.indicator().to_owned().into();
                }
                // If the menu requires the cursor position, update it (ide menu)
                let cursor_pos = lines.cursor_pos(self.painter.screen_width());
                menu.set_cursor_pos(cursor_pos);

                menu.update_working_details(
                    &mut self.editor,
                    self.completer.as_mut(),
                    self.history.as_ref(),
                    &self.painter,
                );

                // That update is where a first answer lands and ends the opening phase,
                // so ask again: the painter picks the menu to draw below, and an
                // indicator saying otherwise would draw its rows under the ordinary
                // prompt. Reading it twice is the price of the loop: the indicator sets
                // the prompt width that positions the cursor, which the update consumes,
                // so on the frame a menu opens that width lags by one paint.
                if menu.is_visible() {
                    lines.prompt_indicator = menu.indicator().to_owned().into();
                }
            }
        }

        // GNU's `rl_message` overlays: while a non-incremental search
        // reads its string the prompt becomes the one-char
        // `_rl_make_prompt_for_search` indicator (search.c:235), and a
        // pending numeric argument paints `(arg: N) `
        // (`_rl_arg_getchar`, misc.c:103). Reedline paints the
        // equivalent as an indicator swap rather than a message line.
        if let Some(state) = self.noninc_search.as_ref() {
            lines.prompt_indicator = state.prompt_char.to_string().into();
        } else if let Some(arg) = self.pending_arg {
            lines.prompt_indicator = format!("(arg: {}) ", arg.count()).into();
        }

        let menu = self.menus.iter().find(|menu| menu.is_visible());

        self.painter.repaint_buffer(
            prompt,
            &lines,
            self.prompt_edit_mode(),
            menu,
            use_ansi_coloring,
            &self.cursor_shapes,
        )?;

        if self.mouse_click_mode.is_enabled() {
            if let Some(layout) = &self.painter.last_layout {
                let buffer = self.editor.get_buffer();
                let (raw_before, raw_after) = buffer.split_at(cursor_position_in_buffer);
                self.last_render_snapshot = Some(
                    self.painter
                        .render_snapshot(&lines, menu, raw_before, raw_after, layout),
                );
            }
        } else {
            self.last_render_snapshot = None;
        }

        Ok(())
    }

    /// Adds an external printer
    pub fn with_external_printer(mut self, printer: impl ExternalOutput + 'static) -> Self {
        self.external_printer = Some(Box::new(printer));
        self
    }

    /// Sets the poll interval used when features that require periodic processing
    /// are active (e.g., external printer, idle callback).
    ///
    /// This controls how frequently Reedline yields control back to these features
    /// while waiting for user input. The default is 100ms.
    ///
    /// Common values are 33ms (~30fps) for UI updates or 100ms for less frequent tasks.
    ///
    /// Note: This setting only takes effect when an external printer or idle callback
    /// is configured. Without these features, Reedline blocks until input is received.
    ///
    /// # Example
    /// ```no_run
    /// use std::time::Duration;
    /// use reedline::Reedline;
    ///
    /// let editor = Reedline::create()
    ///     .with_poll_interval(Duration::from_millis(50));
    /// ```
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Sets how long a half-typed multi-key sequence waits for its next key
    /// before the pending prefix is flushed (default: 500ms).
    ///
    /// This matches readline's `keyseq-timeout`. When the timeout expires the
    /// mode resolves the pending keys through its fallback: emacs and vi
    /// insert self-insert the characters, vi normal replays its keybinding
    /// table. A value of [`Duration::ZERO`] flushes on the next poll without
    /// waiting.
    ///
    /// # Example
    /// ```no_run
    /// use std::time::Duration;
    /// use reedline::Reedline;
    ///
    /// let editor = Reedline::create()
    ///     .with_keyseq_timeout(Duration::from_millis(250));
    /// ```
    pub fn with_keyseq_timeout(mut self, timeout: Duration) -> Self {
        self.keyseq_timeout = timeout;
        self
    }

    /// Sets an idle callback that is called periodically while waiting for user input.
    ///
    /// This is useful for applications that need to process external events
    /// (such as GUI updates, network events, or timer-based operations) while
    /// the user is typing or the editor is waiting for input.
    ///
    /// Use [`with_poll_interval`](Self::with_poll_interval) to control how frequently
    /// the callback is invoked (default: 100ms).
    ///
    /// # Example
    /// ```no_run
    /// use std::time::Duration;
    /// use reedline::Reedline;
    ///
    /// let editor = Reedline::create()
    ///     .with_poll_interval(Duration::from_millis(33))
    ///     .with_idle_callback(Box::new(|| {
    ///         // Process external events here
    ///     }));
    /// ```
    pub fn with_idle_callback(mut self, callback: Box<dyn FnMut() + Send>) -> Self {
        self.idle_callback = Some(callback);
        self
    }

    /// Offer an open menu first refusal on a keypress that would otherwise
    /// submit, and report whether it took it.
    fn menu_took_the_key(&mut self, prompt: &dyn Prompt) -> io::Result<bool> {
        Ok(matches!(
            self.handle_editor_event(prompt, ReedlineEvent::MenuAccept)?,
            EventStatus::Handled
        ))
    }

    /// Close an active menu when a typed character ends the word it was opened
    /// for. Menus without word characters, and persistent menus, live on as
    /// they always have.
    ///
    /// Every command in the batch is read, not just the first: a burst of
    /// typing arrives as one `Edit` with several `InsertChar`s, and the word
    /// can end anywhere in it. A newline ends the word too, whether it came
    /// as `InsertChar('\n')` or as `InsertNewline`, the command the default
    /// `Alt+Enter` and `Shift+Enter` bindings emit.
    fn close_a_menu_past_its_word(&mut self, commands: &[EditCommand]) {
        if self.persistent_menus {
            return;
        }
        let Some(menu) = self.menus.iter_mut().find(|menu| menu.is_active()) else {
            return;
        };
        let word_ended = commands.iter().any(|command| match command {
            EditCommand::InsertChar(c) => menu.settings().word_ends_at(*c),
            EditCommand::InsertNewline => menu.settings().word_ends_at('\n'),
            _ => false,
        });
        if word_ended {
            menu.menu_event(MenuEvent::Deactivate);
        }
    }

    fn submit_buffer(&mut self, prompt: &dyn Prompt) -> io::Result<EventStatus> {
        // A menu that let the submit through (no suggestions to accept) must
        // not stay active into the next line's editing.
        self.deactivate_menus();
        #[cfg_attr(not(feature = "system_clipboard"), allow(unused_mut))]
        let mut buffer = self.editor.get_buffer().to_string();
        // Let an installed paste interceptor replace the line at submit (e.g.
        // expand paste-reference placeholders — the compact form is only for
        // composing). Done before the repaint below so reedline itself paints
        // the expanded (possibly multi-line) buffer with correct
        // wrapping/continuation; the replacement is also what `Signal::Success`
        // returns and what history records. A host may leave non-text
        // placeholders (e.g. an image reference) intact, since a terminal
        // cannot render them. The `and_then` yields an owned `Option<String>`,
        // releasing the immutable borrow of `self.paste_interceptor` before the
        // `&mut self` `run_edit_commands`.
        #[cfg(feature = "system_clipboard")]
        if let Some(expanded) = self
            .paste_interceptor
            .as_ref()
            .and_then(|i| i.expand_on_submit(&buffer))
        {
            self.run_edit_commands(&[
                EditCommand::Clear,
                EditCommand::InsertString(expanded.clone()),
            ]);
            buffer = expanded;
        }
        self.hide_hints = true;
        // Additional repaint to show the content without hints etc.
        if let Some(transient_prompt) = self.transient_prompt.take() {
            self.repaint(transient_prompt.as_ref())?;
            self.transient_prompt = Some(transient_prompt);
        } else {
            self.repaint(prompt)?;
        }
        if !buffer.is_empty() {
            let mut entry = HistoryItem::from_command_line(&buffer);
            entry.session_id = self.get_history_session_id();

            let excluded = self
                .history_exclusion_prefix
                .as_ref()
                .is_some_and(|prefix| buffer.starts_with(prefix));

            let saved = if excluded {
                None
            } else {
                match self.history.save(entry.clone()) {
                    Ok(saved) => Some(saved),
                    Err(err) => {
                        // Ran but not stored: the excluded shape. Keep the line,
                        // stash the error for `take_history_save_error`.
                        self.history_save_error = Some(err);
                        None
                    }
                }
            };

            match saved {
                Some(saved) => {
                    self.history_last_run = saved
                        .id
                        .map_or(HistoryLastRun::Empty, HistoryLastRun::Stored)
                }
                None => {
                    self.history_last_run = HistoryLastRun::Excluded {
                        item: entry,
                        current: true,
                        recalled: false,
                    }
                }
            }
        }
        self.run_edit_commands(&[EditCommand::Clear]);
        self.editor.reset_undo_stack();

        Ok(EventStatus::Exits(Signal::Success(buffer)))
    }
}

/// Asks `highlighter` how `candidate`, a hint for the text before `cursor` in
/// `line`, closes `closers`, the text after the cursor. The answer counts only
/// if it is sound: one strictly increasing offset per closer, each at a
/// character of `candidate` that is that closer.
fn closer_offsets_for_hint(
    highlighter: &dyn Highlighter,
    line: &str,
    cursor: usize,
    candidate: &str,
    closers: &str,
) -> Option<Vec<usize>> {
    let offsets = highlighter.hint_closer_offsets(line, cursor, candidate, closers)?;
    let mut previous = None;
    let sound = offsets.len() == closers.chars().count()
        && offsets
            .iter()
            .zip(closers.chars())
            .all(|(&offset, closer)| {
                let ascending = previous.is_none_or(|previous| offset > previous);
                previous = Some(offset);
                ascending
                    && candidate.get(offset..).and_then(|rest| rest.chars().next()) == Some(closer)
            });
    sound.then_some(offsets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_extensions::semantic_prompt::PromptKind;
    use crate::{
        ColumnarMenu, CompletionOrigin, CompletionResult, DefaultPrompt, Direction, FindStop,
        ListMenu, MenuBuilder, MotionTarget, PromptHelixMode, PromptViMode, Span, Suggestion,
    };
    use rstest::rstest;
    use std::path::Path;

    fn seam_engine(edit_mode: Box<dyn EditMode>) -> Reedline {
        let mut rl = Reedline::create().with_edit_mode(edit_mode);
        rl.painter.force_prompt_anchored_for_test(0);
        rl
    }

    /// Emacs with one binding added to its default table.
    fn emacs_with(
        modifiers: KeyModifiers,
        code: KeyCode,
        event: ReedlineEvent,
    ) -> Box<dyn EditMode> {
        let mut emacs = crate::default_emacs_keybindings();
        emacs.add_binding(modifiers, code, event);
        Box::new(crate::Emacs::new(emacs))
    }

    /// A vi or helix machine with `Alt-<c>` bound to `SwitchMode(target)` in
    /// every table, so the switch fires from whichever state a test lands in.
    fn machine_switching_on_alt(helix: bool, c: char, target: PromptEditMode) -> Box<dyn EditMode> {
        let bind = |mut table: crate::Keybindings| {
            table.add_binding(
                KeyModifiers::ALT,
                KeyCode::Char(c),
                ReedlineEvent::SwitchMode(target.clone()),
            );
            table
        };
        if helix {
            Box::new(
                crate::Helix::default()
                    .with_insert_keybindings(bind(crate::default_helix_insert_keybindings()))
                    .with_normal_keybindings(bind(crate::default_helix_normal_keybindings()))
                    .with_select_keybindings(bind(crate::default_helix_select_keybindings())),
            )
        } else {
            Box::new(crate::Vi::new(
                bind(crate::default_vi_insert_keybindings()),
                bind(crate::default_vi_normal_keybindings()),
                bind(crate::default_vi_visual_keybindings()),
            ))
        }
    }

    fn drive(rl: &mut Reedline, keys: &[KeyEvent]) {
        let prompt = DefaultPrompt::default();
        let events = keys.iter().copied().map(Event::Key).collect();
        let _ = rl.process_input_batch(&prompt, events).expect("batch ok");
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Drive each key as its own input batch, so vi mode transitions settle
    /// between presses the way real keystrokes arrive.
    fn type_each(rl: &mut Reedline, keys: &[KeyEvent]) {
        for k in keys {
            drive(rl, &[*k]);
        }
    }

    fn auto_pair_engine(pairs: &[(char, char)]) -> Reedline {
        Reedline::create().with_auto_pairs(AutoPairs::new(pairs.iter().copied()))
    }

    #[test]
    fn line_input_completed_treats_ctrl_j_as_line_ending() {
        // A plain Enter still completes the batch.
        assert!(Reedline::line_input_completed(&[Event::Key(key(
            KeyCode::Enter
        ))]));
        // A piped LF arrives as Ctrl+J in raw mode; it must also end the
        // batch, otherwise several LF-terminated lines that land in one drain
        // merge into a single batch and every line after the first is silently
        // dropped when the batch breaks on the first submit (issue #1262).
        assert!(Reedline::line_input_completed(&[Event::Key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)
        )]));
        // Ordinary keys do not complete the batch.
        assert!(!Reedline::line_input_completed(&[Event::Key(ch('a'))]));
        // Alt+J is not a line ending.
        assert!(!Reedline::line_input_completed(&[Event::Key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)
        )]));
        // An empty batch is not complete either.
        assert!(!Reedline::line_input_completed(&[]));
    }

    // --- multi-key sequence timeout ---

    fn chord_combo(c: char) -> crate::KeyCombination {
        crate::KeyCombination {
            modifier: KeyModifiers::NONE,
            key_code: KeyCode::Char(c),
        }
    }

    fn emacs_with_sequence(keys: &[crate::KeyCombination], event: ReedlineEvent) -> Reedline {
        let mut keybindings = crate::default_emacs_keybindings();
        keybindings.add_binding_sequence(keys, event);
        seam_engine(Box::new(crate::Emacs::new(keybindings)))
    }

    fn vi_insert_with_sequence(keys: &[crate::KeyCombination], event: ReedlineEvent) -> Reedline {
        let mut insert = crate::default_vi_insert_keybindings();
        insert.add_binding_sequence(keys, event);
        seam_engine(Box::new(crate::Vi::new(
            insert,
            crate::default_vi_normal_keybindings(),
            crate::default_vi_visual_keybindings(),
        )))
    }

    /// Let the pending sequence's deadline pass and flush it, as the read loop
    /// does when its poll times out.
    fn expire_pending_sequence(rl: &mut Reedline) {
        assert!(
            rl.edit_mode.has_pending_sequence(),
            "setup: sequence pending"
        );
        rl.keyseq_deadline = Some(Instant::now() - Duration::from_millis(1));
        assert!(rl
            .keyseq_remaining()
            .is_some_and(|remaining| remaining.is_zero()));
        let prompt = DefaultPrompt::default();
        let _ = rl
            .flush_pending_keyseq(&prompt)
            .expect("flush should succeed");
    }

    #[test]
    fn keyseq_timeout_defaults_to_500ms_and_is_configurable() {
        let rl = Reedline::create();
        assert_eq!(rl.keyseq_timeout, Duration::from_millis(500));

        let rl = Reedline::create().with_keyseq_timeout(Duration::from_millis(42));
        assert_eq!(rl.keyseq_timeout, Duration::from_millis(42));
    }

    #[test]
    fn a_new_read_line_drops_a_pending_sequence() {
        let mut rl = emacs_with_sequence(&[chord_combo('j'), chord_combo('j')], ReedlineEvent::Esc);

        drive(&mut rl, &[ch('j')]);
        assert!(rl.edit_mode.has_pending_sequence());

        rl.begin_read();
        assert!(!rl.edit_mode.has_pending_sequence());
    }

    #[test]
    fn a_complete_chord_runs_through_the_engine() {
        let mut rl = emacs_with_sequence(
            &[chord_combo('j'), chord_combo('j')],
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('X')]),
        );

        drive(&mut rl, &[ch('j')]);
        assert_eq!(rl.editor.get_buffer(), "", "the prefix must wait");
        drive(&mut rl, &[ch('j')]);

        assert_eq!(rl.editor.get_buffer(), "X");
    }

    #[test]
    fn an_expired_emacs_chord_self_inserts_its_prefix() {
        let mut rl = emacs_with_sequence(&[chord_combo('j'), chord_combo('j')], ReedlineEvent::Esc);

        drive(&mut rl, &[ch('j')]);
        assert_eq!(rl.editor.get_buffer(), "");
        expire_pending_sequence(&mut rl);

        assert_eq!(rl.editor.get_buffer(), "j");
    }

    #[test]
    fn an_expired_vi_insert_chord_self_inserts_its_prefix() {
        let mut rl =
            vi_insert_with_sequence(&[chord_combo('j'), chord_combo('j')], ReedlineEvent::Esc);

        drive(&mut rl, &[ch('j')]);
        assert_eq!(rl.editor.get_buffer(), "");
        expire_pending_sequence(&mut rl);

        assert_eq!(rl.editor.get_buffer(), "j");
    }

    #[test]
    fn an_expired_chord_uses_the_shorter_binding() {
        let mut keybindings = crate::default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char('j'),
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('J')]),
        );
        keybindings.add_binding_sequence(
            &[chord_combo('j'), chord_combo('j')],
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('X')]),
        );
        let mut rl = seam_engine(Box::new(crate::Emacs::new(keybindings)));

        drive(&mut rl, &[ch('j')]);
        assert_eq!(rl.editor.get_buffer(), "", "the binding is a prefix");
        expire_pending_sequence(&mut rl);

        assert_eq!(rl.editor.get_buffer(), "J");
    }

    #[test]
    fn auto_pairs_disabled_keeps_literal_typing() {
        let mut rl = Reedline::create();
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);

        assert_eq!(rl.editor.get_buffer(), "(");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn auto_pairs_ignore_unconfigured_openers() {
        let mut rl = auto_pair_engine(&[('[', ']')]);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);

        assert_eq!(rl.editor.get_buffer(), "(");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn auto_pairs_insert_pair_and_continue_inside() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);

        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);

        rl.run_edit_commands(&[EditCommand::InsertChar('a')]);

        assert_eq!(rl.editor.get_buffer(), "(a)");
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    #[test]
    fn auto_pairs_skip_existing_closer() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.run_edit_commands(&[EditCommand::InsertChar('('), EditCommand::InsertChar(')')]);

        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    #[test]
    fn auto_pairs_backspace_removes_empty_pair_as_one_undo_step() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);

        rl.run_edit_commands(&[EditCommand::Backspace]);

        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.insertion_point(), 0);

        rl.run_edit_commands(&[EditCommand::Undo]);

        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn auto_pairs_backspace_after_buffer_replacement_removes_empty_pair() {
        let mut rl = auto_pair_engine(&[('(', ')')]);

        // A history/menu replacement bypasses the key event that originally
        // created the pair. Backspace must still inspect the live buffer and
        // remove the empty pair as one operation.
        rl.run_edit_commands(&[
            EditCommand::InsertString("stale input".into()),
            EditCommand::Clear,
            EditCommand::InsertString("()".into()),
            EditCommand::MoveLeft { select: false },
        ]);
        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);

        rl.run_edit_commands(&[EditCommand::Backspace]);

        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.insertion_point(), 0);
    }

    #[test]
    fn auto_pairs_backspace_after_pair_newline_keeps_closer() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);
        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);

        // The newline separates the pair, so Backspace must remove only the
        // newline rather than treating the surrounding characters as an empty
        // pair.
        rl.run_edit_commands(&[EditCommand::InsertNewline]);
        assert_eq!(rl.editor.get_buffer(), "(\n)");
        assert_eq!(rl.editor.insertion_point(), 2);

        rl.run_edit_commands(&[EditCommand::Backspace]);

        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn auto_pairs_wrap_selection_with_opener() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.run_edit_commands(&[
            EditCommand::InsertString("abc".into()),
            EditCommand::MoveToStart { select: false },
            EditCommand::MoveRight { select: true },
            EditCommand::MoveRight { select: true },
            EditCommand::MoveRight { select: true },
            EditCommand::InsertChar('('),
        ]);

        assert_eq!(rl.editor.get_buffer(), "(abc)");
        assert_eq!(rl.editor.get_selection(), None);
        assert_eq!(rl.editor.insertion_point(), 5);
    }

    #[test]
    fn auto_pairs_do_not_rewrite_insert_string() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.run_edit_commands(&[EditCommand::InsertString("()".into())]);

        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    #[test]
    fn auto_pairs_support_same_character_pairs() {
        let mut rl = auto_pair_engine(&[('"', '"')]);
        rl.run_edit_commands(&[EditCommand::InsertChar('"')]);

        assert_eq!(rl.editor.get_buffer(), "\"\"");
        assert_eq!(rl.editor.insertion_point(), 1);

        rl.run_edit_commands(&[EditCommand::InsertChar('"')]);

        assert_eq!(rl.editor.get_buffer(), "\"\"");
        assert_eq!(rl.editor.insertion_point(), 2);

        rl.run_edit_commands(&[
            EditCommand::MoveLeft { select: false },
            EditCommand::Backspace,
        ]);

        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.insertion_point(), 0);
    }

    // --- `Highlighter::should_auto_pair` veto -------------------------------

    /// Vetoes exactly one [`AutoPairAction`], letting the other two proceed
    /// unmodified — used to prove the three actions are gated independently.
    struct VetoActionHighlighter(AutoPairAction);

    impl Highlighter for VetoActionHighlighter {
        fn highlight(&self, _line: &str, _cursor: usize) -> crate::StyledText {
            crate::StyledText::new()
        }

        fn should_auto_pair(&self, context: &AutoPairContext<'_>) -> bool {
            context.action() != self.0
        }
    }

    fn auto_pair_engine_with_veto(pairs: &[(char, char)], vetoed: AutoPairAction) -> Reedline {
        auto_pair_engine(pairs).with_highlighter(Box::new(VetoActionHighlighter(vetoed)))
    }

    #[test]
    fn auto_pairs_veto_open_falls_back_to_literal_insert_char() {
        let mut rl = auto_pair_engine_with_veto(&[('(', ')')], AutoPairAction::Open);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);

        // Fallback must be exactly `InsertChar('(')`, not `InsertPair`.
        assert_eq!(rl.editor.get_buffer(), "(");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn auto_pairs_veto_open_does_not_affect_skip_over_or_backspace_pair() {
        let mut rl = auto_pair_engine_with_veto(&[('(', ')')], AutoPairAction::Open);

        // Build "(a)" directly (bypassing the vetoed `Open` action) and place
        // the cursor right before the closer.
        rl.run_edit_commands(&[
            EditCommand::InsertString("(a)".into()),
            EditCommand::MoveLeft { select: false },
        ]);
        assert_eq!(rl.editor.insertion_point(), 2);

        // `SkipExistingCloser` is not vetoed here, so it must still fire.
        rl.run_edit_commands(&[EditCommand::InsertChar(')')]);
        assert_eq!(rl.editor.get_buffer(), "(a)");
        assert_eq!(rl.editor.insertion_point(), 3);

        // Rebuild an empty pair the same way and confirm `BackspacePair` still
        // collapses it as one step even though `Open` is vetoed.
        rl.run_edit_commands(&[
            EditCommand::Clear,
            EditCommand::InsertString("()".into()),
            EditCommand::MoveLeft { select: false },
            EditCommand::Backspace,
        ]);
        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.insertion_point(), 0);
    }

    #[test]
    fn auto_pairs_veto_skip_existing_closer_falls_back_to_literal_insert_char() {
        let mut rl = auto_pair_engine_with_veto(&[('(', ')')], AutoPairAction::SkipExistingCloser);

        rl.run_edit_commands(&[
            EditCommand::InsertString("(a)".into()),
            EditCommand::MoveLeft { select: false },
        ]);
        assert_eq!(rl.editor.insertion_point(), 2);

        // Decision order: even though the cursor sits on an existing closer
        // (which would normally win unconditionally), the veto is consulted
        // and a literal `)` is inserted instead of skipping over.
        rl.run_edit_commands(&[EditCommand::InsertChar(')')]);
        assert_eq!(rl.editor.get_buffer(), "(a))");
        assert_eq!(rl.editor.insertion_point(), 3);

        // `Open` is unaffected by this veto.
        rl.run_edit_commands(&[EditCommand::Clear, EditCommand::InsertChar('(')]);
        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn auto_pairs_veto_backspace_pair_falls_back_to_plain_backspace() {
        let mut rl = auto_pair_engine_with_veto(&[('(', ')')], AutoPairAction::BackspacePair);

        // `Open` is not vetoed, so this still produces a real empty pair.
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);
        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);

        // `BackspacePair` is vetoed: fallback is a plain `Backspace`, deleting
        // only the grapheme to the left of the cursor.
        rl.run_edit_commands(&[EditCommand::Backspace]);
        assert_eq!(rl.editor.get_buffer(), ")");
        assert_eq!(rl.editor.insertion_point(), 0);
    }

    #[test]
    fn auto_pairs_veto_open_with_selection_replaces_selection_literally() {
        let mut rl = auto_pair_engine_with_veto(&[('(', ')')], AutoPairAction::Open);
        rl.run_edit_commands(&[
            EditCommand::InsertString("abc".into()),
            EditCommand::MoveToStart { select: false },
            EditCommand::MoveRight { select: true },
            EditCommand::MoveRight { select: true },
            EditCommand::MoveRight { select: true },
            EditCommand::InsertChar('('),
        ]);

        // Contrast with `auto_pairs_wrap_selection_with_opener` (no veto),
        // which produces "(abc)" with the cursor at 5: vetoing `Open` must
        // instead replace the selection with a literal '(', same as ordinary
        // typing over a selection.
        assert_eq!(rl.editor.get_buffer(), "(");
        assert_eq!(rl.editor.get_selection(), None);
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn auto_pairs_veto_sees_buffer_and_cursor_after_preceding_commands_in_batch() {
        // The context passed to `should_auto_pair` must reflect live state —
        // not a stale snapshot taken before other commands in the same batch
        // (e.g. a history navigation or completion insert) ran.
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, usize, AutoPairAction)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        struct RecordingHighlighter {
            seen: std::sync::Arc<std::sync::Mutex<Vec<(String, usize, AutoPairAction)>>>,
        }

        impl Highlighter for RecordingHighlighter {
            fn highlight(&self, _line: &str, _cursor: usize) -> crate::StyledText {
                crate::StyledText::new()
            }

            fn should_auto_pair(&self, context: &AutoPairContext<'_>) -> bool {
                self.seen.lock().unwrap().push((
                    context.buffer().to_string(),
                    context.insertion_point(),
                    context.action(),
                ));
                true
            }
        }

        let mut rl = auto_pair_engine(&[('(', ')')])
            .with_highlighter(Box::new(RecordingHighlighter { seen: seen.clone() }));

        // Replace the whole buffer (as a history navigation or menu accept
        // would) and immediately type an opener in the same batch.
        rl.run_edit_commands(&[
            EditCommand::Clear,
            EditCommand::InsertString("echo hi".into()),
            EditCommand::InsertChar('('),
        ]);

        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "echo hi");
        assert_eq!(recorded[0].1, "echo hi".len());
        assert_eq!(recorded[0].2, AutoPairAction::Open);
    }

    // Regression: `auto_pair_command` used to derive the context's selection
    // by comparing `selection_anchor()` against `insertion_point()` directly.
    // Under a forward vi-normal (block) selection those two disagree with the
    // actual selected range: `insertion_point()` is the *caret*, one grapheme
    // back from the cursor's `head` (see `Editor::insertion_point` /
    // `Cursor::caret`), while `Editor::get_selection()` — the range
    // `insert_pair` (and thus the real `InsertPair` wrap) actually uses —
    // reports `cursor.start()..cursor.end()` with `end()` at the widened
    // `head`. The old code silently dropped the selection's last grapheme
    // from the context it handed to `should_auto_pair`.
    //
    // This mirrors the exact selection shape pinned by
    // `vi_normal_selection_cut_is_inclusive` in `core_editor::editor`'s own
    // tests: from "hello" at position 0, two forward `MoveRight { select:
    // true }` steps land the head on 'l' (byte 2) but widen the selection to
    // byte 3 to cover it.
    #[test]
    fn auto_pairs_context_selection_matches_vi_block_forward_selection() {
        let seen: std::sync::Arc<std::sync::Mutex<Vec<std::ops::Range<usize>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        struct RecordingSelectionHighlighter {
            seen: std::sync::Arc<std::sync::Mutex<Vec<std::ops::Range<usize>>>>,
        }

        impl Highlighter for RecordingSelectionHighlighter {
            fn highlight(&self, _line: &str, _cursor: usize) -> crate::StyledText {
                crate::StyledText::new()
            }

            fn should_auto_pair(&self, context: &AutoPairContext<'_>) -> bool {
                if let Some(selection) = context.selection() {
                    self.seen.lock().unwrap().push(selection);
                }
                true
            }
        }

        // A fixed `EditMode` that always reports Vi-normal, so `run_edit_commands`'s
        // `sync_edit_mode` (which re-adopts `Reedline`'s own edit mode on every
        // call, independent of whatever `Editor::set_edit_mode` was last told)
        // does not flip the block-caret rest policy back to `Bar` behind our
        // back between the selection setup below and the `InsertChar` that
        // exercises `auto_pair_command`.
        struct AlwaysViNormal;
        impl EditMode for AlwaysViNormal {
            fn parse_event(&mut self, _e: ReedlineRawEvent) -> ReedlineEvent {
                ReedlineEvent::None
            }
            fn edit_mode(&self) -> PromptEditMode {
                PromptEditMode::Vi(PromptViMode::Normal)
            }
        }

        let mut rl = auto_pair_engine(&[('(', ')')])
            .with_highlighter(Box::new(RecordingSelectionHighlighter {
                seen: seen.clone(),
            }))
            .with_edit_mode(Box::new(AlwaysViNormal));

        rl.run_edit_commands(&[EditCommand::InsertString("hello".into())]);
        rl.editor.run_edit_command(&EditCommand::MoveToPosition {
            position: 0,
            select: false,
        });
        rl.editor
            .run_edit_command(&EditCommand::MoveRight { select: true });
        rl.editor
            .run_edit_command(&EditCommand::MoveRight { select: true });

        // Sanity-check the premise: a forward block selection whose head sits
        // one grapheme past what `insertion_point()` alone would suggest.
        assert_eq!(rl.editor.insertion_point(), 2);
        let expected_selection = rl.editor.get_selection().expect("selection active");
        assert_eq!(expected_selection, (0, 3));

        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);

        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0],
            expected_selection.0..expected_selection.1,
            "context selection must match Editor::get_selection(), not a range \
             derived from insertion_point()"
        );

        // `insert_pair` wraps the exact same range `get_selection()` reported,
        // so the buffer confirms the context wasn't merely coincidentally
        // correct: 'l' at byte 2 must be inside the pair.
        assert_eq!(rl.editor.get_buffer(), "(hel)lo");
    }

    /// A small context-sensitive policy used to exercise the consumer-facing
    /// `Highlighter` hook. It demonstrates that syntax-aware consumers can
    /// veto opening a pair while leaving the other auto-pair actions alone.
    struct ContextAwareAutoPairHighlighter;

    impl ContextAwareAutoPairHighlighter {
        fn inside_unclosed_quote(buffer: &str, point: usize, quote: char) -> bool {
            let mut in_quote = false;
            let mut escaped = false;
            for ch in buffer[..point].chars() {
                if escaped {
                    escaped = false;
                    continue;
                }
                match ch {
                    '\\' => escaped = true,
                    c if c == quote => in_quote = !in_quote,
                    _ => {}
                }
            }
            in_quote
        }
    }

    impl Highlighter for ContextAwareAutoPairHighlighter {
        fn highlight(&self, _line: &str, _cursor: usize) -> crate::StyledText {
            crate::StyledText::new()
        }

        fn should_auto_pair(&self, context: &AutoPairContext<'_>) -> bool {
            if context.action() != AutoPairAction::Open {
                return true;
            }

            let (open, close) = context.pair();
            let buffer = context.buffer();
            let point = context.insertion_point();

            if open == close && Self::inside_unclosed_quote(buffer, point, open) {
                return false;
            }

            true
        }
    }

    fn context_aware_auto_pair_engine() -> Reedline {
        auto_pair_engine(&[('(', ')'), ('"', '"')])
            .with_highlighter(Box::new(ContextAwareAutoPairHighlighter))
    }

    #[test]
    fn context_policy_allows_non_quote_pair_inside_unclosed_region() {
        let mut rl = context_aware_auto_pair_engine();
        rl.run_edit_commands(&[EditCommand::InsertString("\"abc".into())]);

        // A non-quote pair is allowed even inside an unclosed region.
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);
        assert_eq!(rl.editor.get_buffer(), "\"abc()");
        assert_eq!(rl.editor.insertion_point(), 5);
    }

    #[test]
    fn context_policy_vetoes_same_delimiter_inside_unclosed_region() {
        let mut rl = context_aware_auto_pair_engine();
        rl.run_edit_commands(&[EditCommand::InsertString("\"".into())]);
        assert_eq!(rl.editor.insertion_point(), 1);

        // Typing the same quote again would normally open a new pair (cursor
        // is at the end of the buffer), but we are inside an unclosed string
        // of that same quote kind, so it must close it literally instead.
        rl.run_edit_commands(&[EditCommand::InsertChar('"')]);
        assert_eq!(rl.editor.get_buffer(), "\"\"");
        assert_eq!(rl.editor.insertion_point(), 2, "literal insert advances past the closing quote, unlike a paired insert which would leave the cursor at 1");
    }

    // --- undo granularity across all six action x veto combinations --------

    #[test]
    fn auto_pairs_undo_granularity_open_not_vetoed() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);
        assert_eq!(rl.editor.get_buffer(), "()");

        rl.run_edit_commands(&[EditCommand::Undo]);
        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.insertion_point(), 0);
    }

    #[test]
    fn auto_pairs_undo_granularity_open_vetoed() {
        let mut rl = auto_pair_engine_with_veto(&[('(', ')')], AutoPairAction::Open);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);
        assert_eq!(rl.editor.get_buffer(), "(");

        rl.run_edit_commands(&[EditCommand::Undo]);
        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.insertion_point(), 0);
    }

    #[test]
    fn auto_pairs_undo_granularity_skip_existing_closer_not_vetoed() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);
        rl.run_edit_commands(&[EditCommand::InsertChar(')')]);
        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 2);

        // The skip-over is a plain cursor move (`MoveRight`), and cursor
        // moves never open their own undo boundary in reedline — they merge
        // into whatever edit precedes them (verified directly against plain
        // `InsertChar` + `MoveLeft` with no auto-pairing involved). So one
        // `Undo` here reverts the *pair insertion* too, landing back at the
        // pre-`Open` empty buffer, not at the interim "()" state.
        rl.run_edit_commands(&[EditCommand::Undo]);
        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.insertion_point(), 0);
    }

    #[test]
    fn auto_pairs_undo_granularity_skip_existing_closer_vetoed() {
        let mut rl = auto_pair_engine_with_veto(&[('(', ')')], AutoPairAction::SkipExistingCloser);
        rl.run_edit_commands(&[
            EditCommand::InsertString("(a)".into()),
            EditCommand::MoveLeft { select: false },
        ]);
        rl.run_edit_commands(&[EditCommand::InsertChar(')')]);
        assert_eq!(rl.editor.get_buffer(), "(a))");

        rl.run_edit_commands(&[EditCommand::Undo]);
        assert_eq!(rl.editor.get_buffer(), "(a)");
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    #[test]
    fn auto_pairs_undo_granularity_backspace_pair_not_vetoed() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);
        rl.run_edit_commands(&[EditCommand::Backspace]);
        assert_eq!(rl.editor.get_buffer(), "");

        rl.run_edit_commands(&[EditCommand::Undo]);
        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn auto_pairs_undo_granularity_backspace_pair_vetoed() {
        let mut rl = auto_pair_engine_with_veto(&[('(', ')')], AutoPairAction::BackspacePair);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);
        rl.run_edit_commands(&[EditCommand::Backspace]);
        assert_eq!(rl.editor.get_buffer(), ")");

        rl.run_edit_commands(&[EditCommand::Undo]);
        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    // --- edge cases: same-char pairs, overlaps, multi-line, grapheme -------

    #[test]
    fn auto_pairs_overlapping_pair_definitions_resolve_by_search_order() {
        // 'b' is both a closer (of `a`/`b`) and an opener (of `b`/`c`).
        // Closers are searched before openers, but only the cursor-at-closer
        // check short-circuits to a skip; otherwise the opener check runs.
        let mut rl = auto_pair_engine(&[('a', 'b'), ('b', 'c')]);

        // Not sitting on an existing 'b' closer (buffer is empty), so 'b' is
        // treated as an opener of the second pair.
        rl.run_edit_commands(&[EditCommand::InsertChar('b')]);
        assert_eq!(rl.editor.get_buffer(), "bc");
        assert_eq!(rl.editor.insertion_point(), 1);

        // Now place the cursor right before an existing 'b', which is also
        // configured as the closer of the first pair — skip-over wins.
        rl.run_edit_commands(&[
            EditCommand::Clear,
            EditCommand::InsertString("ab".into()),
            EditCommand::MoveLeft { select: false },
        ]);
        assert_eq!(rl.editor.insertion_point(), 1);

        rl.run_edit_commands(&[EditCommand::InsertChar('b')]);
        assert_eq!(rl.editor.get_buffer(), "ab");
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    #[test]
    fn auto_pairs_multiline_and_crlf_buffer() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.run_edit_commands(&[EditCommand::InsertString("line1\r\nline2".into())]);
        rl.run_edit_commands(&[EditCommand::InsertChar('(')]);

        assert_eq!(rl.editor.get_buffer(), "line1\r\nline2()");
        assert_eq!(rl.editor.insertion_point(), "line1\r\nline2(".len());
    }

    #[test]
    fn auto_pairs_skip_over_grapheme_with_combining_mark_after_closer() {
        // The adjacency check (`grapheme_right().starts_with(close)`) is
        // exercised here with the closer immediately followed by a combining
        // mark, so `close` and the mark form a single grapheme cluster.
        // This pins current behaviour; see written report for whether this
        // is considered correct.
        let mut rl = auto_pair_engine(&[('(', ')')]);
        let combining_acute = '\u{0301}';
        rl.run_edit_commands(&[EditCommand::InsertString(format!("(){combining_acute}"))]);
        // The trailing ')' + combining mark form a single grapheme cluster,
        // so one `MoveLeft` from the end lands right before it.
        rl.run_edit_commands(&[EditCommand::MoveLeft { select: false }]);
        assert_eq!(rl.editor.insertion_point(), 1);

        rl.run_edit_commands(&[EditCommand::InsertChar(')')]);

        // The whole grapheme cluster (closer + combining mark) is skipped
        // over in one motion, landing the cursor at the end of the buffer.
        assert_eq!(rl.editor.get_buffer(), format!("(){combining_acute}"));
        assert_eq!(
            rl.editor.insertion_point(),
            format!("(){combining_acute}").len()
        );
    }

    // --- vi / helix InsertChar replay ---------------------------------------

    #[test]
    fn vi_insert_mode_auto_pair_routes_through_veto() {
        let mut rl = seam_engine(Box::<crate::Vi>::default())
            .with_auto_pairs(AutoPairs::new([('(', ')')]))
            .with_highlighter(Box::new(VetoActionHighlighter(AutoPairAction::Open)));

        type_each(&mut rl, &[ch('(')]);

        assert_eq!(rl.editor.get_buffer(), "(");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn vi_insert_mode_auto_pair_still_pairs_when_not_vetoed() {
        let mut rl =
            seam_engine(Box::<crate::Vi>::default()).with_auto_pairs(AutoPairs::new([('(', ')')]));

        type_each(&mut rl, &[ch('(')]);

        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn helix_insert_mode_auto_pair_routes_through_veto() {
        let mut rl = seam_engine(Box::<crate::Helix>::default())
            .with_auto_pairs(AutoPairs::new([('(', ')')]))
            .with_highlighter(Box::new(VetoActionHighlighter(AutoPairAction::Open)));

        type_each(&mut rl, &[ch('(')]);

        assert_eq!(rl.editor.get_buffer(), "(");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn helix_insert_mode_auto_pair_still_pairs_when_not_vetoed() {
        let mut rl = seam_engine(Box::<crate::Helix>::default())
            .with_auto_pairs(AutoPairs::new([('(', ')')]));

        type_each(&mut rl, &[ch('(')]);

        assert_eq!(rl.editor.get_buffer(), "()");
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    // --- `Event::Paste` must never be rewritten -----------------------------

    #[test]
    fn auto_pairs_balanced_paste_event_is_not_rewritten() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.painter.force_prompt_anchored_for_test(0);
        let prompt = DefaultPrompt::default();
        let _ = rl
            .process_input_batch(&prompt, vec![Event::Paste("(a)".into())])
            .expect("batch ok");

        assert_eq!(rl.editor.get_buffer(), "(a)");
        assert_eq!(rl.editor.insertion_point(), 3);
    }

    #[test]
    fn auto_pairs_unbalanced_paste_event_is_not_rewritten() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.painter.force_prompt_anchored_for_test(0);
        let prompt = DefaultPrompt::default();
        let _ = rl
            .process_input_batch(&prompt, vec![Event::Paste("(a".into())])
            .expect("batch ok");

        // Pasted text is inserted verbatim via `InsertString`, which never
        // goes through `auto_pair_command` — even though the pasted opener
        // has no matching closer.
        assert_eq!(rl.editor.get_buffer(), "(a");
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    #[test]
    fn auto_pairs_do_not_rewrite_reverse_history_search_query() {
        let mut rl = auto_pair_engine(&[('(', ')')]);
        rl.enter_history_search();

        rl.handle_history_search_event(ReedlineEvent::Edit(vec![EditCommand::InsertChar('(')]))
            .expect("history search event handled");

        assert_eq!(rl.input_mode, InputMode::HistorySearch);
        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(
            rl.history_cursor.get_navigation(),
            HistoryNavigationQuery::SubstringSearch("(".into())
        );
    }

    /// An auto-pairing engine with a history menu open in `mode`, over the
    /// buffer `setup` leaves behind.
    fn auto_pair_engine_with_open_menu(mode: crate::InputMode, setup: &[EditCommand]) -> Reedline {
        let mut rl = seam_engine(Box::<crate::Emacs>::default())
            .with_auto_pairs(AutoPairs::new([('(', ')')]))
            .with_menu(ReedlineMenu::HistoryMenu(Box::new(
                ListMenu::default()
                    .with_name("history_menu")
                    .with_input_mode(mode),
            )));
        rl.history
            .save(HistoryItem::from_command_line("(gstat).branch"))
            .expect("history ok");
        rl.run_edit_commands(setup);
        rl.handle_event(
            &DefaultPrompt::default(),
            ReedlineEvent::Menu("history_menu".into()),
        )
        .expect("menu opens");
        assert!(menu_is_active(&rl), "setup: menu is open");
        rl
    }

    // A menu in `Diff` mode searches with what is typed after it opened, so a
    // pair would put the closer in the query (nushell's Ctrl-R history menu).
    // The other modes complete against the real buffer, where a pair is code.
    #[rstest]
    #[case::diff(crate::InputMode::Diff, "(")]
    #[case::cursor_prefix(crate::InputMode::CursorPrefix, "()")]
    #[case::full_buffer(crate::InputMode::FullBuffer, "()")]
    fn auto_pairs_stay_out_of_a_query_menu(#[case] mode: crate::InputMode, #[case] expected: &str) {
        let mut rl = auto_pair_engine_with_open_menu(mode, &[]);

        drive(&mut rl, &[ch('(')]);

        assert_eq!(rl.editor.get_buffer(), expected);
    }

    // Skipping a closer is an auto-pair action too: in the query a typed `)`
    // is text, not a step over the `)` that was there before the menu opened.
    #[test]
    fn auto_pairs_do_not_skip_a_closer_in_a_query_menu() {
        let mut rl = auto_pair_engine_with_open_menu(
            crate::InputMode::Diff,
            &[
                EditCommand::InsertString("()".into()),
                EditCommand::MoveLeft { select: false },
            ],
        );

        drive(&mut rl, &[ch(')')]);

        assert_eq!(rl.editor.get_buffer(), "())");
    }

    #[test]
    fn auto_pairs_resume_after_the_query_menu_closes() {
        let mut rl = auto_pair_engine_with_open_menu(crate::InputMode::Diff, &[]);
        drive(&mut rl, &[ch('(')]);

        drive(&mut rl, &[key(KeyCode::Esc)]);
        assert!(!menu_is_active(&rl), "Esc closes the menu");
        drive(&mut rl, &[ch('(')]);

        assert_eq!(rl.editor.get_buffer(), "(()");
    }

    // FLIP SAFETY NET (Group C) — visual operability at the engine seam.
    // RED until the cursor-as-truth flip: `v` emits Esc which clears the
    // selection, so visual mode starts anchorless and `d` cuts nothing. The
    // flip makes the cursor an always-present range, so `v` then `d` deletes
    // the grapheme under the cursor. Valid under both models, so never wasted.
    #[test]
    fn v_then_d_deletes_cursor_grapheme() {
        let mut rl = seam_engine(Box::<crate::Vi>::default());
        type_each(
            &mut rl,
            &[ch('a'), ch('b'), key(KeyCode::Esc), ch('v'), ch('d')],
        );
        assert_eq!(rl.editor.get_buffer(), "a");
    }

    #[test]
    fn v_extend_left_then_d_deletes_selection() {
        // Visual mode is min-width-1 and motions extend it: from "abc" the cursor
        // rests on 'c'; `v` selects it, `h` grows the selection left over 'b',
        // and `d` deletes both — leaving "a".
        let mut rl = seam_engine(Box::<crate::Vi>::default());
        type_each(
            &mut rl,
            &[
                ch('a'),
                ch('b'),
                ch('c'),
                key(KeyCode::Esc),
                ch('v'),
                ch('h'),
                ch('d'),
            ],
        );
        assert_eq!(rl.editor.get_buffer(), "a");
    }

    struct FlipToNormal {
        switched: bool,
    }
    impl EditMode for FlipToNormal {
        fn parse_event(&mut self, _e: ReedlineRawEvent) -> ReedlineEvent {
            self.switched = true;
            ReedlineEvent::None
        }
        fn edit_mode(&self) -> PromptEditMode {
            if self.switched {
                PromptEditMode::Vi(PromptViMode::Normal)
            }
            // OnGrapheme
            else {
                PromptEditMode::Vi(PromptViMode::Insert)
            }
        }
    }

    #[test]
    fn command_less_mode_transition_settles_cursor() {
        let mut rl = seam_engine(Box::new(FlipToNormal { switched: false }));
        rl.editor
            .set_buffer("ab".into(), UndoBehavior::CreateUndoPoint);
        rl.editor
            .edit_buffer(|b| b.set_insertion_point(2), UndoBehavior::MoveCursor); // at len, legal under Between
        drive(&mut rl, &[ch('x')]); // flipts to OnGrapheme, emits nothing
        assert_eq!(rl.current_insertion_point(), 1);
        assert_eq!(rl.current_selection(), None);
    }

    #[test]
    fn harness_drives_typed_chars_into_buffer() {
        // Smoke test: proves the seam harness runs the real batch pipeline
        // (parse_event -> handle_event -> repaint-to-sink) headlessly.
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(&mut rl, &[ch('h'), ch('i')]);
        assert_eq!(rl.editor.get_buffer(), "hi");
        assert_eq!(rl.current_insertion_point(), 2);
        assert_eq!(rl.current_selection(), None);
    }

    fn kc(modifier: KeyModifiers, key_code: KeyCode) -> crate::KeyCombination {
        crate::KeyCombination { modifier, key_code }
    }

    /// A readline-style macro bound to `Ctrl-X`: replayed keys dispatch
    /// through the keymap the next batch, not literally.
    #[test]
    fn key_sequence_replays_through_the_keymap() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::CONTROL,
            KeyCode::Char('x'),
            ReedlineEvent::KeySequence(vec![
                kc(KeyModifiers::NONE, KeyCode::Char('h')),
                kc(KeyModifiers::NONE, KeyCode::Char('i')),
            ]),
        ));
        // The trigger produces no buffer change itself — the sequence is
        // input queued for the next batch.
        drive(
            &mut rl,
            &[KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)],
        );
        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.pending_input.len(), 2);
        drive(&mut rl, &[]);
        assert_eq!(rl.editor.get_buffer(), "hi");
        assert_eq!(rl.current_insertion_point(), 2);
    }

    /// A replayed key resolves against the map live at replay time:
    /// binding a function key inside a sequence runs the bound event,
    /// not a literal insert.
    #[test]
    fn key_sequence_dispatch_uses_bound_events() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::CONTROL,
            KeyCode::Char('x'),
            // "ab" then MoveToLineStart then 'z' => buffer "zab".
            ReedlineEvent::KeySequence(vec![
                kc(KeyModifiers::NONE, KeyCode::Char('a')),
                kc(KeyModifiers::NONE, KeyCode::Char('b')),
                kc(KeyModifiers::CONTROL, KeyCode::Char('a')),
                kc(KeyModifiers::NONE, KeyCode::Char('z')),
            ]),
        ));
        drive(
            &mut rl,
            &[KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)],
        );
        drive(&mut rl, &[]);
        assert_eq!(rl.editor.get_buffer(), "zab");
        assert_eq!(rl.current_insertion_point(), 1);
    }

    /// A macro may replay a key bound to another KeySequence: the nested
    /// sequence queues behind the remainder of the outer one.
    #[test]
    fn key_sequence_nests() {
        let mut emacs = crate::default_emacs_keybindings();
        emacs.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('y'),
            ReedlineEvent::KeySequence(vec![kc(KeyModifiers::NONE, KeyCode::Char('Y'))]),
        );
        emacs.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('x'),
            ReedlineEvent::KeySequence(vec![
                kc(KeyModifiers::CONTROL, KeyCode::Char('y')),
                kc(KeyModifiers::NONE, KeyCode::Char('X')),
            ]),
        );
        let mut rl = seam_engine(Box::new(crate::Emacs::new(emacs)));
        drive(
            &mut rl,
            &[KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)],
        );
        drive(&mut rl, &[]); // C-y replays -> queues 'Y'; 'X' inserts
        assert_eq!(rl.editor.get_buffer(), "X");
        drive(&mut rl, &[]); // 'Y' replays
        assert_eq!(rl.editor.get_buffer(), "XY");
    }

    /// A sequence ending in Enter submits the line, like a typed Enter.
    #[test]
    fn key_sequence_enter_submits() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::CONTROL,
            KeyCode::Char('x'),
            ReedlineEvent::KeySequence(vec![
                kc(KeyModifiers::NONE, KeyCode::Char('o')),
                kc(KeyModifiers::NONE, KeyCode::Char('k')),
                kc(KeyModifiers::NONE, KeyCode::Enter),
            ]),
        ));
        let prompt = DefaultPrompt::default();
        let _ = rl
            .process_input_batch(
                &prompt,
                vec![Event::Key(KeyEvent::new(
                    KeyCode::Char('x'),
                    KeyModifiers::CONTROL,
                ))],
            )
            .expect("trigger batch ok");
        match rl
            .process_input_batch(&prompt, vec![])
            .expect("replay batch ok")
        {
            ControlFlow::Break(Signal::Success(line)) => assert_eq!(line, "ok"),
            other => panic!("expected line submit, got {other:?}"),
        }
    }

    /// The queue is bounded: a payload over the cap is truncated, and a
    /// retriggering sequence cannot grow the queue past the cap.
    #[test]
    fn key_sequence_queue_is_capped() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        rl.enqueue_replayed_keys(
            vec![kc(KeyModifiers::NONE, KeyCode::Char('a')); Reedline::MAX_REPLAYED_KEYS + 10],
            1,
        );
        assert_eq!(rl.pending_input.len(), Reedline::MAX_REPLAYED_KEYS);
        rl.pending_input.clear();
        // Re-enqueueing on a near-full queue only takes the room left.
        rl.enqueue_replayed_keys(
            vec![kc(KeyModifiers::NONE, KeyCode::Char('b')); Reedline::MAX_REPLAYED_KEYS - 1],
            1,
        );
        rl.enqueue_replayed_keys(vec![kc(KeyModifiers::NONE, KeyCode::Char('c')); 10], 1);
        assert_eq!(rl.pending_input.len(), Reedline::MAX_REPLAYED_KEYS);
    }

    /// C-v quoted-insert: the next key is literal data, not a binding —
    /// including control keys that would otherwise act.
    #[test]
    fn quoted_insert_takes_next_key_literally() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        // C-v C-b inserts the control byte instead of moving left.
        drive(
            &mut rl,
            &[
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            ],
        );
        assert_eq!(rl.editor.get_buffer(), "\u{2}");
        assert_eq!(rl.current_insertion_point(), 1);
        // Only one key is consumed; the next C-a acts normally.
        drive(
            &mut rl,
            &[
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
                ch('z'),
            ],
        );
        assert_eq!(rl.editor.get_buffer(), "z\u{2}x");
    }

    /// M-\ delete-horizontal-space: kills whitespace around the point.
    #[test]
    fn delete_horizontal_space_kills_whitespace_around_point() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(
            &mut rl,
            &[ch('a'), ch(' '), ch(' '), ch(' '), ch('b'), ch('c')],
        );
        // Move back onto the run of spaces, then M-\.
        drive(
            &mut rl,
            &[
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::ALT),
            ],
        );
        assert_eq!(rl.editor.get_buffer(), "abc");
    }

    /// `C-x (`/`C-x )`/`C-x e` — GNU's start/end/call-kbd-macro
    /// (macro.c:246/270/291). The recording holds the keystrokes
    /// between the parens, replayed through the live keymap.
    #[test]
    fn kbd_macro_records_and_replays() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        let cx = ctrl('x');
        drive(&mut rl, &[cx, ch('(')]);
        // Human typing: one key per input batch, so each is dispatched
        // (and recorded) separately. A same-batch run coalesces like
        // GNU's `_rl_insert_typein` — see the typein test below.
        type_each(&mut rl, &[ch('h'), ch('i')]);
        drive(&mut rl, &[cx, ch(')')]);
        // Typed keys both record and act.
        assert_eq!(rl.editor.get_buffer(), "hi");
        // The invoking keystrokes are not part of the recording:
        // start's `(` arrives before the recorder arms, and GNU strips
        // `rl_key_sequence_length` at end (macro.c:276) — which also
        // erases the `C-x` prefix half.
        assert_eq!(
            rl.last_macro,
            vec![
                kc(KeyModifiers::NONE, KeyCode::Char('h')),
                kc(KeyModifiers::NONE, KeyCode::Char('i')),
            ]
        );
        // `C-x e` queues the replay ahead of live input.
        drive(&mut rl, &[cx, ch('e')]);
        assert_eq!(rl.pending_input.len(), 2);
        drive(&mut rl, &[]);
        assert_eq!(rl.editor.get_buffer(), "hihi");
    }

    /// GNU `_rl_insert_typein` (input.c:464): a run of self-insert keys
    /// waiting in the input buffer is inserted by one command, so only
    /// the run's first key reaches `_rl_add_macro_char`
    /// (readline.c:907). The oracle's `C-x ( ab C-x ) C-x e` burst
    /// therefore records just `a` and replays one character — `aba`.
    #[test]
    fn kbd_macro_typein_run_records_only_its_first_key() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        let cx = ctrl('x');
        drive(&mut rl, &[cx, ch('('), ch('a'), ch('b'), cx, ch(')')]);
        // Both characters were inserted...
        assert_eq!(rl.editor.get_buffer(), "ab");
        // ...but the typeahead run dispatched only `a`.
        assert_eq!(
            rl.last_macro,
            vec![kc(KeyModifiers::NONE, KeyCode::Char('a'))]
        );
        drive(&mut rl, &[cx, ch('e')]);
        drive(&mut rl, &[]);
        assert_eq!(rl.editor.get_buffer(), "aba");
        assert_eq!(rl.editor.insertion_point(), 3);
    }

    /// A host wrapper can hand a flushed chord to the engine as one
    /// `Multiple` (niu's `ChordEditMode` parks `C-x` and replays it with
    /// the resolving key). The macro controls inside it must still act,
    /// and the chord's raw keys must be erased from the recording.
    #[test]
    fn kbd_macro_controls_nested_in_multiple_are_applied() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        let prompt = DefaultPrompt::default();
        let cx = kc(KeyModifiers::CONTROL, KeyCode::Char('x'));

        rl.handle_event(
            &prompt,
            ReedlineEvent::Multiple(vec![ReedlineEvent::Pending, ReedlineEvent::StartKbdMacro]),
        )
        .unwrap();
        assert!(rl.recording.is_some(), "nested start arms the recorder");

        // The parse loop records each raw key before the mode resolves
        // it: the body key, then the ending chord's `C-x` and `)`.
        for key in [kc(KeyModifiers::NONE, KeyCode::Char('a')), cx.clone()]
            .into_iter()
            .chain([kc(KeyModifiers::NONE, KeyCode::Char(')'))])
        {
            rl.recording.as_mut().expect("recording").push(key);
        }
        rl.handle_event(
            &prompt,
            ReedlineEvent::Multiple(vec![ReedlineEvent::Pending, ReedlineEvent::EndKbdMacro]),
        )
        .unwrap();
        assert_eq!(
            rl.last_macro,
            vec![kc(KeyModifiers::NONE, KeyCode::Char('a'))]
        );

        // The nested call replays the recording.
        rl.handle_event(
            &prompt,
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Pending,
                ReedlineEvent::CallLastKbdMacro,
            ]),
        )
        .unwrap();
        assert_eq!(rl.pending_input.len(), 1);
    }

    /// A host command only suspends the editor. Keys read in the same
    /// batch are still queued input in GNU (`_rl_abort_internal`,
    /// util.c:99, longjmps without draining the input buffer), so
    /// `C-g X` runs `X` after the host resumes — `abcX`.
    #[test]
    fn host_command_suspension_keeps_the_rest_of_the_batch() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::CONTROL,
            KeyCode::Char('g'),
            ReedlineEvent::ExecuteHostCommand("abort".into()),
        ));
        drive(&mut rl, &[ch('a'), ch('b'), ch('c')]);
        assert_eq!(rl.editor.get_buffer(), "abc");

        // One burst: the abort suspends mid-batch, `X` follows it.
        drive(&mut rl, &[ctrl('g'), ch('X')]);
        assert_eq!(rl.editor.get_buffer(), "abc", "X waits for the resume");
        assert!(rl.suspended_state.is_some());

        // The host resumes with a fresh read_line: the parked `X` runs
        // before any new terminal input.
        let _ = rl.begin_read();
        drive(&mut rl, &[]);
        assert_eq!(rl.editor.get_buffer(), "abcX");
        assert_eq!(rl.editor.insertion_point(), 4);
    }

    /// Recording captures keystrokes, not commands: a bound key like
    /// `C-a` is stored raw and re-dispatches through the keymap on
    /// replay.
    #[test]
    fn kbd_macro_records_bound_commands() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        let cx = ctrl('x');
        drive(
            &mut rl,
            &[
                ch('a'),
                ch('b'),
                cx,
                ch('('),
                ctrl('a'),
                ch('z'),
                cx,
                ch(')'),
            ],
        );
        // 'z' landed after C-a moved to line start.
        assert_eq!(rl.editor.get_buffer(), "zab");
        assert_eq!(
            rl.last_macro,
            vec![
                kc(KeyModifiers::CONTROL, KeyCode::Char('a')),
                kc(KeyModifiers::NONE, KeyCode::Char('z')),
            ]
        );
        drive(&mut rl, &[cx, ch('e')]);
        drive(&mut rl, &[]);
        assert_eq!(rl.editor.get_buffer(), "zzab");
    }

    /// The quoted-insert's consumed key is recorded too (text.c:1051),
    /// so the replay re-arms and re-consumes literally.
    #[test]
    fn kbd_macro_records_quoted_insert_keys() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        let cx = ctrl('x');
        drive(&mut rl, &[cx, ch('('), ctrl('v'), ctrl('b'), cx, ch(')')]);
        assert_eq!(rl.editor.get_buffer(), "\u{2}");
        assert_eq!(
            rl.last_macro,
            vec![
                kc(KeyModifiers::CONTROL, KeyCode::Char('v')),
                kc(KeyModifiers::CONTROL, KeyCode::Char('b')),
            ]
        );
        drive(&mut rl, &[cx, ch('e')]);
        drive(&mut rl, &[]);
        assert_eq!(rl.editor.get_buffer(), "\u{2}\u{2}");
    }

    /// `C-x (` while recording aborts — GNU dings (macro.c:248) — and
    /// the recorder stays armed; the dead `(` stays recorded the way
    /// GNU's does.
    #[test]
    fn kbd_macro_start_during_recording_dings_and_keeps_recording() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        let cx = ctrl('x');
        drive(
            &mut rl,
            &[cx, ch('('), ch('a'), cx, ch('('), ch('b'), cx, ch(')')],
        );
        // The aborted `(` dispatches start-kbd-macro, not self-insert —
        // it inserts nothing, but its keystroke stays recorded.
        assert_eq!(rl.editor.get_buffer(), "ab");
        assert_eq!(
            rl.last_macro,
            vec![
                kc(KeyModifiers::NONE, KeyCode::Char('a')),
                kc(KeyModifiers::NONE, KeyCode::Char('(')),
                kc(KeyModifiers::NONE, KeyCode::Char('b')),
            ]
        );
    }

    /// `C-x e` while recording is refused — "no recursive macros"
    /// (macro.c:296) — and the refused `e` is erased from the
    /// recording along with its `C-x`.
    #[test]
    fn kbd_macro_call_during_recording_is_refused() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        let cx = ctrl('x');
        drive(
            &mut rl,
            &[cx, ch('('), ch('a'), cx, ch('e'), ch('b'), cx, ch(')')],
        );
        assert_eq!(rl.editor.get_buffer(), "ab");
        assert_eq!(
            rl.last_macro,
            vec![
                kc(KeyModifiers::NONE, KeyCode::Char('a')),
                kc(KeyModifiers::NONE, KeyCode::Char('b')),
            ]
        );
        // And nothing replayed mid-recording.
        assert!(rl.pending_input.is_empty());
    }

    /// `C-x e` with nothing recorded dings — GNU aborts on
    /// `current_macro == 0` (macro.c:293).
    #[test]
    fn kbd_macro_call_without_recording_does_nothing() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(&mut rl, &[ctrl('x'), ch('e'), ch('a')]);
        assert!(rl.pending_input.is_empty());
        assert_eq!(rl.editor.get_buffer(), "a");
    }

    /// `M-3 a` — the count reaches the terminating key's self-insert
    /// (misc.c:174 + `_rl_insert_char`).
    #[test]
    fn digit_argument_repeats_self_insert() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(&mut rl, &[alt('3'), ch('a')]);
        assert_eq!(rl.editor.get_buffer(), "aaa");
    }

    /// `M-1 M-2 a` — digits accumulate by tens (misc.c:152).
    #[test]
    fn digit_argument_accumulates() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(&mut rl, &[alt('1'), alt('2'), ch('a')]);
        assert_eq!(rl.editor.get_buffer(), "a".repeat(12));
    }

    /// ANY other key terminates the accumulation and is dispatched
    /// under the count — a bound command repeats the same way.
    #[test]
    fn digit_argument_repeats_bound_commands() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(&mut rl, &[ch('x'), ch('y'), ch('z')]);
        drive(&mut rl, &[alt('2'), ctrl('b')]);
        assert_eq!(rl.current_insertion_point(), 1);
        // The arg is spent: the next key inserts once.
        drive(&mut rl, &[ch('a')]);
        assert_eq!(rl.editor.get_buffer(), "xayz");
    }

    /// `M--` negates (misc.c:159): `rl_arg_sign` reaches the
    /// terminating key's count (readline.c:941).
    #[test]
    fn digit_argument_minus_negates() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(&mut rl, &[alt('-'), alt('3')]);
        assert_eq!(rl.pending_arg.unwrap().count(), -3);
        drive(&mut rl, &[ch('a')]);
        // GNU hands the function the signed count; our repetition
        // consumes the magnitude — direction-aware commands are not
        // expressed yet.
        assert_eq!(rl.editor.get_buffer(), "aaa");
        assert!(rl.pending_arg.is_none());
    }

    /// `M--` alone is `M--1` — GNU's NUM_SAWMINUS upgrade (misc.c:170).
    #[test]
    fn digit_argument_bare_minus_is_minus_one() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(&mut rl, &[alt('-'), ch('x')]);
        assert_eq!(rl.editor.get_buffer(), "x");
    }

    /// `rl_universal_argument` multiplies by four (misc.c:262). GNU
    /// binds `C-u` to unix-line-discard in the emacs map, so the test
    /// wires the event to a spare key.
    #[test]
    fn universal_argument_multiplies_by_four() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::ALT,
            KeyCode::Char('u'),
            ReedlineEvent::UniversalArgument,
        ));
        let cu = alt('u');
        drive(&mut rl, &[cu, ch('a')]);
        assert_eq!(rl.editor.get_buffer(), "aaaa");
        drive(&mut rl, &[cu, cu, ch('b')]);
        assert_eq!(rl.editor.get_buffer(), format!("aaaa{}", "b".repeat(16)));
        // A digit after universal-argument replaces it — GNU keeps
        // rl_explicit_arg unset through `C-u`s (misc.c:152), so
        // `C-u 3` is 3, not 12.
        drive(&mut rl, &[cu, alt('3'), ch('c')]);
        assert_eq!(rl.editor.get_buffer(), format!("aaaa{}ccc", "b".repeat(16)));
    }

    /// The argument survives the `C-x` chord prefix and repeats the
    /// macro call — `M-3 C-x e` runs the recording three times.
    #[test]
    fn digit_argument_repeats_macro_call() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        let cx = ctrl('x');
        drive(&mut rl, &[cx, ch('('), ch('x'), cx, ch(')')]);
        drive(&mut rl, &[alt('3'), cx, ch('e')]);
        assert_eq!(rl.pending_input.len(), 3);
        drive(&mut rl, &[]);
        assert_eq!(rl.editor.get_buffer(), "xxxx");
    }

    /// A count on `C-x )` runs the new macro `count - 1` more times —
    /// the definition counted as the first (macro.c:283). The count
    /// keys are recorded like everything else, so the replayed tail
    /// leaves an argument armed the way GNU's does.
    #[test]
    fn digit_argument_on_macro_end_executes_count_minus_one() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        let cx = ctrl('x');
        // Record [z, M-2] and end with arg 2: replay once more.
        drive(&mut rl, &[cx, ch('('), ch('z'), alt('2'), cx, ch(')')]);
        assert_eq!(rl.editor.get_buffer(), "z");
        assert_eq!(rl.pending_input.len(), 2);
        drive(&mut rl, &[]);
        // Replay: z inserts, M-2 arms arg 2 for the next live key.
        assert_eq!(rl.editor.get_buffer(), "zz");
        drive(&mut rl, &[ch('a')]);
        assert_eq!(rl.editor.get_buffer(), "zzaa");
    }

    /// `M-0` terminates with count 0 — nothing runs.
    #[test]
    fn digit_argument_zero_runs_nothing() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(&mut rl, &[alt('0'), ch('a')]);
        assert_eq!(rl.editor.get_buffer(), "");
    }

    /// Insert key toggles overwrite-mode; typed chars then replace.
    #[test]
    fn overwrite_mode_replaces_graphemes() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(&mut rl, &[ch('a'), ch('b'), ch('c'), ch('d')]);
        drive(
            &mut rl,
            &[
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Insert, KeyModifiers::NONE),
                ch('x'),
                ch('y'),
            ],
        );
        assert_eq!(rl.editor.get_buffer(), "abxy");
        // Toggling off inserts again.
        drive(
            &mut rl,
            &[KeyEvent::new(KeyCode::Insert, KeyModifiers::NONE), ch('z')],
        );
        assert_eq!(rl.editor.get_buffer(), "abxyz");
    }

    /// C-z is bound to `Suspend` (GNU rl_suspend), undo moved to C-_.
    #[test]
    fn ctrl_z_maps_to_suspend() {
        let kb = crate::default_emacs_keybindings();
        assert_eq!(
            kb.find_binding(KeyModifiers::CONTROL, KeyCode::Char('z')),
            Some(ReedlineEvent::Suspend)
        );
        let vi = crate::default_vi_insert_keybindings();
        assert_eq!(
            vi.find_binding(KeyModifiers::CONTROL, KeyCode::Char('z')),
            Some(ReedlineEvent::Suspend)
        );
    }

    #[test]
    fn test_current_selection() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        drive(
            &mut rl,
            &[
                ch('h'),
                ch('i'),
                KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT),
            ],
        );
        assert_eq!(rl.editor.get_buffer(), "hi");
        assert_eq!(rl.current_insertion_point(), 1);
        assert_eq!(rl.current_selection(), Some((1, 2)));
    }

    #[test]
    fn immediately_accept_submits_without_hanging() {
        // Regression: the batch-processing call (which pushes the synthetic
        // Submit and returns the buffer) must run even in immediately_accept
        // mode. When it was gated behind `!immediately_accept`, read_line spun
        // forever instead of submitting.
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        rl.immediately_accept = true;
        rl.run_edit_commands(&[EditCommand::InsertString("hi".into())]);
        let prompt = DefaultPrompt::default();
        match rl.process_input_batch(&prompt, vec![]).expect("batch ok") {
            ControlFlow::Break(Signal::Success(buf)) => assert_eq!(buf, "hi"),
            other => panic!("expected immediate submit, got {other:?}"),
        }
    }

    // Stub paste interceptor: records whether `on_paste` fired and returns a
    // fixed action, so the opt-in interception path can be exercised headlessly.
    #[cfg(feature = "system_clipboard")]
    struct StubInterceptor {
        paste_calls: std::sync::atomic::AtomicUsize,
        action: crate::PasteAction,
        expand: Option<String>,
    }
    #[cfg(feature = "system_clipboard")]
    impl crate::PasteInterceptor for StubInterceptor {
        fn on_paste(&self) -> crate::PasteAction {
            self.paste_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.action.clone()
        }
        fn expand_on_submit(&self, _buffer: &str) -> Option<String> {
            self.expand.clone()
        }
    }

    #[test]
    fn no_paste_hooks_installed_by_default() {
        // The two hooks are strictly opt-in: a freshly built engine carries
        // neither, so the read loop and edit dispatch behave exactly as before.
        let rl = Reedline::create();
        #[cfg(feature = "system_clipboard")]
        assert!(rl.paste_interceptor.is_none());
        assert!(rl.paste_burst.is_none());
    }

    #[cfg(feature = "system_clipboard")]
    #[test]
    fn with_paste_interceptor_installs_and_intercepts() {
        // Installing an interceptor makes a bare `PasteSystem` edit call
        // `on_paste` and insert the returned text instead of reading the OS
        // clipboard.
        let interceptor = Arc::new(StubInterceptor {
            paste_calls: std::sync::atomic::AtomicUsize::new(0),
            action: crate::PasteAction::InsertText("XY".into()),
            expand: None,
        });
        let mut rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_interceptor(interceptor.clone());
        assert!(rl.paste_interceptor.is_some());

        let prompt = DefaultPrompt::default();
        rl.handle_event(&prompt, ReedlineEvent::Edit(vec![EditCommand::PasteSystem]))
            .expect("edit ok");
        assert_eq!(
            interceptor
                .paste_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(rl.editor.get_buffer(), "XY");
    }

    #[cfg(feature = "system_clipboard")]
    #[test]
    fn paste_interceptor_noop_inserts_nothing() {
        // A `Noop` action fires the hook but leaves the buffer untouched.
        let interceptor = Arc::new(StubInterceptor {
            paste_calls: std::sync::atomic::AtomicUsize::new(0),
            action: crate::PasteAction::Noop,
            expand: None,
        });
        let mut rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_interceptor(interceptor.clone());
        let prompt = DefaultPrompt::default();
        rl.handle_event(&prompt, ReedlineEvent::Edit(vec![EditCommand::PasteSystem]))
            .expect("edit ok");
        assert_eq!(
            interceptor
                .paste_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(rl.editor.get_buffer(), "");
    }

    #[cfg(feature = "system_clipboard")]
    #[test]
    fn paste_interceptor_expands_on_submit() {
        // On submit, an installed interceptor may replace the buffer (compact
        // placeholder -> full text); the replacement is what `Signal::Success`
        // returns.
        let interceptor = Arc::new(StubInterceptor {
            paste_calls: std::sync::atomic::AtomicUsize::new(0),
            action: crate::PasteAction::Noop,
            expand: Some("expanded text".into()),
        });
        let mut rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_interceptor(interceptor);
        rl.run_edit_commands(&[EditCommand::InsertString("[ref #1]".into())]);
        let prompt = DefaultPrompt::default();
        match rl.submit_buffer(&prompt).expect("submit ok") {
            EventStatus::Exits(Signal::Success(buf)) => assert_eq!(buf, "expanded text"),
            _ => panic!("expected successful submit with expanded buffer"),
        }
        assert_eq!(rl.editor.get_buffer(), "");
    }

    // Stub burst hook: a fixed oracle whose `enter_is_newline` and
    // `is_burst_active` return configured constants, so both the short-paste
    // Enter-reclassification seam and the full burst-coalescing seam can be
    // exercised without real arrival timing.
    struct StubBurst {
        enter_newline: bool,
        active: bool,
    }
    impl crate::PasteBurstHook for StubBurst {
        fn on_char(&self, _c: char) {}
        fn enter_is_newline(&self) -> bool {
            self.enter_newline
        }
        fn is_burst_active(&self) -> bool {
            self.active
        }
        fn poll_timeout(&self) -> Duration {
            Duration::from_millis(1)
        }
        fn settle(&self) {}
        fn resolve_burst(&self, _coalesced: &str) -> Option<String> {
            None
        }
    }

    #[test]
    fn with_paste_burst_installs_hook() {
        let rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_burst(Arc::new(StubBurst {
                enter_newline: false,
                active: false,
            }));
        assert!(rl.paste_burst.is_some());
    }

    #[test]
    fn paste_burst_reclassifies_enter_as_newline() {
        // With a burst hook whose oracle says a bare Enter is paste-embedded,
        // the Enter inserts a newline instead of submitting the line.
        let mut rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_burst(Arc::new(StubBurst {
                enter_newline: true,
                active: false,
            }));
        rl.run_edit_commands(&[EditCommand::InsertString("ab".into())]);
        drive(&mut rl, &[key(KeyCode::Enter)]);
        assert_eq!(rl.editor.get_buffer(), "ab\n");
    }

    #[test]
    fn paste_burst_embedded_enters_coalesce_without_submit() {
        // In the coalescing path, when the oracle judges every Enter to be
        // paste-embedded, a multi-line burst folds into one insertion with its
        // newlines preserved and does NOT submit.
        let mut rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_burst(Arc::new(StubBurst {
                enter_newline: true,
                active: true,
            }));
        let prompt = DefaultPrompt::default();
        let result = rl
            .process_input_batch(
                &prompt,
                vec![
                    Event::Key(ch('a')),
                    Event::Key(key(KeyCode::Enter)),
                    Event::Key(ch('b')),
                ],
            )
            .expect("batch ok");
        assert!(matches!(result, ControlFlow::Continue(())));
        assert_eq!(rl.editor.get_buffer(), "a\nb");
    }

    #[test]
    fn paste_burst_enter_coalesces_as_newline_without_submit() {
        // An Enter drained into an active burst batch is always coalesced as
        // an embedded newline, never a submit: a detected burst never
        // consults the oracle for its Enters (see `enter_is_newline`'s docs),
        // it treats every one of them as paste-embedded. The coalescing path
        // inserts the pasted chars with the Enter folded in as `\n`, and the
        // line stays unsubmitted.
        let mut rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_burst(Arc::new(StubBurst {
                enter_newline: true,
                active: true,
            }));
        let prompt = DefaultPrompt::default();
        let result = rl
            .process_input_batch(
                &prompt,
                vec![
                    Event::Key(ch('h')),
                    Event::Key(ch('i')),
                    Event::Key(key(KeyCode::Enter)),
                ],
            )
            .expect("batch ok");
        assert!(matches!(result, ControlFlow::Continue(())));
        assert_eq!(rl.editor.get_buffer(), "hi\n");
    }

    #[test]
    fn paste_burst_batch_keeps_a_resize() {
        // The burst drain pushes every event it reads, so a resize arriving
        // mid-paste lands in the burst batch. It must reach the painter like
        // it does on the ordinary path, not be dropped as a paste artifact.
        let mut rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_burst(Arc::new(StubBurst {
                enter_newline: true,
                active: true,
            }));
        let prompt = DefaultPrompt::default();
        let result = rl
            .process_input_batch(
                &prompt,
                vec![
                    Event::Key(ch('a')),
                    Event::Resize(120, 40),
                    Event::Key(ch('b')),
                ],
            )
            .expect("batch ok");
        assert!(matches!(result, ControlFlow::Continue(())));
        assert_eq!(rl.editor.get_buffer(), "ab");
        assert_eq!(
            (rl.painter.screen_width(), rl.painter.screen_height()),
            (120, 40)
        );
    }

    #[test]
    fn paste_burst_settles_only_after_a_burst_batch() {
        // `settle` follows a batch that held a burst and no other: a batch
        // without one leaves the detector's count alone, so an event source
        // that delivers one event per batch can still reach the threshold.
        struct CountingBurst {
            active: bool,
            settles: std::sync::atomic::AtomicUsize,
        }
        impl crate::PasteBurstHook for CountingBurst {
            fn on_char(&self, _c: char) {}
            fn enter_is_newline(&self) -> bool {
                false
            }
            fn is_burst_active(&self) -> bool {
                self.active
            }
            fn poll_timeout(&self) -> Duration {
                Duration::from_millis(1)
            }
            fn settle(&self) {
                self.settles
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            fn resolve_burst(&self, _coalesced: &str) -> Option<String> {
                None
            }
        }
        fn settles_after_one_batch(active: bool) -> usize {
            let hook = Arc::new(CountingBurst {
                active,
                settles: std::sync::atomic::AtomicUsize::new(0),
            });
            let mut rl = seam_engine(Box::<crate::Emacs>::default()).with_paste_burst(hook.clone());
            drive(&mut rl, &[ch('a')]);
            rl.settle_paste_burst();
            assert_eq!(rl.editor.get_buffer(), "a");
            hook.settles.load(std::sync::atomic::Ordering::Relaxed)
        }

        assert_eq!(settles_after_one_batch(false), 0, "batch without a burst");
        assert_eq!(settles_after_one_batch(true), 1, "burst batch");
    }

    #[test]
    fn paste_burst_typed_line_still_submits() {
        // With no `settle` after a batch without a burst, the detector's timing
        // carries into the next batch. A detector that keeps its window below
        // human typing speed, like the `paste_burst` example's, must still let
        // a typed line submit; only an Enter inside the window is a newline.
        // The clock is passed in, and `on_char` is fed by hand, since the read
        // loop that feeds it needs a terminal.
        use crate::PasteBurstHook;

        const GAP: Duration = Duration::from_millis(10);

        #[derive(Default)]
        struct ClockedBurst {
            // (now, when the last char arrived)
            clock: std::sync::Mutex<(Duration, Option<Duration>)>,
        }
        impl ClockedBurst {
            fn advance(&self, by: Duration) {
                self.clock.lock().unwrap().0 += by;
            }
        }
        impl PasteBurstHook for ClockedBurst {
            fn on_char(&self, _c: char) {
                let mut clock = self.clock.lock().unwrap();
                clock.1 = Some(clock.0);
            }
            fn enter_is_newline(&self) -> bool {
                let (now, last) = *self.clock.lock().unwrap();
                last.is_some_and(|last| now - last < GAP)
            }
            fn is_burst_active(&self) -> bool {
                false
            }
            fn poll_timeout(&self) -> Duration {
                Duration::from_millis(1)
            }
            fn settle(&self) {
                self.clock.lock().unwrap().1 = None;
            }
            fn resolve_burst(&self, _coalesced: &str) -> Option<String> {
                None
            }
        }

        // Type `ls` at 100ms a key, one batch per key, then press Enter `wait`
        // after the last one.
        fn type_ls_then_enter(wait: Duration) -> (Reedline, Option<Signal>) {
            let hook = Arc::new(ClockedBurst::default());
            let mut rl = seam_engine(Box::<crate::Emacs>::default()).with_paste_burst(hook.clone());
            for c in ['l', 's'] {
                hook.advance(Duration::from_millis(100));
                hook.on_char(c);
                drive(&mut rl, &[ch(c)]);
                rl.settle_paste_burst();
            }
            hook.advance(wait);
            let signal = drive_until_signal(&mut rl, &[key(KeyCode::Enter)]);
            (rl, signal)
        }

        let (_, signal) = type_ls_then_enter(Duration::from_millis(100));
        assert!(matches!(signal, Some(Signal::Success(ref line)) if line == "ls"));

        let (rl, signal) = type_ls_then_enter(Duration::from_millis(2));
        assert!(signal.is_none(), "an Enter inside the window inserts");
        assert_eq!(rl.editor.get_buffer(), "ls\n");
    }

    #[test]
    fn paste_burst_batch_keeps_a_tab() {
        // A tab inside a burst batch is pasted text, so it is coalesced into
        // the insert as `\t` rather than dropped as a paste artifact.
        let mut rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_burst(Arc::new(StubBurst {
                enter_newline: true,
                active: true,
            }));
        let prompt = DefaultPrompt::default();
        let result = rl
            .process_input_batch(
                &prompt,
                vec![
                    Event::Key(ch('a')),
                    Event::Key(key(KeyCode::Tab)),
                    Event::Key(ch('b')),
                ],
            )
            .expect("batch ok");
        assert!(matches!(result, ControlFlow::Continue(())));
        assert_eq!(rl.editor.get_buffer(), "a\tb");
    }

    // ConPTY reports an AltGr char as Ctrl-Alt, and a raw LF arrives as
    // Ctrl-J. Both are pasted text, inside a burst and, for the newline, also
    // outside one when the oracle calls it embedded.
    #[rstest]
    #[case::altgr_in_burst(true, KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT), "a@b")]
    #[case::lf_in_burst(true, KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL), "a\nb")]
    #[case::lf_outside_burst(
        false,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        "a\nb"
    )]
    fn paste_burst_keeps_the_pasted_key(
        #[case] active: bool,
        #[case] pasted: KeyEvent,
        #[case] expected: &str,
    ) {
        let mut rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_burst(Arc::new(StubBurst {
                enter_newline: true,
                active,
            }));
        let prompt = DefaultPrompt::default();
        let result = rl
            .process_input_batch(
                &prompt,
                vec![Event::Key(ch('a')), Event::Key(pasted), Event::Key(ch('b'))],
            )
            .expect("batch ok");
        assert!(matches!(result, ControlFlow::Continue(())));
        assert_eq!(rl.editor.get_buffer(), expected);
    }

    #[test]
    fn immediately_accept_submits_with_a_burst_hook_reporting_active() {
        // Nothing is read in `immediately_accept` mode, so the burst arm has
        // nothing to insert; the synthetic `Submit` must still be pushed, or a
        // hook that reports an active burst leaves `read_line` spinning.
        let mut rl =
            seam_engine(Box::<crate::Emacs>::default()).with_paste_burst(Arc::new(StubBurst {
                enter_newline: true,
                active: true,
            }));
        rl.immediately_accept = true;
        rl.run_edit_commands(&[EditCommand::InsertString("hi".into())]);
        let prompt = DefaultPrompt::default();
        match rl.process_input_batch(&prompt, vec![]).expect("batch ok") {
            ControlFlow::Break(Signal::Success(buf)) => assert_eq!(buf, "hi"),
            other => panic!("expected immediate submit, got {other:?}"),
        }
    }

    #[cfg(feature = "system_clipboard")]
    #[test]
    fn intercepted_paste_reaches_an_open_menu() {
        // The text an interceptor returns is applied as an ordinary edit, so
        // an open completion menu is sent `MenuEvent::Edit` for it the same
        // way it is for a plain `PasteSystem`. The queued event is what the
        // paint cycle reloads the values from (`update_working_details`), so
        // that is where the difference shows: without the event the menu
        // keeps the values it had before the paste.
        fn apply_menu_paint_cycle(rl: &mut Reedline) {
            let Reedline {
                menus,
                editor,
                completer,
                history,
                painter,
                ..
            } = rl;
            for menu in menus.iter_mut().filter(|menu| menu.is_active()) {
                menu.update_working_details(editor, completer.as_mut(), history.as_ref(), painter);
            }
        }
        fn active_menu_values(rl: &Reedline) -> usize {
            rl.menus
                .iter()
                .find(|menu| menu.is_active())
                .expect("menu open")
                .get_values()
                .len()
        }

        let interceptor = Arc::new(StubInterceptor {
            paste_calls: std::sync::atomic::AtomicUsize::new(0),
            action: crate::PasteAction::InsertText("r".into()),
            expand: None,
        });
        let completer = Box::new(DefaultCompleter::new_with_wordlen(
            vec![
                String::from("carpet"),
                String::from("cattle"),
                String::from("dog"),
            ],
            1,
        ));
        let completion_menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("completion_menu"),
        ));
        let mut rl = Reedline::create()
            .with_completer(completer)
            .with_menu(completion_menu)
            .with_paste_interceptor(interceptor);
        let prompt = DefaultPrompt::default();
        rl.run_edit_commands(&[EditCommand::InsertString(String::from("ca"))]);
        rl.handle_event(
            &prompt,
            ReedlineEvent::Menu(String::from("completion_menu")),
        )
        .expect("menu opens");
        apply_menu_paint_cycle(&mut rl);
        assert_eq!(
            active_menu_values(&rl),
            2,
            "\"ca\" matches both carpet and cattle"
        );

        rl.handle_event(&prompt, ReedlineEvent::Edit(vec![EditCommand::PasteSystem]))
            .expect("edit ok");
        assert_eq!(rl.editor.get_buffer(), "car");
        apply_menu_paint_cycle(&mut rl);
        assert_eq!(active_menu_values(&rl), 1, "narrowed to \"carpet\"");
    }

    #[test]
    fn paste_burst_resolve_burst_inserts_placeholder() {
        // When the hook's `resolve_burst` reference-ifies the coalesced burst
        // text, the read loop inserts the placeholder it returned instead of
        // the raw pasted text.
        struct PlaceholderBurst;
        impl crate::PasteBurstHook for PlaceholderBurst {
            fn on_char(&self, _c: char) {}
            fn enter_is_newline(&self) -> bool {
                true
            }
            fn is_burst_active(&self) -> bool {
                true
            }
            fn poll_timeout(&self) -> Duration {
                Duration::from_millis(1)
            }
            fn settle(&self) {}
            fn resolve_burst(&self, _coalesced: &str) -> Option<String> {
                Some("[Pasted text #1 +2 lines]".into())
            }
        }

        let mut rl = seam_engine(Box::<crate::Emacs>::default())
            .with_paste_burst(Arc::new(PlaceholderBurst));
        let prompt = DefaultPrompt::default();
        let result = rl
            .process_input_batch(
                &prompt,
                vec![
                    Event::Key(ch('a')),
                    Event::Key(key(KeyCode::Enter)),
                    Event::Key(ch('b')),
                    Event::Key(key(KeyCode::Enter)),
                    Event::Key(ch('c')),
                ],
            )
            .expect("batch ok");
        assert!(matches!(result, ControlFlow::Continue(())));
        assert_eq!(rl.editor.get_buffer(), "[Pasted text #1 +2 lines]");
    }

    #[test]
    fn no_burst_hook_enter_submits() {
        // Without a burst hook, a bare Enter submits as before — the opt-in
        // no-op default guard.
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        rl.run_edit_commands(&[EditCommand::InsertString("ab".into())]);
        let prompt = DefaultPrompt::default();
        match rl
            .process_input_batch(&prompt, vec![Event::Key(key(KeyCode::Enter))])
            .expect("batch ok")
        {
            ControlFlow::Break(Signal::Success(buf)) => assert_eq!(buf, "ab"),
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn reedline_is_send() {
        // `Reedline` must stay `Send` so it can be moved across threads.
        // The `Send` bound lives on the stored `Box<dyn Completer + Send>`
        // (engine + `ReedlineMenu`), not on the `Completer`/`Menu` traits
        // themselves, so this guards against that bound being dropped.
        fn assert_send<T: Send>() {}
        assert_send::<Reedline>();
    }

    #[test]
    fn test_cursor_position_after_multiline_history_navigation() {
        // Test for https://github.com/nushell/reedline/pull/899
        // Ensure that after navigating to a multiline history entry and then
        // running edit commands, the cursor doesn't jump unexpectedly.
        // The fix prevents set_buffer() from being called unnecessarily,
        // which would reset the insertion point.

        let mut reedline = Reedline::create();

        // Add a multiline entry to history
        let multiline_command = "echo 'line 1'\necho 'line 2'\necho 'line 3'";
        let history_item = HistoryItem::from_command_line(multiline_command);
        reedline
            .history
            .save(history_item)
            .expect("Failed to save history");

        // Navigate to previous history
        reedline.previous_history().expect("history ok");

        // Get the initial insertion point after history navigation
        let initial_insertion_point = reedline.current_insertion_point();

        // The buffer should contain our multiline command
        assert_eq!(reedline.current_buffer_contents(), multiline_command);

        // After the fix, previous_history() positions cursor at end of first line
        // (after move_to_start + move_to_line_end)
        let first_line_end = multiline_command.find('\n').unwrap();
        assert_eq!(initial_insertion_point, first_line_end);

        // Now simulate pressing the right arrow key, which should move cursor right
        // Without the fix, set_buffer() would be called and reset the insertion point,
        // causing the cursor to jump unexpectedly. With the fix, it stays where it is
        // and moves correctly.
        reedline.run_edit_commands(&[EditCommand::MoveRight { select: false }]);

        let after_move_insertion_point = reedline.current_insertion_point();

        // The cursor should have moved right by 1 from where it was
        assert_eq!(after_move_insertion_point, initial_insertion_point + 1);

        // The buffer should still be unchanged
        assert_eq!(reedline.current_buffer_contents(), multiline_command);
    }

    // --- history walk across a recalled multi-line entry (#1109 regression) ---

    /// History holds `older` then `one\ntwo` (newest).
    fn two_entry_history_engine() -> Reedline {
        let mut rl = seam_engine(Box::<Emacs>::default());
        for cmd in ["older", "one\ntwo"] {
            rl.history
                .save(HistoryItem::from_command_line(cmd))
                .expect("save history");
        }
        rl
    }

    #[test]
    fn down_inside_recalled_multiline_entry_keeps_walking_forward() {
        let mut rl = two_entry_history_engine();
        drive(&mut rl, &[key(KeyCode::Up)]);
        assert_eq!(rl.editor.get_buffer(), "one\ntwo", "setup");
        assert_eq!(rl.editor.insertion_point(), 3, "setup: end of line 1");

        drive(&mut rl, &[key(KeyCode::Down)]);
        assert_eq!(rl.editor.get_buffer(), "one\ntwo", "moves to line 2 first");
        assert!(rl.editor.insertion_point() > 3, "setup: on line 2");

        drive(&mut rl, &[key(KeyCode::Down)]);
        assert_eq!(
            rl.editor.get_buffer(),
            "",
            "from the last line, Down walks forward to the empty draft"
        );
    }

    #[test]
    fn up_inside_recalled_multiline_entry_keeps_walking_back() {
        let mut rl = two_entry_history_engine();
        drive(&mut rl, &[key(KeyCode::Up), key(KeyCode::Down)]);
        assert_eq!(rl.editor.get_buffer(), "one\ntwo", "setup");
        assert!(rl.editor.insertion_point() > 3, "setup: on line 2");

        drive(&mut rl, &[key(KeyCode::Up)]);
        assert_eq!(rl.editor.get_buffer(), "one\ntwo", "moves to line 1 first");
        assert!(rl.editor.insertion_point() <= 3, "setup: on line 1");

        drive(&mut rl, &[key(KeyCode::Up)]);
        assert_eq!(
            rl.editor.get_buffer(),
            "older",
            "from the first line, Up walks back to the older entry"
        );
    }

    // --- Up and Down report what they did ---

    /// What an event reports when the engine handles it.
    fn status_of(rl: &mut Reedline, event: ReedlineEvent) -> EventStatus {
        rl.handle_editor_event(&DefaultPrompt::default(), event)
            .expect("event ok")
    }

    /// On the line being typed, with nothing newer below it, Down has nowhere
    /// to go. It says so, and a binding built on `UntilFound` gets to try its
    /// next event — the way `Left` and `Right` already report at the edges.
    #[test]
    fn down_on_the_line_being_typed_is_inapplicable() {
        let mut rl = two_entry_history_engine();
        rl.run_edit_commands(&[EditCommand::InsertString("typed".into())]);
        assert!(matches!(
            status_of(&mut rl, ReedlineEvent::Down),
            EventStatus::Inapplicable
        ));
        assert_eq!(rl.editor.get_buffer(), "typed", "and nothing moved");
    }

    /// With history to walk, Down is handled: forward an entry, then to the
    /// draft, and only then inapplicable — and it stays so.
    #[test]
    fn down_walks_forward_through_history_and_is_handled_until_it_cannot() {
        let mut rl = two_entry_history_engine();
        drive(&mut rl, &[key(KeyCode::Up), key(KeyCode::Up)]);
        assert_eq!(rl.editor.get_buffer(), "older", "setup");
        assert!(matches!(
            status_of(&mut rl, ReedlineEvent::Down),
            EventStatus::Handled
        ));
        assert_eq!(rl.editor.get_buffer(), "one\ntwo", "forward one entry");
        assert!(
            matches!(
                status_of(&mut rl, ReedlineEvent::Down),
                EventStatus::Handled
            ),
            "to the draft"
        );
        assert_eq!(rl.editor.get_buffer(), "");
        assert!(
            matches!(
                status_of(&mut rl, ReedlineEvent::Down),
                EventStatus::Inapplicable
            ),
            "nothing below the draft"
        );
        assert!(
            matches!(
                status_of(&mut rl, ReedlineEvent::Down),
                EventStatus::Inapplicable
            ),
            "and it stays that way"
        );
    }

    /// Inside a multi-line buffer, Down is handled while it moves the cursor:
    /// down a line, then to the end of the last one. Only then, with the
    /// cursor at the end of the line being typed, has it nowhere to go.
    #[test]
    fn down_inside_a_multiline_buffer_is_handled_while_the_cursor_moves() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        rl.run_edit_commands(&[
            EditCommand::InsertString("one\ntwo".into()),
            EditCommand::MoveToStart { select: false },
        ]);
        assert!(
            matches!(
                status_of(&mut rl, ReedlineEvent::Down),
                EventStatus::Handled
            ),
            "to line 2"
        );
        assert!(
            matches!(
                status_of(&mut rl, ReedlineEvent::Down),
                EventStatus::Handled
            ),
            "to its end"
        );
        assert_eq!(rl.editor.insertion_point(), 7);
        assert!(matches!(
            status_of(&mut rl, ReedlineEvent::Down),
            EventStatus::Inapplicable
        ));
    }

    /// Up mirrors Down: handled while there is an older entry, inapplicable
    /// at the oldest, and with no history at all.
    #[test]
    fn up_is_inapplicable_once_history_is_exhausted() {
        let mut rl = two_entry_history_engine();
        assert!(matches!(
            status_of(&mut rl, ReedlineEvent::Up),
            EventStatus::Handled
        ));
        assert_eq!(rl.editor.get_buffer(), "one\ntwo", "the newest entry");
        assert!(
            matches!(status_of(&mut rl, ReedlineEvent::Up), EventStatus::Handled),
            "to the oldest"
        );
        assert_eq!(rl.editor.get_buffer(), "older");
        assert!(
            matches!(
                status_of(&mut rl, ReedlineEvent::Up),
                EventStatus::Inapplicable
            ),
            "nothing older"
        );
        assert_eq!(rl.editor.get_buffer(), "older", "and nothing moved");

        let mut empty = seam_engine(Box::<Emacs>::default());
        assert!(
            matches!(
                status_of(&mut empty, ReedlineEvent::Up),
                EventStatus::Inapplicable
            ),
            "no history at all"
        );
    }

    /// The report is what `UntilFound` consults: Down on the line being typed
    /// falls through to the next event in the binding.
    #[test]
    fn until_found_falls_through_down_on_the_line_being_typed() {
        let mut rl = two_entry_history_engine();
        rl.run_edit_commands(&[EditCommand::InsertString("typed".into())]);
        let status = rl
            .handle_editor_event(
                &DefaultPrompt::default(),
                ReedlineEvent::UntilFound(vec![
                    ReedlineEvent::Down,
                    ReedlineEvent::Edit(vec![EditCommand::InsertString("!".into())]),
                ]),
            )
            .expect("event ok");
        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(rl.editor.get_buffer(), "typed!", "the second event ran");
    }

    // --- a history that refuses to save ---

    struct RefusingHistory;

    impl History for RefusingHistory {
        fn save(&mut self, _h: HistoryItem) -> crate::Result<HistoryItem> {
            Err(ReedlineError(ReedlineErrorVariants::OtherHistoryError(
                "refused",
            )))
        }
        fn load(&self, _id: HistoryItemId) -> crate::Result<HistoryItem> {
            unreachable!("not used")
        }
        fn count(&self, _query: SearchQuery) -> crate::Result<i64> {
            Ok(0)
        }
        fn search(&self, _query: SearchQuery) -> crate::Result<Vec<HistoryItem>> {
            Ok(vec![])
        }
        fn update(
            &mut self,
            _id: HistoryItemId,
            _updater: &dyn Fn(HistoryItem) -> HistoryItem,
        ) -> crate::Result<()> {
            unreachable!("not used")
        }
        fn clear(&mut self) -> crate::Result<()> {
            Ok(())
        }
        fn delete(&mut self, _h: HistoryItemId) -> crate::Result<()> {
            Ok(())
        }
        fn sync(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn session(&self) -> Option<HistorySessionId> {
            None
        }
    }

    fn refusing_history_engine() -> Reedline {
        let mut rl = seam_engine(Box::<Emacs>::default()).with_history(Box::new(RefusingHistory));
        rl.painter.force_prompt_anchored_for_test(0);
        rl
    }

    #[test]
    fn failed_history_save_still_returns_the_line_and_stashes_the_error() {
        let mut rl = refusing_history_engine();
        let signal = drive_until_signal(&mut rl, &[ch('l'), ch('s'), key(KeyCode::Enter)]);
        assert!(
            matches!(signal, Some(Signal::Success(ref s)) if s == "ls"),
            "got {signal:?}"
        );
        let err = rl.take_history_save_error();
        assert!(err.is_some(), "the save error is stashed");
        assert!(rl.take_history_save_error().is_none(), "cleared on read");
    }

    #[test]
    fn failed_history_save_keeps_the_entry_reachable() {
        let mut rl = refusing_history_engine();
        drive_until_signal(&mut rl, &[ch('l'), ch('s'), key(KeyCode::Enter)]);

        drive(&mut rl, &[key(KeyCode::Up)]);
        assert_eq!(rl.editor.get_buffer(), "ls", "Up recalls the unsaved entry");

        rl.update_last_command_context(&|mut item| {
            item.exit_status = Some(7);
            item
        })
        .expect("context update works off the store");
        assert!(matches!(
            rl.history_last_run,
            HistoryLastRun::Excluded {
                item: HistoryItem {
                    exit_status: Some(7),
                    ..
                },
                ..
            }
        ));
    }

    // nushell#19188 for an excluded line: the next read drops its context
    // but keeps the item, since Up still recalls it.
    #[test]
    fn a_read_after_an_excluded_submit_keeps_only_the_recall() {
        let mut rl = seam_engine(Box::new(crate::Emacs::default()))
            .with_history_exclusion_prefix(Some(" ".into()));
        drive_until_signal(&mut rl, &[ch(' '), ch('l'), ch('s'), key(KeyCode::Enter)]);
        assert!(rl.has_last_command_context(), "the submit sets the context");

        rl.begin_read();
        drive_until_signal(
            &mut rl,
            &[KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)],
        );
        assert!(!rl.has_last_command_context());
        assert!(rl.update_last_command_context(&|item| item).is_err());

        drive(&mut rl, &[key(KeyCode::Up)]);
        assert_eq!(rl.editor.get_buffer(), " ls", "Up still recalls it");
    }

    // nushell#19188: a `read_line` that ends without a submit must not leave
    // the previous call's context behind, or the host writes its metadata
    // onto the older entry.
    #[rstest]
    #[case::host_command(&[KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT)], "HostCommand")]
    #[case::ctrl_c(&[KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)], "CtrlC")]
    #[case::empty_enter(&[key(KeyCode::Enter)], "Success")]
    fn a_read_without_a_submit_has_no_command_context(
        #[case] keys: &[KeyEvent],
        #[case] expected: &str,
    ) {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::ALT,
            KeyCode::Char('h'),
            ReedlineEvent::ExecuteHostCommand("cmd".into()),
        ));
        drive_until_signal(&mut rl, &[ch('l'), ch('s'), key(KeyCode::Enter)]);
        assert!(rl.has_last_command_context(), "the submit sets the context");

        rl.begin_read();
        let signal = drive_until_signal(&mut rl, keys).expect("the read exits");
        let name = match signal {
            Signal::HostCommand(_) => "HostCommand",
            Signal::CtrlC => "CtrlC",
            Signal::Success(ref s) if s.is_empty() => "Success",
            ref other => panic!("unexpected {other:?}"),
        };
        assert_eq!(name, expected);
        assert!(!rl.has_last_command_context());
    }

    // The painter resumes from a host command's suspended state on the next
    // read only, so `begin_read` hands it over once.
    #[test]
    fn begin_read_hands_over_the_suspended_state_once() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::ALT,
            KeyCode::Char('h'),
            ReedlineEvent::ExecuteHostCommand("cmd".into()),
        ));
        assert!(rl.begin_read().is_none(), "nothing suspended yet");

        let signal = drive_until_signal(
            &mut rl,
            &[KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT)],
        );
        assert!(matches!(signal, Some(Signal::HostCommand(_))));
        assert!(rl.begin_read().is_some(), "the host command suspended");
        assert!(rl.begin_read().is_none(), "a second read starts fresh");
    }

    #[test]
    fn thread_safe() {
        fn f<S: Send>(_: S) {}
        f(Reedline::create());
    }

    #[test]
    fn thread_safe_with_idle_callback() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        fn f<S: Send>(_: S) {}

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let reedline = Reedline::create()
            .with_poll_interval(Duration::from_millis(100))
            .with_idle_callback(Box::new(move || {
                counter_clone.fetch_add(1, Ordering::SeqCst);
            }));

        // Verify that Reedline with idle_callback is still Send
        f(reedline);
    }

    #[test]
    fn idle_callback_builder_pattern() {
        // Test that with_idle_callback can be chained with other builder methods
        let _reedline = Reedline::create()
            .with_quick_completions(true)
            .with_poll_interval(Duration::from_millis(33))
            .with_idle_callback(Box::new(|| {}))
            .with_partial_completions(true);
    }

    #[test]
    fn mouse_click_moves_cursor_in_regular_mode() {
        let mut reedline = Reedline::create().with_mouse_click(MouseClickMode::Enabled);
        let prompt = DefaultPrompt::default();

        reedline
            .editor
            .set_buffer("hello".to_string(), UndoBehavior::CreateUndoPoint);
        reedline
            .editor
            .edit_buffer(|buf| buf.set_insertion_point(5), UndoBehavior::MoveCursor);

        reedline.last_render_snapshot = Some(RenderSnapshot {
            screen_width: 20,
            screen_height: 10,
            prompt_start_row: 0,
            prompt_height: 1,
            large_buffer: false,
            prompt_str_left: "".to_string(),
            prompt_indicator: "".to_string(),
            before_cursor: "hello".to_string(),
            after_cursor: "".to_string(),
            first_buffer_col: 0,
            menu_active: false,
            menu_start_row: None,
            large_buffer_extra_rows_after_prompt: None,
            large_buffer_offset: None,
            right_prompt: None,
        });

        let result = reedline.handle_event(
            &prompt,
            ReedlineEvent::Mouse {
                column: 0,
                row: 0,
                button: MouseButton::Left,
            },
        );

        assert!(matches!(result, Ok(EventStatus::Handled)));
        assert_eq!(reedline.current_insertion_point(), 0);
    }

    #[test]
    fn mouse_click_osc133_sets_semantic_markers() {
        let reedline = Reedline::create().with_mouse_click(MouseClickMode::EnabledWithOsc133);
        let markers = reedline
            .painter
            .semantic_markers()
            .expect("expected semantic markers");

        assert_eq!(
            markers.prompt_start(PromptKind::Primary).as_ref(),
            "\x1b]133;A;k=i;click_events=1\x1b\\"
        );
    }

    /// Drive one key per batch, stopping at the first `Signal`.
    fn drive_until_signal(rl: &mut Reedline, keys: &[KeyEvent]) -> Option<Signal> {
        let prompt = DefaultPrompt::default();
        for k in keys {
            match rl
                .process_input_batch(&prompt, vec![Event::Key(*k)])
                .expect("batch ok")
            {
                ControlFlow::Break(signal) => return Some(signal),
                ControlFlow::Continue(()) => {}
            }
        }
        None
    }

    // #1231: Ctrl-C and Ctrl-D exit without a repaint (Enter repaints in
    // `submit_buffer`), so after a clear in the same batch the exit has to
    // show the cursor itself. The re-anchor stands in for the clear, whose
    // `terminal::size()` needs a tty.
    #[rstest]
    #[case::ctrl_c('c', "CtrlC")]
    #[case::ctrl_d('d', "CtrlD")]
    fn exit_before_the_next_paint_shows_the_cursor(#[case] key: char, #[case] expected: &str) {
        let mut rl = Reedline::create();
        rl.painter
            .initialize_prompt_position_with_size((80, 24), None)
            .unwrap();
        assert!(rl.painter.cursor_hidden_for_test());

        let signal = drive_until_signal(&mut rl, &[ctrl(key)]).expect("expected an exit");

        assert_eq!(format!("{signal:?}"), expected);
        assert!(!rl.painter.cursor_hidden_for_test());
    }

    /// `DefaultValidator` reads an unclosed `"` as incomplete, so `Enter` breaks
    /// the line instead of submitting it and leaves the buffer inspectable.
    fn helix_engine_with_validator() -> Reedline {
        let mut rl = Reedline::create()
            .with_edit_mode(Box::<crate::Helix>::default())
            .with_validator(Box::new(crate::DefaultValidator));
        rl.painter.force_prompt_anchored_for_test(0);
        rl
    }

    // --- submitting from helix normal mode ---
    //
    // The resting cursor is a selection and outlives the `next_mode` flip to
    // insert, so anything on the `Enter` path that opens with `delete_selection`
    // eats the covered grapheme. `InsertNewline` does, which makes the incomplete
    // branch the one that can observe it: a submitted buffer is cleared before
    // anything can be asserted about it.

    #[test]
    fn helix_normal_submit_keeps_the_grapheme_under_the_cursor() {
        let mut rl = helix_engine_with_validator();
        let signal = drive_until_signal(
            &mut rl,
            &[
                ch('"'),
                ch('a'),
                ch('b'),
                ch('c'),
                key(KeyCode::Esc),
                key(KeyCode::Enter),
            ],
        );
        assert!(signal.is_none(), "incomplete input must not submit");
        // Not `"ab\n`: the cursor rests *on* the `c`, which is not a selection
        // the break should consume.
        assert_eq!(rl.editor.get_buffer(), "\"abc\n");
    }

    #[test]
    fn helix_normal_submit_breaks_at_the_cursor_not_past_it() {
        let mut rl = helix_engine_with_validator();
        // `hh` walks the caret back onto the `a`.
        let signal = drive_until_signal(
            &mut rl,
            &[
                ch('"'),
                ch('a'),
                ch('b'),
                ch('c'),
                key(KeyCode::Esc),
                ch('h'),
                ch('h'),
                key(KeyCode::Enter),
            ],
        );
        assert!(signal.is_none(), "incomplete input must not submit");
        // The head already sits past the covered `a`, so collapsing forward
        // breaks there. A vi-style `MoveRight` would step one further and give
        // `"ab\nc`; a plain deselect would land before it, at `"\nabc`.
        assert_eq!(rl.editor.get_buffer(), "\"a\nbc");
    }

    /// Helix rests *on* the line terminator under `BlockOverNewline`, which vi
    /// never does, so a break from there is a case vi's handling never answers.
    #[test]
    fn helix_normal_submit_breaks_from_a_terminator() {
        let mut rl = helix_engine_with_validator();
        let signal = drive_until_signal(
            &mut rl,
            &[
                ch('"'),
                ch('a'),
                key(KeyCode::Esc),
                key(KeyCode::Enter),
                key(KeyCode::Esc),
                key(KeyCode::Enter),
            ],
        );
        assert!(signal.is_none(), "incomplete input must not submit");
        assert_eq!(rl.editor.get_buffer(), "\"a\n\n");
    }

    /// A keybinding flip into insert must collapse the resting selection first,
    /// exactly as `i` does. The helix block cursor *is* a one-grapheme
    /// selection, and `insert_char` deletes the selection before inserting, so
    /// without the collapse the first keystroke replaces the covered grapheme.
    #[test]
    fn switch_mode_into_insert_keeps_the_covered_grapheme() {
        let mut bindings = crate::default_helix_normal_keybindings();
        bindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char('z'),
            ReedlineEvent::SwitchMode(PromptEditMode::Helix(PromptHelixMode::Insert)),
        );
        let mut rl = Reedline::create()
            .with_edit_mode(Box::new(
                crate::Helix::default().with_normal_keybindings(bindings),
            ))
            .with_validator(Box::new(crate::DefaultValidator));
        rl.painter.force_prompt_anchored_for_test(0);
        // `hh` walks the caret back onto the `a`, so the cursor covers a
        // grapheme with buffer on both sides of it.
        let signal = drive_until_signal(
            &mut rl,
            &[
                ch('"'),
                ch('a'),
                ch('b'),
                ch('c'),
                key(KeyCode::Esc),
                ch('h'),
                ch('h'),
                ch('z'),
                ch('X'),
            ],
        );
        assert!(signal.is_none(), "incomplete input must not submit");
        // Not `"Xbc`: the covered `a` is a resting cursor, not a selection the
        // insert should consume.
        assert_eq!(rl.editor.get_buffer(), "\"Xabc");
    }

    /// Vi visual rests min-width-1 under `RestPolicy::Block` just as helix does,
    /// so the same rule has to cover a `SwitchMode` flip out of it. Without the
    /// collapse the visual selection is still live and the first keystroke
    /// replaces it. The binding sits in the visual table, the one consulted
    /// once `v` has been pressed.
    #[test]
    fn switch_mode_out_of_vi_visual_keeps_the_covered_grapheme() {
        let mut bindings = crate::default_vi_visual_keybindings();
        bindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char('z'),
            ReedlineEvent::SwitchMode(PromptEditMode::Vi(PromptViMode::Insert)),
        );
        let mut rl = Reedline::create()
            .with_edit_mode(Box::new(crate::Vi::new(
                crate::default_vi_insert_keybindings(),
                crate::default_vi_normal_keybindings(),
                bindings,
            )))
            .with_validator(Box::new(crate::DefaultValidator));
        rl.painter.force_prompt_anchored_for_test(0);
        // `hh` walks back onto the `a`, `v` enters visual covering it.
        let signal = drive_until_signal(
            &mut rl,
            &[
                ch('"'),
                ch('a'),
                ch('b'),
                ch('c'),
                key(KeyCode::Esc),
                ch('h'),
                ch('h'),
                ch('v'),
                ch('z'),
                ch('X'),
            ],
        );
        assert!(signal.is_none(), "incomplete input must not submit");
        assert_eq!(rl.editor.get_buffer(), "\"Xabc");
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Registering a standby machine does not activate it.
    #[test]
    fn additional_edit_mode_stays_inactive_until_switched() {
        let rl = Reedline::create()
            .with_edit_mode(Box::<crate::Vi>::default())
            .with_additional_edit_mode(Box::<crate::Helix>::default());
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Vi(PromptViMode::Insert)
        );
    }

    /// A `SwitchMode` whose target names a standby machine activates it: the
    /// emacs binding fires, and the following keys are read by helix normal,
    /// where `h` is a motion rather than text.
    #[test]
    fn switch_mode_activates_a_standby_machine() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::CONTROL,
            KeyCode::Char('h'),
            ReedlineEvent::SwitchMode(PromptEditMode::Helix(PromptHelixMode::Normal)),
        ))
        .with_additional_edit_mode(Box::<crate::Helix>::default())
        .with_validator(Box::new(crate::DefaultValidator));

        drive_until_signal(&mut rl, &[ch('a'), ch('b'), ch('c'), ctrl('h')]);
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Helix(PromptHelixMode::Normal)
        );
        // The block caret has nothing to the right at the buffer end, so the
        // commit covers the last grapheme `c`. `h` steps onto `b`, `i`
        // collapses in front of it, and `X` lands there.
        drive_until_signal(&mut rl, &[ch('h'), ch('i'), ch('X')]);
        assert_eq!(rl.editor.get_buffer(), "aXbc");
    }

    /// The block-caret repair is stated over the rest policy, so it covers a
    /// switch *between* machines too: leaving helix normal for emacs must not
    /// hand emacs a live one-grapheme selection to overwrite.
    #[test]
    fn switch_mode_between_machines_keeps_the_covered_grapheme() {
        let mut normal = crate::default_helix_normal_keybindings();
        normal.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char('z'),
            ReedlineEvent::SwitchMode(PromptEditMode::Emacs),
        );
        let mut rl = seam_engine(Box::new(
            crate::Helix::default().with_normal_keybindings(normal),
        ))
        .with_additional_edit_mode(Box::<crate::Emacs>::default())
        .with_validator(Box::new(crate::DefaultValidator));

        let signal = drive_until_signal(
            &mut rl,
            &[
                ch('"'),
                ch('a'),
                ch('b'),
                ch('c'),
                key(KeyCode::Esc),
                ch('h'),
                ch('h'),
                ch('z'),
                ch('X'),
            ],
        );
        assert!(signal.is_none(), "incomplete input must not submit");
        assert_eq!(rl.prompt_edit_mode(), PromptEditMode::Emacs);
        assert_eq!(rl.editor.get_buffer(), "\"Xabc");
    }

    /// The target names the state, not just the machine: coming back to vi
    /// lands in the state the binding asked for, not where vi was left.
    #[test]
    fn switch_mode_lands_in_the_named_state() {
        let mut vi_insert = crate::default_vi_insert_keybindings();
        vi_insert.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('h'),
            ReedlineEvent::SwitchMode(PromptEditMode::Helix(PromptHelixMode::Normal)),
        );
        let mut helix_normal = crate::default_helix_normal_keybindings();
        helix_normal.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('v'),
            ReedlineEvent::SwitchMode(PromptEditMode::Vi(PromptViMode::Normal)),
        );
        let mut rl = seam_engine(Box::new(crate::Vi::new(
            vi_insert,
            crate::default_vi_normal_keybindings(),
            crate::default_vi_visual_keybindings(),
        )))
        .with_additional_edit_mode(Box::new(
            crate::Helix::default().with_normal_keybindings(helix_normal),
        ));

        drive_until_signal(&mut rl, &[ch('a'), ctrl('h')]);
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Helix(PromptHelixMode::Normal)
        );
        drive_until_signal(&mut rl, &[ctrl('v')]);
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Vi(PromptViMode::Normal)
        );
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    /// Switching into emacs from any state leaves the cursor where it was: a
    /// bar caret stays put, a block caret collapses onto the grapheme it covers.
    /// `setup` walks the caret from the end of `abcd` back to between `b` and
    /// `c` (or onto `c` for a block) before Alt-e fires from that table.
    #[rstest]
    #[case::vi_insert(&[key(KeyCode::Left), key(KeyCode::Left)], false)]
    #[case::vi_normal(&[key(KeyCode::Esc), ch('h')], false)]
    #[case::vi_visual(&[key(KeyCode::Esc), ch('h'), ch('v')], false)]
    #[case::helix_insert(&[key(KeyCode::Left), key(KeyCode::Left)], true)]
    #[case::helix_normal(&[key(KeyCode::Esc), ch('h')], true)]
    #[case::helix_select(&[key(KeyCode::Esc), ch('h'), ch('v')], true)]
    fn switch_mode_into_emacs_keeps_the_cursor_in_place(
        #[case] setup: &[KeyEvent],
        #[case] helix: bool,
    ) {
        let mut rl = seam_engine(machine_switching_on_alt(helix, 'e', PromptEditMode::Emacs))
            .with_additional_edit_mode(Box::<crate::Emacs>::default())
            .with_validator(Box::new(crate::DefaultValidator));

        drive_until_signal(&mut rl, &[ch('a'), ch('b'), ch('c'), ch('d')]);
        drive_until_signal(&mut rl, setup);
        drive_until_signal(&mut rl, &[alt('e')]);
        assert_eq!(rl.prompt_edit_mode(), PromptEditMode::Emacs);
        assert_eq!(rl.editor.insertion_point(), 2);
        drive_until_signal(&mut rl, &[ch('X')]);
        assert_eq!(rl.editor.get_buffer(), "abXcd");
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    /// The visual table's navigation keys grow the selection they are pressed
    /// in, so the operator that follows takes all of it. Starts from `abcde`
    /// with `v` on `a`.
    #[rstest]
    #[case::right_then_d(&[key(KeyCode::Right), ch('d')], "cde")]
    #[case::end_then_d(&[key(KeyCode::End), ch('d')], "")]
    #[case::right_then_delete(&[key(KeyCode::Right), key(KeyCode::Delete)], "cde")]
    fn vi_visual_navigation_keys_feed_the_operator(
        #[case] keys: &[KeyEvent],
        #[case] left_over: &str,
    ) {
        let mut rl = seam_engine(Box::<crate::Vi>::default())
            .with_validator(Box::new(crate::DefaultValidator));

        drive_until_signal(&mut rl, &[ch('a'), ch('b'), ch('c'), ch('d'), ch('e')]);
        drive_until_signal(&mut rl, &[key(KeyCode::Esc), ch('0'), ch('v')]);
        drive_until_signal(&mut rl, keys);

        assert_eq!(rl.editor.get_buffer(), left_over);
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Vi(PromptViMode::Normal)
        );
    }

    /// `Esc` drops a selection without moving the caret. Under a bar caret the
    /// caret is the head on either side of the anchor; reading a forward head
    /// as a block's far edge would step it back a grapheme.
    #[rstest]
    #[case::selected_leftward(&[shift(KeyCode::Left), shift(KeyCode::Left)], 2)]
    #[case::selected_rightward(
        &[key(KeyCode::Home), key(KeyCode::Right), shift(KeyCode::Right), shift(KeyCode::Right)],
        3
    )]
    fn esc_clears_a_bar_selection_without_moving_the_caret(
        #[case] select: &[KeyEvent],
        #[case] caret: usize,
    ) {
        let mut rl = seam_engine(Box::<crate::Emacs>::default())
            .with_validator(Box::new(crate::DefaultValidator));

        drive_until_signal(&mut rl, &[ch('a'), ch('b'), ch('c'), ch('d'), ch('e')]);
        drive_until_signal(&mut rl, &[key(KeyCode::Left)]);
        drive_until_signal(&mut rl, select);
        assert!(
            rl.editor.get_selection().is_some(),
            "setup: a live selection"
        );
        assert_eq!(rl.editor.insertion_point(), caret);

        drive_until_signal(&mut rl, &[key(KeyCode::Esc)]);
        assert_eq!(rl.editor.get_selection(), None);
        assert_eq!(rl.editor.insertion_point(), caret);
    }

    /// Vi normal has no notion of a selection, so one carried in from a bar
    /// caret must not survive the switch: every operator would read it as its
    /// range and the first keystroke would delete text. Both directions are
    /// covered since the head sits on a different edge in each, and either way
    /// the caret stays where the user left it.
    #[rstest]
    #[case::selected_leftward(&[shift(KeyCode::Left), shift(KeyCode::Left), shift(KeyCode::Left)], 1)]
    #[case::selected_rightward(
        &[key(KeyCode::Home), key(KeyCode::Right), shift(KeyCode::Right), shift(KeyCode::Right)],
        3
    )]
    fn switch_mode_into_vi_normal_drops_a_bar_selection(
        #[case] select: &[KeyEvent],
        #[case] caret: usize,
    ) {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::ALT,
            KeyCode::Char('n'),
            ReedlineEvent::SwitchMode(PromptEditMode::Vi(PromptViMode::Normal)),
        ))
        .with_additional_edit_mode(Box::<crate::Vi>::default())
        .with_validator(Box::new(crate::DefaultValidator));

        drive_until_signal(&mut rl, &[ch('a'), ch('b'), ch('c'), ch('d'), ch('e')]);
        drive_until_signal(&mut rl, &[key(KeyCode::Left)]);
        drive_until_signal(&mut rl, select);
        assert!(
            rl.editor.get_selection().is_some(),
            "setup: a live selection"
        );

        drive_until_signal(&mut rl, &[alt('n')]);
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Vi(PromptViMode::Normal)
        );
        assert_eq!(rl.editor.get_selection(), None);
        assert_eq!(rl.editor.insertion_point(), caret);
        // `x` takes the one grapheme under the caret, not the old selection.
        drive_until_signal(&mut rl, &[ch('x')]);
        assert_eq!(rl.editor.get_buffer().len(), 4);
    }

    /// A selection wider than one grapheme collapses onto the caret, where the
    /// machine's own `Esc` leaves it, rather than onto the selection's start.
    /// `setup` selects `abc` forward from the start of `abcd`, caret on `c`.
    #[rstest]
    #[case::vi_visual_into_vi_normal(
        false,
        &[key(KeyCode::Esc), ch('0'), ch('v'), ch('l'), ch('l')],
        PromptEditMode::Vi(PromptViMode::Normal)
    )]
    #[case::vi_visual_into_emacs(
        false,
        &[key(KeyCode::Esc), ch('0'), ch('v'), ch('l'), ch('l')],
        PromptEditMode::Emacs
    )]
    #[case::helix_select_into_emacs(
        true,
        &[key(KeyCode::Esc), ch('g'), ch('h'), ch('v'), ch('l'), ch('l')],
        PromptEditMode::Emacs
    )]
    #[case::helix_select_into_helix_insert(
        true,
        &[key(KeyCode::Esc), ch('g'), ch('h'), ch('v'), ch('l'), ch('l')],
        PromptEditMode::Helix(PromptHelixMode::Insert)
    )]
    fn switch_mode_collapses_a_wide_selection_onto_the_caret(
        #[case] helix: bool,
        #[case] setup: &[KeyEvent],
        #[case] target: PromptEditMode,
    ) {
        let mut rl = seam_engine(machine_switching_on_alt(helix, 't', target.clone()))
            .with_additional_edit_mode(Box::<crate::Emacs>::default())
            .with_validator(Box::new(crate::DefaultValidator));

        drive_until_signal(&mut rl, &[ch('a'), ch('b'), ch('c'), ch('d')]);
        drive_until_signal(&mut rl, setup);
        assert_eq!(rl.editor.insertion_point(), 2, "setup: caret on `c`");

        drive_until_signal(&mut rl, &[alt('t')]);
        assert_eq!(rl.prompt_edit_mode(), target);
        assert_eq!(rl.editor.get_selection(), None);
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    /// A target naming the state the active machine is already in is not a
    /// move: the engine declines it before any machine is asked, so nothing is
    /// repaired and a live selection survives.
    #[test]
    fn switch_mode_into_the_active_state_is_inapplicable() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default())
            .with_additional_edit_mode(Box::<crate::Vi>::default());
        drive_until_signal(&mut rl, &[ch('a'), ch('b'), ch('c'), ch('d'), ch('e')]);
        drive_until_signal(&mut rl, &[shift(KeyCode::Left), shift(KeyCode::Left)]);
        assert_eq!(
            rl.editor.get_selection(),
            Some((3, 5)),
            "setup: a live selection"
        );

        assert!(matches!(
            status_of(&mut rl, ReedlineEvent::SwitchMode(PromptEditMode::Emacs)),
            EventStatus::Inapplicable
        ));
        assert_eq!(rl.prompt_edit_mode(), PromptEditMode::Emacs);
        assert_eq!(
            rl.editor.get_selection(),
            Some((3, 5)),
            "and nothing was repaired"
        );

        // The same rule inside a machine with several states: only a move
        // between them is handled.
        let mut vi = seam_engine(Box::<crate::Vi>::default());
        drive_until_signal(&mut vi, &[ch('a'), key(KeyCode::Esc)]);
        assert!(matches!(
            status_of(
                &mut vi,
                ReedlineEvent::SwitchMode(PromptEditMode::Vi(PromptViMode::Normal))
            ),
            EventStatus::Inapplicable
        ));
        assert!(matches!(
            status_of(
                &mut vi,
                ReedlineEvent::SwitchMode(PromptEditMode::Vi(PromptViMode::Insert))
            ),
            EventStatus::Handled
        ));
    }

    /// The honest answer is what makes a toggle: with both targets in one
    /// `UntilFound`, the one already active falls through to the other.
    #[test]
    fn until_found_toggles_between_two_switch_targets() {
        let toggle = ReedlineEvent::UntilFound(vec![
            ReedlineEvent::SwitchMode(PromptEditMode::Vi(PromptViMode::Normal)),
            ReedlineEvent::SwitchMode(PromptEditMode::Emacs),
        ]);
        let mut vi_normal = crate::default_vi_normal_keybindings();
        vi_normal.add_binding(KeyModifiers::ALT, KeyCode::Char('t'), toggle.clone());
        let mut rl = seam_engine(emacs_with(KeyModifiers::ALT, KeyCode::Char('t'), toggle))
            .with_additional_edit_mode(Box::new(crate::Vi::new(
                crate::default_vi_insert_keybindings(),
                vi_normal,
                crate::default_vi_visual_keybindings(),
            )));

        drive_until_signal(&mut rl, &[alt('t')]);
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Vi(PromptViMode::Normal)
        );
        drive_until_signal(&mut rl, &[alt('t')]);
        assert_eq!(rl.prompt_edit_mode(), PromptEditMode::Emacs);
        drive_until_signal(&mut rl, &[alt('t')]);
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Vi(PromptViMode::Normal)
        );
    }

    /// A bar selection carried into a block mode keeps the text it covers.
    /// Emacs holds `bc` of `abcde` as the span `(1, 3)` with the caret after
    /// `c`; a block mode reads the same span as `v` on `b` then `l`, caret on
    /// `c`, so the operator that follows takes exactly what was shown. A
    /// backward span already has its caret on a grapheme and moves nothing.
    #[rstest]
    #[case::forward_into_vi_visual(
        &[key(KeyCode::Home), key(KeyCode::Right), shift(KeyCode::Right), shift(KeyCode::Right)],
        PromptEditMode::Vi(PromptViMode::Visual),
        (1, 3),
        2,
        "ade"
    )]
    #[case::forward_into_helix_select(
        &[key(KeyCode::Home), key(KeyCode::Right), shift(KeyCode::Right), shift(KeyCode::Right)],
        PromptEditMode::Helix(PromptHelixMode::Select),
        (1, 3),
        2,
        "ade"
    )]
    #[case::forward_into_helix_normal(
        &[key(KeyCode::Home), key(KeyCode::Right), shift(KeyCode::Right), shift(KeyCode::Right)],
        PromptEditMode::Helix(PromptHelixMode::Normal),
        (1, 3),
        2,
        "ade"
    )]
    #[case::backward_into_vi_visual(
        &[shift(KeyCode::Left), shift(KeyCode::Left)],
        PromptEditMode::Vi(PromptViMode::Visual),
        (3, 5),
        3,
        "abc"
    )]
    #[case::backward_into_helix_select(
        &[shift(KeyCode::Left), shift(KeyCode::Left)],
        PromptEditMode::Helix(PromptHelixMode::Select),
        (3, 5),
        3,
        "abc"
    )]
    #[case::backward_into_helix_normal(
        &[shift(KeyCode::Left), shift(KeyCode::Left)],
        PromptEditMode::Helix(PromptHelixMode::Normal),
        (3, 5),
        3,
        "abc"
    )]
    fn switch_mode_into_a_block_mode_keeps_the_selected_text(
        #[case] select: &[KeyEvent],
        #[case] target: PromptEditMode,
        #[case] selection: (usize, usize),
        #[case] caret: usize,
        #[case] after_d: &str,
    ) {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::ALT,
            KeyCode::Char('b'),
            ReedlineEvent::SwitchMode(target.clone()),
        ))
        .with_additional_edit_mode(Box::<crate::Vi>::default())
        .with_additional_edit_mode(Box::<crate::Helix>::default())
        .with_validator(Box::new(crate::DefaultValidator));

        drive_until_signal(&mut rl, &[ch('a'), ch('b'), ch('c'), ch('d'), ch('e')]);
        drive_until_signal(&mut rl, select);
        assert_eq!(rl.editor.get_selection(), Some(selection), "setup");

        drive_until_signal(&mut rl, &[alt('b')]);
        assert_eq!(rl.prompt_edit_mode(), target);
        assert_eq!(rl.editor.get_selection(), Some(selection));
        assert_eq!(rl.editor.insertion_point(), caret);
        drive_until_signal(&mut rl, &[ch('d')]);
        assert_eq!(rl.editor.get_buffer(), after_d);
    }

    /// The demo's bindings, driven as the keys a terminal sends: a function
    /// key into the active machine changes nothing, one into helix normal
    /// carries the selection over.
    #[test]
    fn function_key_switches_through_the_emacs_parser_keep_an_emacs_selection() {
        let mut emacs = crate::default_emacs_keybindings();
        emacs.add_binding(
            KeyModifiers::NONE,
            KeyCode::F(5),
            ReedlineEvent::SwitchMode(PromptEditMode::Emacs),
        );
        emacs.add_binding(
            KeyModifiers::NONE,
            KeyCode::F(7),
            ReedlineEvent::SwitchMode(PromptEditMode::Helix(PromptHelixMode::Normal)),
        );
        let mut rl = seam_engine(Box::new(crate::Emacs::new(emacs)))
            .with_additional_edit_mode(Box::<crate::Vi>::default())
            .with_additional_edit_mode(Box::<crate::Helix>::default())
            .with_validator(Box::new(crate::DefaultValidator));

        drive_until_signal(&mut rl, &[ch('a'), ch('b'), ch('c'), ch('d'), ch('e')]);
        drive_until_signal(&mut rl, &[shift(KeyCode::Left), shift(KeyCode::Left)]);
        assert_eq!(rl.editor.get_selection(), Some((3, 5)), "setup");

        drive_until_signal(&mut rl, &[key(KeyCode::F(5))]);
        assert_eq!(rl.prompt_edit_mode(), PromptEditMode::Emacs);
        assert_eq!(rl.editor.get_selection(), Some((3, 5)), "F5 in emacs");

        drive_until_signal(&mut rl, &[key(KeyCode::F(7))]);
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Helix(PromptHelixMode::Normal)
        );
        assert_eq!(rl.editor.get_selection(), Some((3, 5)), "F7 into helix");
    }

    /// A switch while the history menu is open neither moves the cursor nor
    /// touches the buffer the menu is querying with.
    #[test]
    fn switch_mode_under_an_open_history_menu_keeps_the_cursor() {
        let mut normal = crate::default_helix_normal_keybindings();
        normal.add_binding(
            KeyModifiers::ALT,
            KeyCode::Char('e'),
            ReedlineEvent::SwitchMode(PromptEditMode::Emacs),
        );
        let mut rl = seam_engine(Box::new(
            crate::Helix::default().with_normal_keybindings(normal),
        ))
        .with_additional_edit_mode(Box::<crate::Emacs>::default())
        .with_menu(ReedlineMenu::HistoryMenu(Box::new(
            crate::ListMenu::default().with_name("history_menu"),
        )));
        rl.history
            .save(HistoryItem::from_command_line("abcd x"))
            .expect("history ok");
        let prompt = DefaultPrompt::default();

        // Caret on `c`, then open the menu over that buffer.
        drive_until_signal(
            &mut rl,
            &[
                ch('a'),
                ch('b'),
                ch('c'),
                ch('d'),
                key(KeyCode::Esc),
                ch('h'),
            ],
        );
        rl.handle_event(&prompt, ReedlineEvent::Menu("history_menu".into()))
            .expect("menu opens");
        assert!(menu_is_active(&rl), "setup: history menu is open");

        drive_until_signal(&mut rl, &[alt('e')]);
        assert_eq!(rl.prompt_edit_mode(), PromptEditMode::Emacs);
        assert_eq!(rl.editor.get_buffer(), "abcd");
        assert_eq!(rl.editor.insertion_point(), 2);
        assert!(menu_is_active(&rl), "the menu outlives the switch");
    }

    /// No registered machine accepts the target: nothing changes, and the
    /// `Inapplicable` lets `UntilFound` fall through to the next candidate.
    #[test]
    fn switch_mode_to_an_unregistered_machine_falls_through() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::CONTROL,
            KeyCode::Char('h'),
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::SwitchMode(PromptEditMode::Helix(PromptHelixMode::Normal)),
                ReedlineEvent::Edit(vec![EditCommand::InsertString("!".into())]),
            ]),
        ));

        drive_until_signal(&mut rl, &[ctrl('h')]);
        assert_eq!(rl.prompt_edit_mode(), PromptEditMode::Emacs);
        assert_eq!(rl.editor.get_buffer(), "!");
    }

    /// `clear_edit_modes` drops the standbys and leaves the active machine, so
    /// a host that rebuilds its set every prompt does not accumulate one.
    #[test]
    fn clear_edit_modes_drops_the_standbys_and_keeps_the_active_machine() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::CONTROL,
            KeyCode::Char('h'),
            ReedlineEvent::SwitchMode(PromptEditMode::Helix(PromptHelixMode::Normal)),
        ))
        .with_additional_edit_mode(Box::<crate::Helix>::default())
        .clear_edit_modes();

        assert!(rl.standby_edit_modes.is_empty());
        drive_until_signal(&mut rl, &[ctrl('h')]);
        assert_eq!(rl.prompt_edit_mode(), PromptEditMode::Emacs);
    }

    /// A host-defined machine is reached through the name it reports, so two
    /// custom machines are told apart by `PromptEditMode::Custom`.
    #[test]
    fn switch_mode_reaches_a_custom_machine_by_name() {
        struct Named(&'static str);
        impl EditMode for Named {
            fn parse_event(&mut self, _e: ReedlineRawEvent) -> ReedlineEvent {
                ReedlineEvent::None
            }
            fn edit_mode(&self) -> PromptEditMode {
                PromptEditMode::Custom(self.0.into())
            }
            fn handle_mode_specific_event(&mut self, event: ReedlineEvent) -> EventStatus {
                match event {
                    ReedlineEvent::SwitchMode(PromptEditMode::Custom(name)) if name == self.0 => {
                        EventStatus::Handled
                    }
                    _ => EventStatus::Inapplicable,
                }
            }
        }
        let prompt = DefaultPrompt::default();
        let mut rl = Reedline::create()
            .with_edit_mode(Box::new(Named("fish")))
            .with_additional_edit_mode(Box::new(Named("shark")));

        let status = rl
            .handle_event(
                &prompt,
                ReedlineEvent::SwitchMode(PromptEditMode::Custom("shark".into())),
            )
            .expect("switching does not touch the terminal");
        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Custom("shark".into())
        );

        let status = rl
            .handle_event(
                &prompt,
                ReedlineEvent::SwitchMode(PromptEditMode::Custom("eel".into())),
            )
            .expect("switching does not touch the terminal");
        assert!(matches!(status, EventStatus::Inapplicable));
        assert_eq!(
            rl.prompt_edit_mode(),
            PromptEditMode::Custom("shark".into())
        );
    }

    /// The submitted path cannot assert on the buffer (`submit_buffer` clears
    /// it), so pin it through the returned signal instead.
    #[test]
    fn helix_normal_submit_returns_the_whole_buffer() {
        let mut rl = Reedline::create().with_edit_mode(Box::<crate::Helix>::default());
        rl.painter.force_prompt_anchored_for_test(0);
        let signal = drive_until_signal(
            &mut rl,
            &[
                ch('a'),
                ch('b'),
                ch('c'),
                key(KeyCode::Esc),
                key(KeyCode::Enter),
            ],
        );
        match signal {
            Some(Signal::Success(buffer)) => assert_eq!(buffer, "abc"),
            other => panic!("expected a submitted buffer, got {other:?}"),
        }
    }

    // --- `j` / `k` ---
    //
    // These lower to `ReedlineEvent::Up`/`Down` rather than a `MotionTarget`,
    // since which of line movement and history traversal applies is decided
    // against the whole buffer, above where a motion resolves.

    /// Two lines, built through the incomplete branch since a bare Enter would
    /// submit. Leaves the caret on the second line, in insert mode.
    fn two_line_helix_engine() -> Reedline {
        let mut rl = helix_engine_with_validator();
        drive_until_signal(
            &mut rl,
            &[
                ch('"'),
                ch('a'),
                ch('b'),
                ch('c'),
                key(KeyCode::Esc),
                key(KeyCode::Enter),
                ch('d'),
                ch('e'),
                ch('f'),
            ],
        );
        // `"abc` is 0..4, the terminator 4, `def` 5..8. Both lines are wide
        // enough for a column-preserving move to differ from a line-start one.
        assert_eq!(rl.editor.get_buffer(), "\"abc\ndef", "setup");
        rl
    }

    #[test]
    fn helix_normal_k_moves_a_line_before_it_reaches_history() {
        let mut rl = two_line_helix_engine();
        drive_until_signal(&mut rl, &[key(KeyCode::Esc), ch('k')]);
        assert!(
            rl.editor.insertion_point() < 4,
            "expected the caret on the first line, got {}",
            rl.editor.insertion_point()
        );
        assert_eq!(
            rl.editor.get_buffer(),
            "\"abc\ndef",
            "history must not load yet"
        );
    }

    #[test]
    fn helix_normal_k_recalls_history_at_the_first_line() {
        let mut rl = seam_engine(Box::<crate::Helix>::default());
        let signal = drive_until_signal(
            &mut rl,
            &[
                ch('o'),
                ch('n'),
                ch('e'),
                key(KeyCode::Esc),
                key(KeyCode::Enter),
            ],
        );
        assert!(
            matches!(signal, Some(Signal::Success(ref b)) if b == "one"),
            "setup: expected a submit, got {signal:?}"
        );
        // The buffer is empty now, so there is no line above to move to.
        drive_until_signal(&mut rl, &[key(KeyCode::Esc), ch('k')]);
        assert_eq!(rl.editor.get_buffer(), "one");
    }

    #[test]
    fn helix_select_extends_with_arrow_keys() {
        // The original report: `v` then arrows moved the caret but dropped the
        // anchor, since the arrows resolved through the mode-blind normal
        // table to `MoveRight { select: false }`.
        let mut rl = helix_engine_with_validator();
        drive_until_signal(
            &mut rl,
            &[
                ch('"'),
                ch('a'),
                ch('b'),
                ch('c'),
                key(KeyCode::Esc),
                ch('g'),
                ch('h'),
                ch('v'),
            ],
        );
        drive_until_signal(&mut rl, &[key(KeyCode::Right), key(KeyCode::Right)]);
        assert_eq!(
            rl.editor.get_selection(),
            Some((0, 3)),
            "arrows must extend like `l` does"
        );
    }

    #[test]
    fn helix_select_j_extends_to_the_column_normal_mode_would_land_on() {
        let mut rl = two_line_helix_engine();
        // `k` from the `f` (column 2) lands on the `b`, also column 2.
        drive_until_signal(&mut rl, &[key(KeyCode::Esc), ch('k')]);
        assert_eq!(rl.editor.insertion_point(), 2, "setup: expected the `b`");

        drive_until_signal(&mut rl, &[ch('v'), ch('j')]);
        assert_eq!(
            rl.editor.get_buffer(),
            "\"abc\ndef",
            "select mode must not traverse history"
        );
        let (start, end) = rl.editor.get_selection().expect("expected a selection");
        // Down from column 2 is the `f` at 7, not the line start at 5: the
        // extension has to stop where a normal-mode `j` would land.
        assert_eq!(
            (start, end),
            (2, 8),
            "expected the selection to reach the `f`, not the line start"
        );
    }

    #[test]
    fn helix_tilde_switches_case_and_keeps_the_selection() {
        let mut rl = seam_engine(Box::<crate::Helix>::default());
        drive_until_signal(&mut rl, &[ch('a'), ch('b'), key(KeyCode::Esc), ch('%')]);
        assert_eq!(rl.editor.get_selection(), Some((0, 2)), "setup");

        drive_until_signal(&mut rl, &[ch('~')]);
        assert_eq!(rl.editor.get_buffer(), "AB");
        // Still selected, so a second `~` acts on the same span.
        assert_eq!(rl.editor.get_selection(), Some((0, 2)));
        drive_until_signal(&mut rl, &[ch('~')]);
        assert_eq!(rl.editor.get_buffer(), "ab");
    }

    #[test]
    fn helix_backtick_lowercases_and_alt_backtick_uppercases() {
        let alt_backtick = KeyEvent::new(KeyCode::Char('`'), KeyModifiers::ALT);

        let mut rl = seam_engine(Box::<crate::Helix>::default());
        drive_until_signal(&mut rl, &[ch('A'), ch('b'), key(KeyCode::Esc), ch('%')]);
        assert_eq!(rl.editor.get_selection(), Some((0, 2)), "setup");

        drive_until_signal(&mut rl, &[ch('`')]);
        assert_eq!(rl.editor.get_buffer(), "ab");
        drive_until_signal(&mut rl, &[alt_backtick]);
        assert_eq!(rl.editor.get_buffer(), "AB");
        // Both keep the span, so they can be applied in sequence.
        assert_eq!(rl.editor.get_selection(), Some((0, 2)));
    }

    // --- `%`, `A`, `I` ---

    #[test]
    fn helix_percent_selects_the_whole_buffer() {
        let mut rl = seam_engine(Box::<crate::Helix>::default());
        drive_until_signal(
            &mut rl,
            &[ch('a'), ch('b'), ch('c'), key(KeyCode::Esc), ch('%')],
        );
        assert_eq!(rl.editor.get_selection(), Some((0, 3)));
    }

    /// `#1190`: `%` and `x` both leave a forward selection wider than one
    /// grapheme, and a backward extend from one used to move the caret two
    /// cells per press.
    #[rstest]
    #[case::select_all_h(ch('%'), ch('h'))]
    #[case::select_all_left(ch('%'), key(KeyCode::Left))]
    #[case::select_line_h(ch('x'), ch('h'))]
    #[case::select_line_left(ch('x'), key(KeyCode::Left))]
    fn helix_select_left_walks_one_cell_after_a_wide_selection(
        #[case] select: KeyEvent,
        #[case] left: KeyEvent,
    ) {
        let mut rl = seam_engine(Box::<crate::Helix>::default());
        drive_until_signal(
            &mut rl,
            &[
                ch('a'),
                ch('b'),
                ch('c'),
                ch('d'),
                key(KeyCode::Esc),
                select,
                ch('v'),
            ],
        );
        assert_eq!(rl.editor.get_selection(), Some((0, 4)));
        assert_eq!(
            rl.editor.insertion_point(),
            3,
            "caret rests on the final `d`"
        );
        for expected in [2, 1, 0] {
            drive_until_signal(&mut rl, &[left]);
            assert_eq!(
                rl.editor.insertion_point(),
                expected,
                "one cell per press, not two"
            );
        }
    }

    /// Appending has to land *past* the last grapheme: the block cursor rests on
    /// it, while insert mode sits between graphemes.
    #[test]
    fn helix_capital_a_appends_past_the_last_grapheme() {
        use crate::PromptHelixMode;

        let mut rl = seam_engine(Box::<crate::Helix>::default());
        drive_until_signal(
            &mut rl,
            &[ch(' '), ch('h'), ch('i'), key(KeyCode::Esc), ch('A')],
        );
        assert_eq!(rl.editor.insertion_point(), 3);
        assert!(matches!(
            rl.prompt_edit_mode(),
            PromptEditMode::Helix(PromptHelixMode::Insert)
        ));
        // Typing lands at the end rather than one grapheme short.
        drive_until_signal(&mut rl, &[ch('!')]);
        assert_eq!(rl.editor.get_buffer(), " hi!");
    }

    /// The leading space is what separates this from a plain line start.
    #[test]
    fn helix_capital_i_inserts_at_the_first_non_blank() {
        let mut rl = seam_engine(Box::<crate::Helix>::default());
        drive_until_signal(
            &mut rl,
            &[ch(' '), ch('h'), ch('i'), key(KeyCode::Esc), ch('I')],
        );
        assert_eq!(rl.editor.insertion_point(), 1);
        drive_until_signal(&mut rl, &[ch('!')]);
        assert_eq!(rl.editor.get_buffer(), " !hi");
    }

    #[test]
    fn with_edit_mode_builder_accepts_custom_helix_mode() {
        use crate::PromptHelixMode;

        let reedline = Reedline::create().with_edit_mode(Box::new(crate::Helix::default()));

        assert!(matches!(
            reedline.prompt_edit_mode(),
            PromptEditMode::Helix(PromptHelixMode::Insert)
        ));
    }

    #[test]
    fn break_signal_builder_pattern() {
        let signal = Arc::new(AtomicBool::new(false));
        let _reedline = Reedline::create()
            .with_quick_completions(true)
            .with_break_signal(signal)
            .with_partial_completions(true);
    }

    #[test]
    fn break_signal_is_send() {
        fn f<S: Send>(_: S) {}
        let signal = Arc::new(AtomicBool::new(false));
        f(Reedline::create().with_break_signal(signal));
    }

    #[test]
    fn take_repaint_request_is_false_without_a_handle() {
        let reedline = Reedline::create();
        assert!(!reedline.take_repaint_request());
        assert!(!reedline.take_repaint_request());
    }

    #[test]
    fn take_repaint_request_consumes_the_request() {
        let mut reedline = Reedline::create();
        let signal = reedline.repaint_signal();

        signal.request_repaint();
        assert!(reedline.take_repaint_request());
        // Consumed: no repaint left pending
        assert!(!reedline.take_repaint_request());

        // A new request is honored again
        signal.request_repaint();
        assert!(reedline.take_repaint_request());
        assert!(!reedline.take_repaint_request());
    }

    #[test]
    fn repaint_signal_switches_input_loop_to_polling() {
        let mut reedline = Reedline::create();
        assert!(
            !reedline.input_needs_polling(),
            "without external triggers the loop should block on input"
        );

        let _signal = reedline.repaint_signal();
        assert!(
            reedline.input_needs_polling(),
            "a repaint handle must switch the loop to polling so requests are noticed"
        );
    }

    #[test]
    fn repaint_request_during_active_read_survives_while_stale_ones_are_dropped() {
        // Emulates the loop's consumption pattern: read_line_helper drains any
        // stale pre-read_line request before painting the initial prompt, so
        // only requests raised afterwards trigger an extra repaint.
        let mut reedline = Reedline::create();
        let signal = reedline.repaint_signal();

        // Raised while no read_line is active -> dropped by the initial drain
        signal.request_repaint();
        reedline.take_repaint_request();
        assert!(!reedline.take_repaint_request());

        // Raised "mid-edit" -> observed by the next loop iteration
        signal.request_repaint();
        assert!(reedline.take_repaint_request());
    }

    #[test]
    fn repaint_signal_handles_share_one_flag() {
        // Every handle returned by `repaint_signal()` (and its clones) must
        // observe the same underlying flag, so a request from any of them is
        // seen exactly once by the loop.
        let mut reedline = Reedline::create();
        let a = reedline.repaint_signal();
        let b = reedline.repaint_signal();
        let c = a.clone();

        b.request_repaint();
        assert!(reedline.take_repaint_request());
        assert!(!reedline.take_repaint_request());

        // A request made through the clone is also observed.
        c.request_repaint();
        assert!(reedline.take_repaint_request());
    }

    #[test]
    fn repaint_signal_survives_behind_an_arc() {
        // This is what a shell would be using...
        // handed to a worker that knows nothing about `Reedline`.
        use std::sync::Arc;
        let mut reedline = Reedline::create();
        let shared: Arc<RepaintSignal> = Arc::new(reedline.repaint_signal());

        let worker = shared.clone();
        std::thread::spawn(move || worker.request_repaint())
            .join()
            .expect("worker thread panicked");

        assert!(reedline.take_repaint_request());
    }

    #[test]
    fn repaint_request_collapses_rapid_fire() {
        // Many requests arriving between two loop iterations must collapse into
        // a single repaint, not N. `take` is a swap(false), so only one take
        // should observe the request regardless of how many were raised.
        let mut reedline = Reedline::create();
        let signal = reedline.repaint_signal();

        for _ in 0..1_000 {
            signal.request_repaint();
        }
        assert!(reedline.take_repaint_request());
        assert!(!reedline.take_repaint_request());
    }

    #[test]
    fn repaint_signal_is_independent_of_break_signal() {
        // The two out-of-band triggers must not interfere: arming a repaint
        // request must not be mistaken for a break, and vice versa.
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let mut reedline = Reedline::create().with_break_signal(Arc::new(AtomicBool::new(false)));
        let repaint = reedline.repaint_signal();

        repaint.request_repaint();
        assert!(reedline.take_repaint_request());
        assert!(
            !reedline
                .break_signal
                .as_ref()
                .unwrap()
                .load(std::sync::atomic::Ordering::Relaxed),
            "repaint must not toggle the break flag"
        );

        reedline
            .break_signal
            .as_ref()
            .unwrap()
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            reedline
                .break_signal
                .as_ref()
                .unwrap()
                .swap(false, std::sync::atomic::Ordering::Relaxed),
            "break flag must be independently observable"
        );
        assert!(
            !reedline.take_repaint_request(),
            "break must not leave a repaint pending"
        );
    }
    #[test]
    fn signal_external_break_pattern_match() {
        let buffer_content = "some partial input".to_string();
        let signal = Signal::ExternalBreak(buffer_content.clone());
        match signal {
            Signal::ExternalBreak(buf) => assert_eq!(buf, buffer_content),
            _ => panic!("Expected Signal::ExternalBreak"),
        }
    }

    fn reedline_with_abbrevs_and_string_lit_override(abbrevs: &[(&str, &str)]) -> Reedline {
        let map = abbrevs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Reedline::create()
            .with_highlighter(Box::new(ExampleHighlighter::default()))
            .with_abbreviations(map)
    }

    fn reedline_with_abbrevs_and_default_string_lit_check(abbrevs: &[(&str, &str)]) -> Reedline {
        let map = abbrevs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Reedline::create()
            .with_highlighter(Box::new(SimpleMatchHighlighter::default()))
            .with_abbreviations(map)
    }

    fn set_buffer_at_end(reedline: &mut Reedline, text: &str) {
        reedline.run_edit_commands(&[
            EditCommand::Clear,
            EditCommand::InsertString(text.to_string()),
        ]);
    }

    #[test]
    fn abbreviation_expands_on_submit() {
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("gc", "git commit")]);
        set_buffer_at_end(&mut reedline, "gc");
        let event = reedline.try_expand_abbreviation_at_cursor(true);
        assert!(event.is_some(), "expected expansion on submit");
        reedline.run_edit_commands(&match event.unwrap() {
            ReedlineEvent::Edit(cmds) => cmds,
            _ => panic!("expected Edit event"),
        });
        assert_eq!(reedline.current_buffer_contents(), "git commit");
    }

    #[test]
    fn abbreviation_expands_on_space_without_double_space() {
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("gc", "git commit")]);
        // When expansion is triggered by <space>, the triggering space has
        // already been inserted into the buffer before the expansion runs.
        set_buffer_at_end(&mut reedline, "gc ");
        let event = reedline.try_expand_abbreviation_at_cursor(false);
        assert!(event.is_some(), "expected expansion on space");
        reedline.run_edit_commands(&match event.unwrap() {
            ReedlineEvent::Edit(cmds) => cmds,
            _ => panic!("expected Edit event"),
        });
        // Exactly one trailing space: the triggering space must be replaced,
        // not left in place alongside the inserted suffix space.
        assert_eq!(reedline.current_buffer_contents(), "git commit ");
    }

    #[test]
    fn abbreviation_no_match_returns_none() {
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("gc", "git commit")]);
        set_buffer_at_end(&mut reedline, "gx");
        assert!(reedline.try_expand_abbreviation_at_cursor(true).is_none());
    }

    #[test]
    fn abbreviation_empty_buffer_returns_none() {
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("gc", "git commit")]);
        assert!(reedline.try_expand_abbreviation_at_cursor(true).is_none());
    }

    #[test]
    fn abbreviation_expands_last_word_only() {
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("gc", "git commit")]);
        set_buffer_at_end(&mut reedline, "sudo gc");
        let event = reedline.try_expand_abbreviation_at_cursor(true);
        assert!(event.is_some());
        reedline.run_edit_commands(&match event.unwrap() {
            ReedlineEvent::Edit(cmds) => cmds,
            _ => panic!("expected Edit event"),
        });
        assert_eq!(reedline.current_buffer_contents(), "sudo git commit");
    }

    #[rstest]
    #[case("\"hello gc", false)]
    #[case("'hello gc", false)]
    #[case("\"hello\" gc", true)]
    #[case("'Сегодня хороший gc", false)]
    #[case("'Сегодня' gc", true)]
    #[case("'今日はいい日だ gc", false)]
    #[case("'🔥🎉 gc", false)]
    fn abbreviation_string_detection_with_override(
        #[case] buffer: &str,
        #[case] should_expand: bool,
    ) {
        let mut reedline = reedline_with_abbrevs_and_string_lit_override(&[("gc", "git commit")]);
        set_buffer_at_end(&mut reedline, buffer);
        assert_eq!(
            reedline.try_expand_abbreviation_at_cursor(true).is_some(),
            should_expand
        );
    }

    #[rstest]
    #[case("\"hello gc")]
    #[case("'hello gc")]
    #[case("\"hello\" gc")]
    #[case("'Сегодня хороший gc")]
    #[case("'Сегодня' gc")]
    #[case("'今日はいい日だ gc")]
    #[case("'🔥🎉 gc")]
    fn abbreviation_string_detection_default(#[case] buffer: &str) {
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("gc", "git commit")]);
        set_buffer_at_end(&mut reedline, buffer);
        assert!(
            reedline.try_expand_abbreviation_at_cursor(true).is_some(),
            "must expand when highlighter does not override should_expand_abbr"
        );
    }

    #[test]
    fn abbreviation_non_ascii_key_and_expansion() {
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("café", "coffee shop")]);
        set_buffer_at_end(&mut reedline, "café");
        let event = reedline.try_expand_abbreviation_at_cursor(true);
        assert!(event.is_some(), "expected expansion for non-ASCII key");
        reedline.run_edit_commands(&match event.unwrap() {
            ReedlineEvent::Edit(cmds) => cmds,
            _ => panic!("expected Edit event"),
        });
        assert_eq!(reedline.current_buffer_contents(), "coffee shop");
    }

    #[test]
    fn try_expand_abbreviation_survives_multibyte_char_before_cursor() {
        // Regression: `word_end` was a raw byte subtraction of `offset` from the
        // byte cursor position. With a multi-byte char (e.g. pasted CJK) right
        // before the cursor, `word_end` could land inside that char, so slicing
        // the buffer panicked with "byte index N is not a char boundary".
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("gc", "git commit")]);
        set_buffer_at_end(&mut reedline, "中");
        // Place the cursor at byte offset 1, inside the 3-byte '中'. Pre-patch,
        // `&buffer[..1]` would panic here.
        let mut line_buffer = LineBuffer::new();
        line_buffer.set_buffer("中".to_string());
        line_buffer.set_insertion_point(1);
        reedline
            .editor
            .set_line_buffer(line_buffer, UndoBehavior::CreateUndoPoint);
        // Must return without panicking (no match, so `None` is expected).
        assert!(reedline.try_expand_abbreviation_at_cursor(true).is_none());
    }

    #[test]
    fn abbreviation_leading_spaces_returns_none() {
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("gc", "git commit")]);
        set_buffer_at_end(&mut reedline, "   ");
        assert!(reedline.try_expand_abbreviation_at_cursor(true).is_none());
    }

    #[test]
    fn abbreviation_mid_word_cursor_on_submit_returns_none() {
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("gc", "git commit")]);
        set_buffer_at_end(&mut reedline, "gcsomething");
        reedline.run_edit_commands(&[EditCommand::MoveToPosition {
            position: 2,
            select: false,
        }]);
        assert!(
            reedline.try_expand_abbreviation_at_cursor(true).is_none(),
            "must not expand the prefix of a word when the cursor is mid-word"
        );
    }

    #[test]
    fn abbreviation_expands_before_trailing_text_on_submit() {
        let mut reedline =
            reedline_with_abbrevs_and_default_string_lit_check(&[("gc", "git commit")]);
        set_buffer_at_end(&mut reedline, "gc rest");
        reedline.run_edit_commands(&[EditCommand::MoveToPosition {
            position: 2,
            select: false,
        }]);
        let event = reedline.try_expand_abbreviation_at_cursor(true);
        assert!(
            event.is_some(),
            "expected expansion at a real word boundary"
        );
        reedline.run_edit_commands(&match event.unwrap() {
            ReedlineEvent::Edit(cmds) => cmds,
            _ => panic!("expected Edit event"),
        });
        assert_eq!(reedline.current_buffer_contents(), "git commit rest");
    }

    // Feed one key as its own batch, mirroring real interactive input where each
    // keypress drives a separate `process_input_batch`.
    fn step_key(rl: &mut Reedline, k: KeyEvent) -> ControlFlow<Signal> {
        rl.process_input_batch(&DefaultPrompt::default(), vec![Event::Key(k)])
            .expect("batch ok")
    }

    #[test]
    fn abbreviation_expands_on_enter_in_vi_normal() {
        // Regression: a vi-normal block caret rests *on* the last grapheme, so
        // before the caret-release on Enter the submit-time scan saw `g` instead
        // of `gc` and silently skipped expansion.
        let mut abbreviations = HashMap::new();
        abbreviations.insert("gc".to_string(), "git commit".to_string());
        let mut rl = seam_engine(Box::<crate::Vi>::default()).with_abbreviations(abbreviations);

        let _ = step_key(&mut rl, ch('g'));
        let _ = step_key(&mut rl, ch('c'));
        let _ = step_key(&mut rl, key(KeyCode::Esc)); // vi normal, caret on 'c'
        match step_key(&mut rl, key(KeyCode::Enter)) {
            ControlFlow::Break(Signal::Success(buf)) => assert_eq!(buf, "git commit"),
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn vi_normal_enter_inserts_newline_at_end_not_mid_word() {
        // Regression: the same stranded block caret made an incomplete-input
        // newline land one grapheme short, splitting the last word (`ab` -> `a\nb`).
        struct AlwaysIncomplete;
        impl crate::Validator for AlwaysIncomplete {
            fn validate(&self, _line: &str) -> ValidationResult {
                ValidationResult::Incomplete
            }
        }
        let mut rl =
            seam_engine(Box::<crate::Vi>::default()).with_validator(Box::new(AlwaysIncomplete));

        let _ = step_key(&mut rl, ch('a'));
        let _ = step_key(&mut rl, ch('b'));
        let _ = step_key(&mut rl, key(KeyCode::Esc)); // vi normal, caret on 'b'
        let _ = step_key(&mut rl, key(KeyCode::Enter)); // incomplete -> insert newline
        assert_eq!(rl.editor.get_buffer(), "ab\n");
    }

    #[cfg(feature = "bashisms")]
    fn reedline_with_history_and_string_lit_check(entries: &[&str]) -> Reedline {
        let mut reedline =
            Reedline::create().with_highlighter(Box::new(ExampleHighlighter::default()));
        for entry in entries {
            reedline
                .history
                .save(HistoryItem::from_command_line(*entry))
                .expect("failed to save history");
        }
        reedline
    }

    #[cfg(feature = "bashisms")]
    fn reedline_with_history_default(entries: &[&str]) -> Reedline {
        let mut reedline =
            Reedline::create().with_highlighter(Box::new(SimpleMatchHighlighter::default()));
        for entry in entries {
            reedline
                .history
                .save(HistoryItem::from_command_line(*entry))
                .expect("failed to save history");
        }
        reedline
    }

    #[rstest]
    #[case("!!", true)]
    #[case("\"echo !!", false)]
    #[case("'echo !!", false)]
    #[case("'echo' !!", true)]
    #[case("\"echo !git", false)]
    #[case("'echo !git", false)]
    #[case("'Сегодня !!", false)]
    #[case("'今日は !!", false)]
    #[case("'🔥 !!", false)]
    #[cfg(feature = "bashisms")]
    fn bang_string_detection_with_override(#[case] buffer: &str, #[case] should_expand: bool) {
        let mut reedline = reedline_with_history_and_string_lit_check(&["git status"]);
        set_buffer_at_end(&mut reedline, buffer);
        assert_eq!(reedline.parse_bang_command().is_some(), should_expand);
    }

    #[rstest]
    #[case("\"echo !!")]
    #[case("'echo !!")]
    #[case("'echo' !!")]
    #[case("\"echo !git")]
    #[case("'echo !git")]
    #[case("'Сегодня !!")]
    #[case("'今日は !!")]
    #[case("'🔥 !!")]
    #[cfg(feature = "bashisms")]
    fn bang_always_expands_without_override(#[case] buffer: &str) {
        let mut reedline = reedline_with_history_default(&["git status"]);
        set_buffer_at_end(&mut reedline, buffer);
        assert!(
            reedline.parse_bang_command().is_some(),
            "must expand when highlighter does not override should_expand_abbr"
        );
    }

    #[rstest]
    #[case("")]
    #[case("line of text")]
    #[case(
        "longgggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg line of text"
    )]
    fn test_move_to_line_start(#[case] input: &str) {
        let mut reedline = Reedline::create();

        // Write the string, and then move to the start of the line.
        let insertion = EditCommand::InsertString(String::from(input));
        reedline.run_edit_commands(&[insertion]);

        let move_to_start = EditCommand::MoveToLineStart { select: false };
        reedline.run_edit_commands(&[move_to_start]);

        assert_eq!(reedline.editor.line_buffer().insertion_point(), 0);
    }

    #[rstest]
    #[case("")]
    #[case("line of text")]
    #[case(
        "longgggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg line of text"
    )]
    fn test_move_to_line_start_history(#[case] input: &str) {
        let mut reedline = Reedline::create();

        // Enter the string into history, then scroll back up and move to the start of the line.
        let history = HistoryItem::from_command_line(input);
        reedline.history.save(history).unwrap();

        reedline.previous_history().expect("history ok");

        let move_to_start = EditCommand::MoveToLineStart { select: false };
        reedline.run_edit_commands(&[move_to_start]);

        assert_eq!(reedline.editor.line_buffer().insertion_point(), 0);
    }

    #[rstest]
    #[case("a\nb", 2)]
    #[case("123456789\n123456789\n123456789", 20)]
    #[case("0\n1\n2\n3\n4\n5\n6\n7\n8\n9", 18)]
    fn test_move_to_line_start_multiline(#[case] input: &str, #[case] last_line_start: usize) {
        let mut reedline = Reedline::create();

        // Write the string, and then move to the start of the last line.
        let insertion = EditCommand::InsertString(String::from(input));
        reedline.run_edit_commands(&[insertion]);

        let move_to_start = EditCommand::MoveToLineStart { select: false };
        reedline.run_edit_commands(&[move_to_start]);

        assert_eq!(
            reedline.editor.line_buffer().insertion_point(),
            last_line_start
        );
    }

    #[rstest]
    #[case("a\nb")]
    #[case("123456789\n123456789\n123456789")]
    #[case("0\n1\n2\n3\n4\n5\n6\n7\n8\n9")]
    fn test_move_to_line_start_multiline_history_up_start(#[case] input: &str) {
        let mut reedline = Reedline::create();

        // Enter the string into history, then scroll back up and move to the start of the line.
        let history = HistoryItem::from_command_line(input);
        reedline.history.save(history).unwrap();

        reedline.previous_history().expect("history ok");

        let move_to_start = EditCommand::MoveToLineStart { select: false };
        reedline.run_edit_commands(&[move_to_start]);

        assert_eq!(reedline.editor.line_buffer().insertion_point(), 0);
    }

    #[rstest]
    #[case("a\nb", 2)]
    #[case("123456789\n123456789\n123456789", 10)]
    #[case("0\n1\n2\n3\n4\n5\n6\n7\n8\n9", 2)]
    fn test_move_to_line_start_multiline_history_up_down_start(
        #[case] input: &str,
        #[case] second_line_start: usize,
    ) {
        let mut reedline = Reedline::create();

        // Enter the string again, then scroll up in history, move down one line,
        // and move to the start of the second line.
        let history = HistoryItem::from_command_line(input);
        reedline.history.save(history).unwrap();

        reedline.previous_history().expect("history ok");

        reedline.down_command().expect("history ok");

        let move_to_start = EditCommand::MoveToLineStart { select: false };
        reedline.run_edit_commands(&[move_to_start]);

        assert_eq!(
            reedline.editor.line_buffer().insertion_point(),
            second_line_start
        );
    }

    #[rstest]
    #[case("a\nb", 2)]
    #[case("123456789\n123456789\n123456789", 20)]
    #[case("0\n1\n2\n3\n4\n5\n6\n7\n8\n9", 18)]
    fn test_move_to_line_start_multiline_history_up_end_start(
        #[case] input: &str,
        #[case] last_line_start: usize,
    ) {
        let mut reedline = Reedline::create();

        // Enter the string again, then scroll up in history, move to the end of the text,
        // and move to the start of the last line.
        let history = HistoryItem::from_command_line(input);
        reedline.history.save(history).unwrap();

        reedline.previous_history().expect("history ok");

        let move_to_end = EditCommand::MoveToEnd { select: false };
        reedline.run_edit_commands(&[move_to_end]);

        let move_to_start = EditCommand::MoveToLineStart { select: false };
        reedline.run_edit_commands(&[move_to_start]);

        assert_eq!(
            reedline.editor.line_buffer().insertion_point(),
            last_line_start
        );
    }

    #[test]
    fn test_complete_line_from_history() {
        let completer = Box::new(DefaultCompleter::new(Vec::from([String::from("67")])));
        let completion_menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("completion_menu"),
        ));
        let mut reedline = Reedline::create()
            .with_quick_completions(true)
            .with_completer(completer)
            .with_menu(completion_menu);

        // Save "6" to the history and scroll back to it
        let history = HistoryItem::from_command_line("6");
        reedline.history.save(history).unwrap();
        reedline.previous_history().expect("history ok");
        assert_eq!(reedline.current_buffer_contents(), "6");

        // Perform quick completion
        let prompt = DefaultPrompt::default();
        let completion = ReedlineEvent::Menu(String::from("completion_menu"));
        reedline.handle_event(&prompt, completion).unwrap();
        assert_eq!(reedline.current_buffer_contents(), "67");

        // Insert the "x" to the prompt
        let insertion = EditCommand::InsertString(String::from("x"));
        reedline.run_edit_commands(&[insertion]);
        assert_eq!(reedline.current_buffer_contents(), "67x");
    }

    /// A completer that computes in the background: the first request cannot be answered
    /// for the line on screen, and only later ones carry its values. This is what a cold
    /// cache looks like from the engine's side (nushell/reedline#1142).
    struct DeferredCompleter {
        first_answer: CompletionResult,
        values: Vec<String>,
        dispatched: bool,
    }

    impl DeferredCompleter {
        /// A cold cache: nothing to show at all until the background work lands.
        fn pending(values: &[&str]) -> Self {
            Self::new(CompletionResult::Pending, values)
        }

        /// A warm-but-wrong cache: `stale` is answered from a neighbouring entry, so the
        /// value is real but its span belongs to `origin_buffer` rather than the line.
        fn stale(stale: &str, origin_buffer: &str, values: &[&str]) -> Self {
            let first_answer = CompletionResult::Stale {
                suggestions: vec![Suggestion {
                    value: stale.to_string(),
                    span: Span {
                        start: 0,
                        end: origin_buffer.len(),
                    },
                    ..Default::default()
                }]
                .into(),
                origin: CompletionOrigin::new(origin_buffer, origin_buffer.len()),
                partial: None,
            };
            Self::new(first_answer, values)
        }

        fn new(first_answer: CompletionResult, values: &[&str]) -> Self {
            Self {
                first_answer,
                values: values.iter().map(|value| value.to_string()).collect(),
                dispatched: false,
            }
        }
    }

    impl Completer for DeferredCompleter {
        fn complete(&mut self, _line: &str, pos: usize) -> CompletionResult {
            if !self.dispatched {
                self.dispatched = true;
                return self.first_answer.clone();
            }
            CompletionResult::fresh(
                self.values
                    .iter()
                    .map(|value| Suggestion {
                        value: value.clone(),
                        span: Span { start: 0, end: pos },
                        ..Default::default()
                    })
                    .collect::<Vec<_>>(),
            )
        }

        fn poll_completion(&mut self) -> CompletionStatus {
            if self.dispatched {
                CompletionStatus::Ready
            } else {
                CompletionStatus::Idle
            }
        }
    }

    /// [`engine_awaiting`] with partial completions off.
    fn engine_awaiting_completions(values: &[&str], buffer: &str, quick: bool) -> Reedline {
        engine_awaiting(values, buffer, quick, false)
    }

    /// Engine with `quick` and `partial` completions and a background completer over
    /// `values`, with `buffer` typed and the completion menu activated — the state right
    /// after Tab, while the completer is still working.
    fn engine_awaiting(values: &[&str], buffer: &str, quick: bool, partial: bool) -> Reedline {
        let (reedline, _) = activate_menu_over(
            Box::new(DeferredCompleter::pending(values)),
            buffer,
            quick,
            partial,
        );

        // Nothing could be decided yet, the premise of every test below.
        assert_eq!(reedline.current_buffer_contents(), buffer);
        reedline
    }

    /// Type `buffer` and press Tab, against a completer that answers however the test
    /// needs it to. Returns the engine and how the menu activation was handled.
    fn activate_menu_over(
        completer: Box<dyn Completer + Send>,
        buffer: &str,
        quick: bool,
        partial: bool,
    ) -> (Reedline, EventStatus) {
        let completion_menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("completion_menu"),
        ));
        let mut reedline = Reedline::create()
            .with_completer(completer)
            .with_menu(completion_menu)
            .with_quick_completions(quick)
            .with_partial_completions(partial);

        // Settling repaints, which needs a painter that believes it is on a terminal.
        // Size first: `handle_resize` invalidates the anchor the next line pins.
        reedline.painter.handle_resize(80, 24);
        reedline.painter.force_prompt_anchored_for_test(0);

        reedline.run_edit_commands(&[EditCommand::InsertString(buffer.to_string())]);
        let status = reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Menu(String::from("completion_menu")),
            )
            .unwrap();

        (reedline, status)
    }

    fn settle(reedline: &mut Reedline) {
        reedline
            .settle_completions(&DefaultPrompt::default())
            .unwrap();
    }

    /// The regression: a lone suggestion that arrives after the menu opened must still be
    /// accepted, exactly as a synchronous completer's would have been.
    #[test]
    fn quick_completion_accepts_a_lone_suggestion_that_arrives_late() {
        let mut reedline = engine_awaiting_completions(&["crates"], "cr", true);

        settle(&mut reedline);

        assert_eq!(reedline.current_buffer_contents(), "crates");
        assert!(!menu_is_active(&reedline), "accepting closes the menu");
    }

    /// The decision belongs to the keystroke that asked for it. Once the user has typed
    /// on, a late result must not rewrite the line underneath them.
    #[test]
    fn a_late_lone_suggestion_is_dropped_once_the_line_moved_on() {
        let mut reedline = engine_awaiting_completions(&["crates"], "cr", true);

        send_edit(&mut reedline, EditCommand::InsertChar('a'));
        settle(&mut reedline);

        assert_eq!(reedline.current_buffer_contents(), "cra");
    }

    /// Arming happens before the count is final, so settling has to re-check it: several
    /// suggestions mean a menu, not an acceptance.
    #[test]
    fn late_results_with_several_suggestions_only_populate_the_menu() {
        let mut reedline = engine_awaiting_completions(&["test", "this", "that"], "t", true);

        settle(&mut reedline);

        assert_eq!(reedline.current_buffer_contents(), "t");
        assert!(menu_is_active(&reedline));
    }

    /// An arm is spent by the result that settles it, so a later request cannot inherit it.
    #[test]
    fn an_unsatisfied_arm_does_not_fire_on_a_later_result() {
        let mut reedline = engine_awaiting_completions(&["test", "this", "that"], "t", true);

        settle(&mut reedline);
        assert!(reedline.deferred_menu_completion.is_none(), "arm is spent");

        // A second round of results, now down to one value, with no Tab in between.
        reedline.completer = Box::new(DeferredCompleter::pending(&["test"]));
        reedline.completer.complete("t", 1);
        settle(&mut reedline);

        assert_eq!(reedline.current_buffer_contents(), "t");
    }

    /// The other half of the same Tab: partial completions read the same empty menu the
    /// quick check did, so a shared prefix must be spliced in when the values land.
    #[test]
    fn late_results_splice_the_shared_prefix() {
        let mut reedline = engine_awaiting(&["nu-cmd-base", "nu-cmd-lang"], "nu-cm", true, true);

        settle(&mut reedline);

        assert_eq!(reedline.current_buffer_contents(), "nu-cmd-");
    }

    /// A shared prefix is spliced under `partial` alone, with no quick completions.
    #[test]
    fn late_results_splice_the_shared_prefix_without_quick_completions() {
        let mut reedline = engine_awaiting(&["nu-cmd-base", "nu-cmd-lang"], "nu-cm", false, true);

        settle(&mut reedline);

        assert_eq!(reedline.current_buffer_contents(), "nu-cmd-");
    }

    /// Accepting a lone value wins over splicing, as it does on activation.
    #[test]
    fn a_lone_late_suggestion_is_accepted_rather_than_spliced() {
        let mut reedline = engine_awaiting(&["crates"], "cr", true, true);

        settle(&mut reedline);

        assert_eq!(reedline.current_buffer_contents(), "crates");
        assert!(!menu_is_active(&reedline));
    }

    /// Splicing is owed to a keystroke too, and is void once the line moved on.
    #[test]
    fn a_late_shared_prefix_is_dropped_once_the_line_moved_on() {
        let mut reedline = engine_awaiting(&["nu-cmd-base", "nu-cmd-lang"], "nu-cm", true, true);

        send_edit(&mut reedline, EditCommand::InsertChar('d'));
        settle(&mut reedline);

        assert_eq!(reedline.current_buffer_contents(), "nu-cmd");
    }

    /// With quick completions off, late results populate the menu and nothing else.
    #[test]
    fn late_results_never_auto_accept_without_quick_completions() {
        let mut reedline = engine_awaiting_completions(&["crates"], "cr", false);

        settle(&mut reedline);

        assert_eq!(reedline.current_buffer_contents(), "cr");
        assert!(menu_is_active(&reedline));
    }

    /// A lone *stale* suggestion looks like a lone fresh one to the quick completion
    /// check, but accepting it is a no-op. The Tab must still be honoured once the real
    /// answer lands.
    #[test]
    fn a_lone_stale_suggestion_does_not_swallow_the_completion() {
        let (mut reedline, _) = activate_menu_over(
            Box::new(DeferredCompleter::stale("console", "co", &["crates"])),
            "cr",
            true,
            false,
        );

        // The stale span is refused, so the buffer is untouched.
        assert_eq!(reedline.current_buffer_contents(), "cr");

        settle(&mut reedline);

        assert_eq!(
            reedline.current_buffer_contents(),
            "crates",
            "the fresh result was dropped because the stale one closed the menu first"
        );
    }

    /// The same lone stale value through the second route: `MenuNext` with the menu
    /// already open. The accept is refused downstream either way, but the `Enter` it
    /// rode on would still deactivate the menu, so the pending completion had nothing
    /// left to land in.
    #[test]
    fn menu_next_does_not_close_the_menu_over_a_lone_stale_value() {
        let (mut reedline, _) = activate_menu_over(
            Box::new(DeferredCompleter::stale("console", "co", &["crates"])),
            "cr",
            true,
            false,
        );
        assert!(menu_is_active(&reedline), "setup");

        reedline
            .handle_event(&DefaultPrompt::default(), ReedlineEvent::MenuNext)
            .unwrap();

        assert!(
            menu_is_active(&reedline),
            "MenuNext accepted a provisional lone value and closed the menu"
        );
        assert_eq!(reedline.current_buffer_contents(), "cr");

        // With the menu still open, the arm from the activation is still owed.
        settle(&mut reedline);
        assert_eq!(reedline.current_buffer_contents(), "crates");
    }

    /// The flicker: a menu opened over an answer that is not about this line stays off
    /// screen, so a Tab that resolves to one suggestion never draws a menu at all.
    #[test]
    fn an_opening_menu_is_not_visible_until_answered() {
        let (reedline, _) = activate_menu_over(
            Box::new(DeferredCompleter::stale("console", "co", &["crates"])),
            "cr",
            true,
            false,
        );

        let menu = reedline
            .menus
            .iter()
            .find(|menu| menu.is_active())
            .expect("the menu is open");
        assert!(
            menu.is_awaiting_first_answer(),
            "it would only be taken away again once the real answer lands"
        );
    }

    fn menu_is_active(reedline: &Reedline) -> bool {
        reedline.menus.iter().any(|menu| menu.is_active())
    }

    /// Engine with a completion menu activated on a "th" buffer. "th" matches
    /// two words, so quick completions don't auto-select on activation.
    fn engine_with_active_menu(quick: bool, persistent: bool) -> Reedline {
        let completer = Box::new(DefaultCompleter::new_with_wordlen(
            vec![
                String::from("test"),
                String::from("this"),
                String::from("that"),
            ],
            1,
        ));
        let completion_menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("completion_menu"),
        ));
        let mut reedline = Reedline::create()
            .with_completer(completer)
            .with_menu(completion_menu)
            .with_quick_completions(quick)
            .with_persistent_menus(persistent);

        reedline.run_edit_commands(&[EditCommand::InsertString(String::from("th"))]);
        reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Menu(String::from("completion_menu")),
            )
            .unwrap();
        assert!(menu_is_active(&reedline));
        reedline
    }

    /// A completer that always answers with the same suggestions, so a test can
    /// pin down both the candidates and the span they replace.
    struct FixedCompleter {
        suggestions: Vec<Suggestion>,
    }

    impl FixedCompleter {
        fn new(values: &[&str], span: Span) -> Self {
            Self {
                suggestions: values
                    .iter()
                    .map(|value| Suggestion {
                        value: (*value).to_string(),
                        span,
                        ..Default::default()
                    })
                    .collect(),
            }
        }
    }

    impl Completer for FixedCompleter {
        fn complete(&mut self, _line: &str, _pos: usize) -> CompletionResult {
            CompletionResult::fresh(self.suggestions.clone())
        }
    }

    /// An engine with a completion menu and `buffer` already typed.
    fn engine_with_completion_menu(
        buffer: &str,
        completer: Box<dyn Completer + Send>,
        quick: bool,
        partial: bool,
    ) -> Reedline {
        let completion_menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("completion_menu"),
        ));
        let mut reedline = Reedline::create()
            .with_completer(completer)
            .with_menu(completion_menu)
            .with_quick_completions(quick)
            .with_partial_completions(partial);
        reedline.run_edit_commands(&[EditCommand::InsertString(buffer.to_string())]);
        reedline
    }

    fn active_menu_value_count(reedline: &Reedline) -> usize {
        reedline
            .menus
            .iter()
            .find(|menu| menu.is_active())
            .expect("menu open")
            .get_values()
            .len()
    }

    #[test]
    fn insert_completions_inserts_every_candidate_separated_by_spaces() {
        let mut reedline = engine_with_completion_menu(
            "th",
            Box::new(FixedCompleter::new(
                &["test", "this", "that"],
                Span::new(0, 2),
            )),
            false,
            false,
        );

        let status = reedline
            .handle_event(&DefaultPrompt::default(), ReedlineEvent::InsertCompletions)
            .unwrap();

        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(reedline.current_buffer_contents(), "test this that ");
    }

    #[test]
    fn insert_completions_replaces_only_the_completer_span() {
        let mut reedline = engine_with_completion_menu(
            "git ch",
            Box::new(FixedCompleter::new(
                &["checkout", "cherry-pick"],
                Span::new(4, 6),
            )),
            false,
            false,
        );

        reedline
            .handle_event(&DefaultPrompt::default(), ReedlineEvent::InsertCompletions)
            .unwrap();

        assert_eq!(
            reedline.current_buffer_contents(),
            "git checkout cherry-pick "
        );
    }

    #[test]
    fn insert_completions_is_a_no_op_without_candidates() {
        let mut reedline = engine_with_completion_menu(
            "zzz",
            Box::new(FixedCompleter::new(&[], Span::new(0, 3))),
            false,
            false,
        );

        let status = reedline
            .handle_event(&DefaultPrompt::default(), ReedlineEvent::InsertCompletions)
            .unwrap();

        assert!(matches!(status, EventStatus::Inapplicable));
        assert_eq!(reedline.current_buffer_contents(), "zzz");
    }

    #[test]
    fn possible_completions_lists_candidates_without_touching_the_line() {
        let mut reedline = engine_with_completion_menu(
            "th",
            Box::new(FixedCompleter::new(
                &["test", "this", "that"],
                Span::new(0, 2),
            )),
            false,
            false,
        );

        let status = reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::PossibleCompletions,
            )
            .unwrap();

        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(reedline.current_buffer_contents(), "th");
        assert!(menu_is_active(&reedline));
        assert_eq!(active_menu_value_count(&reedline), 3);
    }

    /// `possible-completions` lists, it never completes: a lone candidate must
    /// stay in the menu even with the options that make Tab accept it.
    #[test]
    fn possible_completions_does_not_accept_a_lone_candidate() {
        let mut reedline = engine_with_completion_menu(
            "th",
            Box::new(FixedCompleter::new(&["test"], Span::new(0, 2))),
            true,
            true,
        );

        let status = reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::PossibleCompletions,
            )
            .unwrap();

        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(reedline.current_buffer_contents(), "th");
        assert!(menu_is_active(&reedline));
    }

    #[test]
    fn possible_completions_is_a_no_op_without_candidates() {
        let mut reedline = engine_with_completion_menu(
            "zzz",
            Box::new(FixedCompleter::new(&[], Span::new(0, 3))),
            false,
            false,
        );

        let status = reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::PossibleCompletions,
            )
            .unwrap();

        assert!(matches!(status, EventStatus::Inapplicable));
        assert_eq!(reedline.current_buffer_contents(), "zzz");
        assert!(!menu_is_active(&reedline), "nothing to leave open");
    }

    /// Without a menu there is nowhere to draw the list; the event must not fall
    /// back to completing the line.
    #[test]
    fn possible_completions_without_a_menu_changes_nothing() {
        let mut reedline = Reedline::create()
            .with_completer(Box::new(FixedCompleter::new(&["test"], Span::new(0, 2))));
        reedline.run_edit_commands(&[EditCommand::InsertString("th".to_string())]);

        let status = reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::PossibleCompletions,
            )
            .unwrap();

        assert!(matches!(status, EventStatus::Inapplicable));
        assert_eq!(reedline.current_buffer_contents(), "th");
    }

    /// The menu maintenance the paint cycle runs between keystrokes — value
    /// refresh only, no layout (the headless test painter has no width) — so
    /// an assertion after a typed character sees what a user would on screen.
    fn apply_menu_maintenance(reedline: &mut Reedline) {
        for menu in reedline.menus.iter_mut() {
            if menu.is_active() {
                menu.update_values(
                    &mut reedline.editor,
                    reedline.completer.as_mut(),
                    reedline.history.as_ref(),
                );
            }
        }
    }

    // --- a stale menu must not intercept Enter ---
    //
    // The completion menu stays active while the user types past it, and any
    // Enter while a menu is active is routed to the menu. Left unguarded that
    // turns Enter into a dead key (empty menu) or an insertion of whatever the
    // menu last highlighted (non-empty menu) at the end of a finished line.

    /// `Enter`, `Submit` and `SubmitOrNewline` all reach the line through
    /// `MenuAccept`, so an open menu takes all three the same way.
    #[rstest]
    #[case::enter(ReedlineEvent::Enter)]
    #[case::submit(ReedlineEvent::Submit)]
    #[case::submit_or_newline(ReedlineEvent::SubmitOrNewline)]
    fn every_submit_event_accepts_an_open_menu(#[case] event: ReedlineEvent) {
        let mut reedline = engine_with_active_menu(false, false);
        reedline.painter.force_prompt_anchored_for_test(0);
        apply_menu_maintenance(&mut reedline);
        let prompt = DefaultPrompt::default();

        let status = reedline.handle_event(&prompt, event).unwrap();

        assert!(
            matches!(status, EventStatus::Handled),
            "the menu takes the keypress"
        );
        assert!(!menu_is_active(&reedline), "and closes behind the accept");
        assert_ne!(
            reedline.editor.get_buffer(),
            "th",
            "the selection reached the buffer"
        );
    }

    /// And once the menu declines, all three mean what they always meant.
    #[rstest]
    #[case::enter(ReedlineEvent::Enter)]
    #[case::submit(ReedlineEvent::Submit)]
    #[case::submit_or_newline(ReedlineEvent::SubmitOrNewline)]
    fn every_submit_event_passes_an_empty_menu(#[case] event: ReedlineEvent) {
        let mut reedline = engine_with_active_menu(false, false);
        reedline.painter.force_prompt_anchored_for_test(0);
        let prompt = DefaultPrompt::default();
        // "thz" matches nothing; the menu refilters to empty but stays active.
        reedline
            .handle_event(
                &prompt,
                ReedlineEvent::Edit(vec![EditCommand::InsertChar('z')]),
            )
            .unwrap();
        apply_menu_maintenance(&mut reedline);
        assert!(menu_is_active(&reedline));

        let status = reedline.handle_event(&prompt, event).unwrap();

        assert!(
            matches!(status, EventStatus::Exits(Signal::Success(ref s)) if s == "thz"),
            "the line ran"
        );
    }

    /// A menu whose filtered suggestions are empty has nothing to accept:
    /// Enter must close it and submit the line, not be swallowed.
    #[test]
    fn enter_with_an_empty_menu_submits_the_line() {
        let mut reedline = engine_with_active_menu(false, false);
        reedline.painter.force_prompt_anchored_for_test(0);
        let prompt = DefaultPrompt::default();
        // "thz" matches nothing; the menu refilters to empty but stays active.
        reedline
            .handle_event(
                &prompt,
                ReedlineEvent::Edit(vec![EditCommand::InsertChar('z')]),
            )
            .unwrap();
        apply_menu_maintenance(&mut reedline);
        assert!(menu_is_active(&reedline));

        let status = reedline
            .handle_event(&prompt, ReedlineEvent::Enter)
            .unwrap();
        assert!(
            matches!(status, EventStatus::Exits(Signal::Success(ref s)) if s == "thz"),
            "Enter was swallowed by an empty menu"
        );
        assert!(!menu_is_active(&reedline));
    }

    /// The full shape of the report this fixes: Tab opens the menu mid-line,
    /// the user types the rest of the statement and hits Enter. Before the
    /// fix the Enter was consumed by the stale menu instead of submitting.
    #[test]
    fn typing_past_a_completion_then_enter_runs_the_line() {
        let mut reedline = engine_with_active_menu(false, false);
        reedline.painter.force_prompt_anchored_for_test(0);
        let prompt = DefaultPrompt::default();
        for c in " more words".chars() {
            reedline
                .handle_event(
                    &prompt,
                    ReedlineEvent::Edit(vec![EditCommand::InsertChar(c)]),
                )
                .unwrap();
        }
        apply_menu_maintenance(&mut reedline);

        let status = reedline
            .handle_event(&prompt, ReedlineEvent::Enter)
            .unwrap();
        assert!(
            matches!(status, EventStatus::Exits(Signal::Success(ref s)) if s == "th more words"),
            "the line was not submitted intact"
        );
    }

    /// A completion menu open over "th", closing at the end of the word for
    /// the given word characters.
    fn engine_with_word_bounded_menu(word_chars: &str, persistent: bool) -> Reedline {
        let completer = Box::new(DefaultCompleter::new_with_wordlen(
            vec![
                String::from("test"),
                String::from("this"),
                String::from("that"),
            ],
            1,
        ));
        let completion_menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default()
                .with_name("completion_menu")
                .with_word_chars(word_chars),
        ));
        let mut reedline = Reedline::create()
            .with_completer(completer)
            .with_menu(completion_menu)
            .with_persistent_menus(persistent);

        reedline.run_edit_commands(&[EditCommand::InsertString(String::from("th"))]);
        reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Menu(String::from("completion_menu")),
            )
            .unwrap();
        assert!(menu_is_active(&reedline));
        reedline
    }

    fn insert(reedline: &mut Reedline, c: char) {
        reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Edit(vec![EditCommand::InsertChar(c)]),
            )
            .unwrap();
    }

    /// A character that cannot extend the word ends the menu opened for it;
    /// one that can leaves the menu to refilter.
    #[rstest]
    #[case::space(' ', true)]
    #[case::statement_end(';', true)]
    #[case::close_paren(')', true)]
    #[case::letter('e', false)]
    #[case::underscore('_', false)]
    #[case::qualifier('.', false)]
    fn a_word_boundary_closes_the_menu(#[case] typed: char, #[case] closes: bool) {
        let mut reedline = engine_with_word_bounded_menu("_.", false);
        insert(&mut reedline, typed);
        assert_eq!(!menu_is_active(&reedline), closes, "inserting {typed:?}");
    }

    /// A newline ends the word however it arrives: the default `Alt+Enter`
    /// and `Shift+Enter` bindings emit `InsertNewline`, not `InsertChar`, and
    /// a menu that outlived it would hold `Enter` on the next line of a
    /// multi-line statement.
    #[test]
    fn a_newline_command_closes_the_menu() {
        let mut reedline = engine_with_word_bounded_menu("_.", false);
        reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
            )
            .unwrap();
        assert!(!menu_is_active(&reedline));
    }

    /// The word characters are the caller's to choose: a path completer keeps
    /// its menu across the separators an identifier completer ends on.
    #[rstest]
    #[case::path_separator('/', false)]
    #[case::dash('-', false)]
    #[case::space(' ', true)]
    fn word_chars_decide_where_the_word_ends(#[case] typed: char, #[case] closes: bool) {
        let mut reedline = engine_with_word_bounded_menu("_-./", false);
        insert(&mut reedline, typed);
        assert_eq!(!menu_is_active(&reedline), closes, "inserting {typed:?}");
    }

    /// Left unset, a menu still lives for the rest of the line.
    #[rstest]
    #[case::space(' ')]
    #[case::statement_end(';')]
    fn a_menu_without_word_chars_outlives_the_word(#[case] typed: char) {
        let mut reedline = engine_with_active_menu(false, false);
        insert(&mut reedline, typed);
        assert!(menu_is_active(&reedline), "inserting {typed:?}");
    }

    /// A persistent menu is persistent: the word ending does not close it.
    #[test]
    fn a_persistent_menu_survives_the_end_of_the_word() {
        let mut reedline = engine_with_word_bounded_menu("_.", true);
        insert(&mut reedline, ';');
        assert!(menu_is_active(&reedline));
    }

    /// Only typing ends a word. Text arriving whole — a paste, a macro — is
    /// not the user walking off the end of the completion.
    #[test]
    fn inserted_text_does_not_end_the_word() {
        let mut reedline = engine_with_word_bounded_menu("_.", false);
        reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Edit(vec![EditCommand::InsertString(String::from("is; "))]),
            )
            .unwrap();
        assert!(menu_is_active(&reedline));
    }

    /// A menu that filters on whole command lines rather than a word — the
    /// history menu `examples/demo.rs` pairs with the completion menu — has no
    /// word to end, and is left alone while its neighbour closes.
    #[test]
    fn a_menu_without_word_chars_is_left_alone() {
        let mut reedline = Reedline::create()
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(
                ColumnarMenu::default()
                    .with_name("completion_menu")
                    .with_word_chars("_."),
            )))
            .with_menu(ReedlineMenu::HistoryMenu(Box::new(
                ListMenu::default().with_name("history_menu"),
            )));
        let prompt = DefaultPrompt::default();

        reedline.run_edit_commands(&[EditCommand::InsertString(String::from("git"))]);
        reedline
            .handle_event(&prompt, ReedlineEvent::Menu(String::from("history_menu")))
            .unwrap();
        assert!(menu_is_active(&reedline));

        insert(&mut reedline, ' ');

        assert!(
            menu_is_active(&reedline),
            "a history search is a line, not a word"
        );
    }

    /// The space that ends a word also triggers abbreviation expansion, which
    /// returns out of the edit before the menu is looked at again. The word
    /// ends first, so the menu does not outlive the expansion.
    #[test]
    fn an_abbreviation_expanded_on_the_space_still_ends_the_word() {
        let mut reedline = engine_with_word_bounded_menu("_.", false)
            .with_abbreviations(HashMap::from([(String::from("th"), String::from("there"))]));

        insert(&mut reedline, ' ');

        assert_eq!(reedline.editor.get_buffer(), "there ");
        assert!(!menu_is_active(&reedline), "the space ended the word");
    }

    /// A completer that counts how often it is asked.
    struct CountingCompleter {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Completer for CountingCompleter {
        fn complete(&mut self, _line: &str, _pos: usize) -> CompletionResult {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            CompletionResult::fresh(Vec::new())
        }
    }

    /// The character that ends the word closes the menu before anything asks
    /// the completer to refilter it: the answer would be thrown away.
    #[test]
    fn the_completer_is_not_asked_about_a_word_that_has_ended() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let completion_menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default()
                .with_name("completion_menu")
                .with_word_chars("_."),
        ));
        let mut reedline = Reedline::create()
            .with_completer(Box::new(CountingCompleter {
                calls: Arc::clone(&calls),
            }))
            .with_menu(completion_menu)
            .with_quick_completions(true);
        let prompt = DefaultPrompt::default();

        reedline.run_edit_commands(&[EditCommand::InsertString(String::from("th"))]);
        reedline
            .handle_event(
                &prompt,
                ReedlineEvent::Menu(String::from("completion_menu")),
            )
            .unwrap();
        calls.store(0, Ordering::Relaxed);

        insert(&mut reedline, ' ');

        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(!menu_is_active(&reedline));
    }

    /// Typed fast enough, the rest of a statement arrives as one `Edit` of
    /// several `InsertChar`s. The word still ends inside it.
    #[test]
    fn a_word_ends_inside_a_burst_of_typing() {
        let mut reedline = engine_with_word_bounded_menu("_.", false);

        reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Edit(vec![
                    EditCommand::InsertChar('i'),
                    EditCommand::InsertChar('s'),
                    EditCommand::InsertChar(';'),
                    EditCommand::InsertChar(' '),
                ]),
            )
            .unwrap();

        assert!(!menu_is_active(&reedline), "the ';' ended the word");
    }

    /// A completer that always has something to offer, the way a grammar-driven
    /// SQL completer suggests next-statement keywords after a ';'. A menu fed by
    /// one of these is never empty, so nothing but the end of the word takes its
    /// claim on `Enter` away.
    struct AlwaysSuggests;

    impl Completer for AlwaysSuggests {
        fn complete(&mut self, _line: &str, pos: usize) -> CompletionResult {
            CompletionResult::fresh(vec![Suggestion {
                value: String::from("table"),
                span: Span {
                    start: pos,
                    end: pos,
                },
                ..Default::default()
            }])
        }
    }

    /// The report this comes from, byte for byte: `show tab`, Tab to open the
    /// menu, then `les;` and Enter. The ';' ends the word, so Enter runs the
    /// statement instead of appending the highlighted "table" to it.
    #[test]
    fn the_end_of_a_word_returns_enter_to_the_line() {
        let completion_menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default()
                .with_name("completion_menu")
                .with_word_chars("_."),
        ));
        let mut reedline = Reedline::create()
            .with_completer(Box::new(AlwaysSuggests))
            .with_menu(completion_menu);
        reedline.painter.force_prompt_anchored_for_test(0);
        let prompt = DefaultPrompt::default();

        reedline.run_edit_commands(&[EditCommand::InsertString(String::from("show tab"))]);
        reedline
            .handle_event(
                &prompt,
                ReedlineEvent::Menu(String::from("completion_menu")),
            )
            .unwrap();
        assert!(menu_is_active(&reedline));

        for c in "les;".chars() {
            insert(&mut reedline, c);
        }
        assert!(!menu_is_active(&reedline), "';' ends the word");

        let status = reedline
            .handle_event(&prompt, ReedlineEvent::Enter)
            .unwrap();
        assert!(
            matches!(status, EventStatus::Exits(Signal::Success(ref s)) if s == "show tables;"),
            "the statement runs with no stray word appended"
        );
    }

    /// Engine with a completion menu open over "th" and partial completions on, so
    /// `MenuNext` reaches the completer through `can_partially_complete`.
    fn engine_with_partial_completion_menu() -> Reedline {
        let completer = Box::new(DefaultCompleter::new_with_wordlen(
            vec![String::from("this"), String::from("that")],
            1,
        ));
        let completion_menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("completion_menu"),
        ));
        let mut reedline = Reedline::create()
            .with_completer(completer)
            .with_menu(completion_menu)
            .with_partial_completions(true);

        reedline.run_edit_commands(&[EditCommand::InsertString(String::from("th"))]);
        reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Menu(String::from("completion_menu")),
            )
            .unwrap();
        assert!(menu_is_active(&reedline));
        reedline
    }

    /// The anchor cache exists to keep `cursor::position()` off the hot path (#1090),
    /// so an event may only spend that round-trip when it actually runs a host
    /// completer. Which events those are does not follow from whether the menu's
    /// selection moved: `MenuNext` splices a partial completion first, `MenuPrevious`
    /// does not. See #1130.
    #[rstest]
    #[case::next_queries_the_completer(ReedlineEvent::MenuNext, false)]
    #[case::previous_only_moves(ReedlineEvent::MenuPrevious, true)]
    fn menu_events_invalidate_the_anchor_only_when_they_query(
        #[case] event: ReedlineEvent,
        #[case] stays_verified: bool,
    ) {
        let mut reedline = engine_with_partial_completion_menu();
        reedline.painter.force_prompt_anchored_for_test(0);

        reedline
            .handle_event(&DefaultPrompt::default(), event)
            .unwrap();

        assert_eq!(
            reedline.painter.prompt_anchor_is_verified_for_test(),
            stays_verified
        );
    }

    fn send_edit(reedline: &mut Reedline, command: EditCommand) {
        reedline
            .handle_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Edit(vec![command]),
            )
            .unwrap();
    }

    #[rstest]
    #[case::up_at_first_line(
        EditCommand::MoveLineUp { select: false },
        0,
        "!a\nb",
        1
    )]
    #[case::up_from_second_line(
        EditCommand::MoveLineUp { select: false },
        3,
        "a\nb",
        1
    )]
    #[case::down_at_last_line(
        EditCommand::MoveLineDown { select: false },
        3,
        "a\nb!",
        4
    )]
    #[case::down_from_first_line(
        EditCommand::MoveLineDown { select: false },
        0,
        "a\nb",
        2
    )]
    #[case::left_at_buffer_start(EditCommand::MoveLeft { select: false }, 0, "!a\nb", 1)]
    #[case::right_from_buffer_start(EditCommand::MoveRight { select: false }, 0, "a\nb", 1)]
    #[case::line_start_at_line_start(
        EditCommand::MoveToLineStart { select: false },
        2,
        "a\n!b",
        3
    )]
    #[case::line_end_from_line_start(
        EditCommand::MoveToLineEnd { select: false },
        0,
        "a\nb",
        1
    )]
    #[case::extend_missing_find(
        EditCommand::Extend(MotionTarget::Find {
            ch: 'z',
            direction: Direction::Forward,
            stop: FindStop::On,
        }),
        0,
        "!a\nb",
        1
    )]
    fn until_found_move_status(
        #[case] command: EditCommand,
        #[case] start: usize,
        #[case] expected_buffer: &str,
        #[case] expected_cursor: usize,
    ) {
        let mut reedline = Reedline::create();
        reedline.run_edit_commands(&[
            EditCommand::InsertString("a\nb".into()),
            EditCommand::MoveToPosition {
                position: start,
                select: false,
            },
        ]);

        let status = send(
            &mut reedline,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Edit(vec![command]),
                ReedlineEvent::Edit(vec![EditCommand::InsertChar('!')]),
            ]),
        );

        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(reedline.current_buffer_contents(), expected_buffer);
        assert_eq!(reedline.editor.insertion_point(), expected_cursor);
    }

    #[rstest]
    #[case(ReedlineEvent::Left, 0, "!a", 1)]
    #[case(ReedlineEvent::Left, 1, "a", 0)]
    #[case(ReedlineEvent::Right, 0, "a", 1)]
    #[case(ReedlineEvent::Right, 1, "a!", 2)]
    #[case(ReedlineEvent::ToStart, 0, "!a", 1)]
    #[case(ReedlineEvent::ToStart, 1, "a", 0)]
    #[case(ReedlineEvent::ToEnd, 0, "a", 1)]
    #[case(ReedlineEvent::ToEnd, 1, "a!", 2)]
    fn until_found_horizontal_alias_status(
        #[case] event: ReedlineEvent,
        #[case] start: usize,
        #[case] expected: &str,
        #[case] expected_cursor: usize,
    ) {
        let mut reedline = Reedline::create();
        reedline.run_edit_commands(&[
            EditCommand::InsertChar('a'),
            EditCommand::MoveToPosition {
                position: start,
                select: false,
            },
        ]);
        let status = send(
            &mut reedline,
            ReedlineEvent::UntilFound(vec![
                event,
                ReedlineEvent::Edit(vec![EditCommand::InsertChar('!')]),
            ]),
        );
        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(reedline.current_buffer_contents(), expected);
        assert_eq!(reedline.editor.insertion_point(), expected_cursor);
    }

    #[rstest]
    #[case(ReedlineEvent::ToStart, 0)]
    #[case(ReedlineEvent::ToEnd, 3)]
    fn buffer_edge_alias_preserves_history_traversal(
        #[case] event: ReedlineEvent,
        #[case] destination: usize,
        #[values(0, 3)] start: usize,
    ) {
        let mut reedline = Reedline::create();
        reedline
            .history
            .save(HistoryItem::from_command_line("abc"))
            .unwrap();
        reedline.previous_history().unwrap();
        reedline.editor.line_buffer_mut().set_insertion_point(start);

        let status = send(&mut reedline, event);

        assert_eq!(matches!(status, EventStatus::Handled), start != destination);
        assert_eq!(reedline.editor.insertion_point(), destination);
        assert_eq!(reedline.input_mode, InputMode::HistoryTraversal);
    }

    #[rstest]
    #[case(ReedlineEvent::ToStart)]
    #[case(ReedlineEvent::ToEnd)]
    fn buffer_edge_alias_preserves_redo(
        #[case] event: ReedlineEvent,
        #[values(0, 3)] start: usize,
    ) {
        let mut reedline = Reedline::create();
        reedline.run_edit_commands(&[
            EditCommand::InsertString("abc".into()),
            EditCommand::InsertString("x".into()),
            EditCommand::Undo,
        ]);
        assert_eq!(reedline.current_buffer_contents(), "abc");
        reedline.editor.line_buffer_mut().set_insertion_point(start);

        send(&mut reedline, event);
        reedline.run_edit_commands(&[EditCommand::Redo]);

        assert_eq!(reedline.current_buffer_contents(), "abcx");
    }

    #[rstest]
    #[case(ReedlineEvent::ToStart, 1, 0)]
    #[case(ReedlineEvent::ToEnd, 0, 1)]
    fn buffer_edge_alias_clearing_selection_is_handled(
        #[case] event: ReedlineEvent,
        #[case] anchor: usize,
        #[case] head: usize,
    ) {
        let mut reedline = Reedline::create();
        reedline.run_edit_commands(&[
            EditCommand::InsertChar('a'),
            EditCommand::MoveToPosition {
                position: anchor,
                select: false,
            },
            EditCommand::MoveToPosition {
                position: head,
                select: true,
            },
        ]);

        assert!(matches!(send(&mut reedline, event), EventStatus::Handled));
        assert_eq!(reedline.editor.insertion_point(), head);
        assert_eq!(reedline.editor.get_selection(), None);
    }

    #[rstest]
    #[case(EditCommand::MoveLeft { select: false })]
    #[case(EditCommand::MoveLineUp { select: false })]
    #[case(EditCommand::MoveLineDown { select: false })]
    fn until_found_noop_move_preserves_redo(#[case] command: EditCommand) {
        let mut reedline = Reedline::create();
        reedline.run_edit_commands(&[EditCommand::InsertString("abc".into()), EditCommand::Undo]);
        assert_eq!(reedline.current_buffer_contents(), "");

        let status = send(
            &mut reedline,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Edit(vec![command]),
                ReedlineEvent::Edit(vec![EditCommand::Redo]),
            ]),
        );

        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(reedline.current_buffer_contents(), "abc");
    }

    #[test]
    fn until_found_noop_move_preserves_history_traversal() {
        let mut reedline = Reedline::create();
        for entry in ["first", "second"] {
            reedline
                .history
                .save(HistoryItem::from_command_line(entry))
                .unwrap();
        }

        for (command, fallback, expected) in [
            (
                EditCommand::MoveLineUp { select: false },
                ReedlineEvent::PreviousHistory,
                ["second", "first"],
            ),
            (
                EditCommand::MoveLineDown { select: false },
                ReedlineEvent::NextHistory,
                ["second", ""],
            ),
        ] {
            for buffer in expected {
                let status = send(
                    &mut reedline,
                    ReedlineEvent::UntilFound(vec![
                        ReedlineEvent::Edit(vec![command.clone()]),
                        fallback.clone(),
                    ]),
                );
                assert!(matches!(status, EventStatus::Handled));
                assert_eq!(reedline.current_buffer_contents(), buffer);
            }
        }
    }

    fn history_jump_engine() -> Reedline {
        let mut reedline = Reedline::create();
        for entry in ["echo hi", "git status", "ls", "git log"] {
            reedline
                .history
                .save(HistoryItem::from_command_line(entry))
                .unwrap();
        }
        reedline
    }

    #[test]
    fn first_history_jumps_to_the_oldest_entry() {
        let mut reedline = history_jump_engine();

        assert!(matches!(
            send(&mut reedline, ReedlineEvent::FirstHistoryEntry),
            EventStatus::Handled
        ));
        assert_eq!(reedline.current_buffer_contents(), "echo hi");
        assert!(matches!(
            send(&mut reedline, ReedlineEvent::FirstHistoryEntry),
            EventStatus::Inapplicable
        ));
        assert_eq!(reedline.current_buffer_contents(), "echo hi");

        send(&mut reedline, ReedlineEvent::NextHistory);
        assert_eq!(reedline.current_buffer_contents(), "git status");
    }

    #[test]
    fn first_history_honors_the_typed_prefix_and_last_history_restores_it() {
        let mut reedline = history_jump_engine();
        reedline.run_edit_commands(&[EditCommand::InsertString("git".into())]);

        send(&mut reedline, ReedlineEvent::FirstHistoryEntry);
        assert_eq!(reedline.current_buffer_contents(), "git status");

        assert!(matches!(
            send(&mut reedline, ReedlineEvent::LastHistoryEntry),
            EventStatus::Handled
        ));
        assert_eq!(reedline.current_buffer_contents(), "git");
        assert!(matches!(
            send(&mut reedline, ReedlineEvent::LastHistoryEntry),
            EventStatus::Inapplicable
        ));
    }

    #[test]
    fn last_history_restores_the_draft_after_walking_back() {
        let mut reedline = history_jump_engine();
        reedline.run_edit_commands(&[
            EditCommand::InsertString("draft".into()),
            EditCommand::MoveToStart { select: false },
        ]);

        send(&mut reedline, ReedlineEvent::PreviousHistory);
        send(&mut reedline, ReedlineEvent::PreviousHistory);
        assert_eq!(reedline.current_buffer_contents(), "ls");

        send(&mut reedline, ReedlineEvent::LastHistoryEntry);
        assert_eq!(reedline.current_buffer_contents(), "draft");
    }

    #[test]
    fn first_history_without_a_match_leaves_the_line_alone() {
        let mut reedline = history_jump_engine();
        reedline.run_edit_commands(&[EditCommand::InsertString("zzz".into())]);

        assert!(matches!(
            send(&mut reedline, ReedlineEvent::FirstHistoryEntry),
            EventStatus::Inapplicable
        ));
        assert_eq!(reedline.current_buffer_contents(), "zzz");
        assert!(matches!(
            send(&mut reedline, ReedlineEvent::LastHistoryEntry),
            EventStatus::Inapplicable
        ));
    }

    // GNU `beginning-of-history` shows the oldest stored entry even while Up
    // is recalling the excluded last run.
    #[test]
    fn first_history_leaves_the_recalled_last_run_for_the_oldest_entry() {
        let mut rl = seam_engine(Box::new(crate::Emacs::default()))
            .with_history_exclusion_prefix(Some(" ".into()));
        rl.history
            .save(HistoryItem::from_command_line("echo hi"))
            .unwrap();
        drive_until_signal(&mut rl, &[ch(' '), ch('l'), ch('s'), key(KeyCode::Enter)]);

        drive(&mut rl, &[key(KeyCode::Up)]);
        assert_eq!(
            rl.editor.get_buffer(),
            " ls",
            "Up recalls the excluded line"
        );

        assert!(matches!(
            send(&mut rl, ReedlineEvent::FirstHistoryEntry),
            EventStatus::Handled
        ));
        assert_eq!(rl.editor.get_buffer(), "echo hi");
    }

    #[test]
    fn until_found_noop_move_that_clears_selection_is_applied() {
        let mut reedline = Reedline::create();
        reedline.run_edit_commands(&[
            EditCommand::InsertString("a\nb".into()),
            EditCommand::MoveToPosition {
                position: 1,
                select: false,
            },
            EditCommand::MoveToStart { select: true },
        ]);

        send(
            &mut reedline,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Edit(vec![EditCommand::MoveLineUp { select: false }]),
                ReedlineEvent::Edit(vec![EditCommand::InsertChar('!')]),
            ]),
        );

        assert_eq!(reedline.current_buffer_contents(), "a\nb");
        assert_eq!(reedline.editor.insertion_point(), 0);
        assert_eq!(reedline.editor.get_selection(), None);
    }

    #[rstest]
    #[case::local_without_selection(EditCommand::CopySelection, false)]
    #[case::local_with_selection(EditCommand::CopySelection, true)]
    #[cfg_attr(
        feature = "system_clipboard",
        case::system_without_selection(EditCommand::CopySelectionSystem, false)
    )]
    #[cfg_attr(
        feature = "system_clipboard",
        case::system_with_selection(EditCommand::CopySelectionSystem, true)
    )]
    fn until_found_copy_selection_status(
        #[case] command: EditCommand,
        #[case] with_selection: bool,
    ) {
        let mut reedline = Reedline::create();
        let mut setup = vec![EditCommand::InsertString("abc".into())];

        if with_selection {
            setup.extend([
                EditCommand::MoveToStart { select: false },
                EditCommand::MoveRight { select: true },
            ]);
        }

        reedline.run_edit_commands(&setup);

        let status = send(
            &mut reedline,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Edit(vec![command]),
                ReedlineEvent::CtrlC,
            ]),
        );

        if with_selection {
            assert!(matches!(status, EventStatus::Handled));
        } else {
            assert!(matches!(status, EventStatus::Exits(Signal::CtrlC)));
        }
    }

    #[test]
    fn edit_status_is_applied_when_any_command_applies() {
        let mut reedline = Reedline::create();

        let status = send(
            &mut reedline,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Edit(vec![
                    EditCommand::InsertString("abc".into()),
                    EditCommand::CopySelection,
                ]),
                ReedlineEvent::CtrlC,
            ]),
        );

        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(reedline.current_buffer_contents(), "abc");
    }

    #[rstest]
    #[case(false, false)]
    #[case(false, true)]
    #[case(true, false)]
    #[case(true, true)]
    fn test_menu_persistence_while_erasing(#[case] quick: bool, #[case] persistent: bool) {
        let mut reedline = engine_with_active_menu(quick, persistent);

        // quick completions close the menu on any backspace unless menus are persistent
        send_edit(&mut reedline, EditCommand::Backspace);
        assert_eq!(reedline.current_buffer_contents(), "t");
        assert_eq!(menu_is_active(&reedline), persistent || !quick);

        // emptying the buffer closes the menu unless menus are persistent
        send_edit(&mut reedline, EditCommand::Backspace);
        assert!(reedline.current_buffer_contents().is_empty());
        assert_eq!(menu_is_active(&reedline), persistent);
    }

    #[rstest]
    #[case(EditCommand::BackspaceWord)]
    #[case(EditCommand::MoveToLineStart { select: false })]
    fn test_menu_persistence_covers_all_quick_dismissal_commands(#[case] command: EditCommand) {
        for persistent in [false, true] {
            let mut reedline = engine_with_active_menu(true, persistent);
            send_edit(&mut reedline, command.clone());
            assert_eq!(menu_is_active(&reedline), persistent);
        }
    }

    #[rstest]
    #[case(EditCommand::MoveToLineStart { select: false }, true)]
    #[case(EditCommand::MoveLeft { select: false }, false)]
    #[case(EditCommand::MoveLineUp { select: false }, false)]
    #[case(EditCommand::MoveLineDown { select: false }, false)]
    fn noop_moves_only_dismiss_eligible_quick_menus(
        #[case] command: EditCommand,
        #[case] can_dismiss: bool,
        #[values(false, true)] quick: bool,
        #[values(false, true)] persistent: bool,
    ) {
        let mut reedline = engine_with_active_menu(quick, persistent);
        reedline.run_edit_commands(&[EditCommand::MoveToLineStart { select: false }]);
        assert!(menu_is_active(&reedline));

        let status = send(&mut reedline, ReedlineEvent::Edit(vec![command]));

        let dismissed = can_dismiss && quick && !persistent;
        assert_eq!(menu_is_active(&reedline), !dismissed);
        assert_eq!(matches!(status, EventStatus::Handled), dismissed);
        assert_eq!(reedline.current_buffer_contents(), "th");
        assert_eq!(reedline.editor.insertion_point(), 0);
    }

    fn send(reedline: &mut Reedline, event: ReedlineEvent) -> EventStatus {
        reedline
            .handle_event(&DefaultPrompt::default(), event)
            .unwrap()
    }

    /// Engine with a completion menu opened over "th" by a completer that has no
    /// word starting with it, so the menu is active but holds no values.
    fn engine_with_empty_menu() -> Reedline {
        let completer = Box::new(DefaultCompleter::new_with_wordlen(
            vec![String::from("xylophone")],
            1,
        ));
        let completion_menu = ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("completion_menu"),
        ));
        let mut reedline = Reedline::create()
            .with_completer(completer)
            .with_menu(completion_menu)
            .with_quick_completions(true);

        reedline.run_edit_commands(&[EditCommand::InsertString(String::from("th"))]);
        send(
            &mut reedline,
            ReedlineEvent::Menu(String::from("completion_menu")),
        );
        assert!(menu_is_active(&reedline), "setup");
        assert!(reedline.menus[0].get_values().is_empty(), "setup");
        reedline
    }

    #[test]
    fn menu_accept_splices_the_selection_and_closes_the_menu() {
        let mut reedline = engine_with_active_menu(true, false);

        let status = send(&mut reedline, ReedlineEvent::MenuAccept);

        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(reedline.current_buffer_contents(), "that");
        assert!(!menu_is_active(&reedline));
    }

    #[test]
    fn menu_accept_without_an_open_menu_is_inapplicable() {
        let mut reedline = Reedline::create();
        reedline.run_edit_commands(&[EditCommand::InsertString(String::from("th"))]);

        let status = send(&mut reedline, ReedlineEvent::MenuAccept);

        assert!(matches!(status, EventStatus::Inapplicable));
        assert_eq!(reedline.current_buffer_contents(), "th");
    }

    /// Nothing to splice, so the keypress is not spent on closing the menu; the
    /// event falls through to whatever the binding lists next.
    #[test]
    fn menu_accept_over_an_empty_menu_is_inapplicable_and_leaves_it_open() {
        let mut reedline = engine_with_empty_menu();

        let status = send(&mut reedline, ReedlineEvent::MenuAccept);

        assert!(matches!(status, EventStatus::Inapplicable));
        assert!(menu_is_active(&reedline));
        assert_eq!(reedline.current_buffer_contents(), "th");
    }

    /// A *stale* answer is refused downstream — its span belongs to the line it
    /// was answered from — so there is nothing to splice and the keypress is not
    /// spent closing the menu over a completion that never happened.
    #[test]
    fn menu_accept_over_a_stale_menu_is_inapplicable_and_leaves_it_open() {
        let (mut reedline, _) = activate_menu_over(
            Box::new(DeferredCompleter::stale("console", "co", &["crates"])),
            "cr",
            true,
            false,
        );
        assert!(menu_is_active(&reedline), "setup");
        assert!(
            reedline.menus[0].results_are_provisional(),
            "setup: the answer is stale"
        );

        let status = send(&mut reedline, ReedlineEvent::MenuAccept);

        assert!(matches!(status, EventStatus::Inapplicable));
        assert!(menu_is_active(&reedline));
        assert_eq!(reedline.current_buffer_contents(), "cr");
    }

    /// The same refusal seen from `Enter`: the line runs as typed rather than
    /// the key dying against a menu that could not have accepted anything.
    #[test]
    fn enter_over_a_stale_menu_runs_the_line() {
        let (mut reedline, _) = activate_menu_over(
            Box::new(DeferredCompleter::stale("console", "co", &["crates"])),
            "cr",
            true,
            false,
        );
        reedline.painter.force_prompt_anchored_for_test(0);

        let status = reedline
            .handle_event(&DefaultPrompt::default(), ReedlineEvent::Enter)
            .unwrap();

        assert!(
            matches!(status, EventStatus::Exits(Signal::Success(ref line)) if line == "cr"),
            "the line ran as typed, with no stale value spliced into it"
        );
    }

    /// The binding this exists for: space accepts the highlighted completion and
    /// then types itself, and stays a plain space when no menu is open.
    #[test]
    fn menu_accept_chains_with_an_edit_as_a_space_binding() {
        let space_binding = || {
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::MenuAccept,
                ReedlineEvent::Edit(vec![EditCommand::InsertChar(' ')]),
            ])
        };

        let mut reedline = engine_with_active_menu(true, false);
        send(&mut reedline, space_binding());
        assert_eq!(reedline.current_buffer_contents(), "that ");
        assert!(!menu_is_active(&reedline));

        let mut reedline = Reedline::create();
        reedline.run_edit_commands(&[EditCommand::InsertString(String::from("th"))]);
        send(&mut reedline, space_binding());
        assert_eq!(reedline.current_buffer_contents(), "th ");
    }

    /// The same pair under `UntilFound` is "accept, or else type a space": the
    /// chain stops at the first event that applies, so an accepted completion is
    /// not followed by the space, and a menu with nothing to accept is skipped
    /// over like no menu at all.
    #[test]
    fn menu_accept_under_until_found_yields_to_the_next_event_only_when_inapplicable() {
        let space_binding = || {
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::MenuAccept,
                ReedlineEvent::Edit(vec![EditCommand::InsertChar(' ')]),
            ])
        };

        let mut reedline = engine_with_active_menu(true, false);
        send(&mut reedline, space_binding());
        assert_eq!(reedline.current_buffer_contents(), "that");
        assert!(!menu_is_active(&reedline));

        let mut reedline = Reedline::create();
        reedline.run_edit_commands(&[EditCommand::InsertString(String::from("th"))]);
        send(&mut reedline, space_binding());
        assert_eq!(reedline.current_buffer_contents(), "th ");

        let mut reedline = engine_with_empty_menu();
        send(&mut reedline, space_binding());
        assert_eq!(reedline.current_buffer_contents(), "th ");
    }

    /// A hinter that always offers a fixed suggestion, so the completion flow can
    /// be driven without the paint cycle that normally refreshes the hint.
    struct FixedHinter(&'static str);
    impl Hinter for FixedHinter {
        fn handle(&mut self, _: &str, _: usize, _: &dyn History, _: bool, _: &str) -> String {
            self.0.to_string()
        }
        fn complete_hint(&self) -> String {
            self.0.to_string()
        }
        fn next_hint_token(&self) -> String {
            self.0.to_string()
        }
    }

    /// Paints `echo ` with a host hinter offering `hint`, returning the engine
    /// and what reached the terminal. A host hinter, not one of ours: the
    /// engine escapes whatever it returns.
    fn paint_with_hint(hint: &'static str, use_ansi_coloring: bool) -> (Reedline, String) {
        let mut rl = Reedline::create()
            .with_ansi_colors(use_ansi_coloring)
            .with_hinter(Box::new(FixedHinter(hint)));
        // A capturing painter, anchored as `seam_engine` anchors its sink.
        rl.painter = Painter::new(W::capture());
        rl.painter.force_prompt_anchored_for_test(0);
        rl.run_edit_commands(&[EditCommand::InsertString("echo ".into())]);
        rl.repaint(&DefaultPrompt::default()).unwrap();
        let painted = String::from_utf8_lossy(rl.painter.captured_for_test()).into_owned();
        (rl, painted)
    }

    #[test]
    fn a_hint_shows_its_escape_sequences_but_completes_them_raw() {
        let (mut rl, painted) = paint_with_hint("\x1b[2Khi", true);
        assert!(painted.contains("^[[2Khi"), "hint not shown: {painted:?}");
        assert!(!painted.contains("\x1b[2Khi"), "sent an erase: {painted:?}");

        send(&mut rl, ReedlineEvent::HistoryHintComplete);
        assert_eq!(rl.current_buffer_contents(), "echo \x1b[2Khi");
    }

    #[test]
    fn a_hint_keeps_its_sgr_only_while_coloring_is_on() {
        let (_, painted) = paint_with_hint("\x1b[8mhidden", true);
        assert!(
            painted.contains("\x1b[8mhidden"),
            "lost its style: {painted:?}"
        );

        let (_, painted) = paint_with_hint("\x1b[8mhidden", false);
        assert!(
            painted.contains("^[[8mhidden"),
            "hint not shown: {painted:?}"
        );
        assert!(!painted.contains("\x1b[8m"), "sent a conceal: {painted:?}");
    }

    fn vi_with_hint(hint: &'static str) -> Reedline {
        seam_engine(Box::<crate::Vi>::default()).with_hinter(Box::new(FixedHinter(hint)))
    }

    fn helix_with_hint(hint: &'static str) -> Reedline {
        seam_engine(Box::<crate::Helix>::default()).with_hinter(Box::new(FixedHinter(hint)))
    }

    #[test]
    fn vi_normal_history_hint_appends_at_buffer_end() {
        // The reported bug: a block caret rests on the last grapheme, so the
        // completion must append *after* it, not split it.
        let mut rl = vi_with_hint("def");
        rl.run_edit_commands(&[EditCommand::InsertString("abc".into())]);
        drive(&mut rl, &[key(KeyCode::Esc)]); // vi normal, caret on 'c' at the end
        rl.handle_event(
            &DefaultPrompt::default(),
            ReedlineEvent::HistoryHintComplete,
        )
        .unwrap();
        assert_eq!(rl.editor.get_buffer(), "abcdef");
    }

    #[test]
    fn vi_visual_selection_blocks_hint_completion() {
        // A hint completing over a selection would run through `delete_selection`
        // and clobber it — the empty-cursor guard must suppress it.
        let mut rl = vi_with_hint("def");
        rl.run_edit_commands(&[EditCommand::InsertString("abc".into())]);
        drive(&mut rl, &[key(KeyCode::Esc), ch('v')]); // visual: selection covers 'c' to len
        rl.handle_event(
            &DefaultPrompt::default(),
            ReedlineEvent::HistoryHintComplete,
        )
        .unwrap();
        assert_eq!(rl.editor.get_buffer(), "abc");
    }

    /// Helix normal rests as a min-width-1 block, not an empty point. The
    /// buffer-end guard must read that as a resting caret, not a selection, or
    /// a history hint never completes in normal mode.
    #[test]
    fn helix_normal_history_hint_appends_at_buffer_end() {
        let mut rl = helix_with_hint("def");
        type_each(&mut rl, &[ch('a'), ch('b'), ch('c'), key(KeyCode::Esc)]);
        assert!(
            !rl.editor.line_buffer().cursor().is_empty(),
            "setup: the helix block caret is a one-grapheme range"
        );
        rl.handle_event(
            &DefaultPrompt::default(),
            ReedlineEvent::HistoryHintComplete,
        )
        .unwrap();
        assert_eq!(rl.editor.get_buffer(), "abcdef");
    }

    /// Regression test for nushell/reedline#1185: `l` accepts the hint like
    /// Right does, and only where a hint applies; mid-line it still moves.
    #[test]
    fn helix_normal_l_accepts_hint_only_at_buffer_end() {
        let mut rl = helix_with_hint("def");
        type_each(&mut rl, &[ch('a'), ch('b'), ch('c'), key(KeyCode::Esc)]);
        type_each(&mut rl, &[ch('l')]);
        assert_eq!(rl.editor.get_buffer(), "abcdef");

        let mut rl = helix_with_hint("def");
        type_each(
            &mut rl,
            &[ch('a'), ch('b'), ch('c'), key(KeyCode::Esc), ch('h')],
        );
        let before = rl.editor.insertion_point();
        type_each(&mut rl, &[ch('l')]);
        assert_eq!(rl.editor.get_buffer(), "abc");
        assert_eq!(rl.editor.insertion_point(), before + 1);
    }

    /// A `v`-started helix selection is still protected, like vi visual.
    #[test]
    fn helix_select_selection_blocks_hint_completion() {
        let mut rl = helix_with_hint("def");
        type_each(
            &mut rl,
            &[ch('a'), ch('b'), ch('c'), key(KeyCode::Esc), ch('v')],
        );
        rl.handle_event(
            &DefaultPrompt::default(),
            ReedlineEvent::HistoryHintComplete,
        )
        .unwrap();
        assert_eq!(rl.editor.get_buffer(), "abc");
    }

    /// The line and position a hinter was last asked about.
    type Asked = Arc<std::sync::Mutex<(String, usize)>>;

    /// Offers `hint` whole and its first word the way `DefaultHinter` does
    /// (or `token`, when set), and records what it was last asked about.
    struct RecordingHinter {
        hint: &'static str,
        token: Option<&'static str>,
        continues_text_before_cursor: bool,
        asked: Asked,
    }
    impl RecordingHinter {
        fn new(hint: &'static str, asked: &Asked) -> Self {
            Self {
                hint,
                token: None,
                continues_text_before_cursor: true,
                asked: Arc::clone(asked),
            }
        }
    }
    impl Hinter for RecordingHinter {
        fn handle(&mut self, line: &str, pos: usize, _: &dyn History, _: bool, _: &str) -> String {
            *self.asked.lock().unwrap() = (line.to_string(), pos);
            self.hint.to_string()
        }
        fn complete_hint(&self) -> String {
            self.hint.to_string()
        }
        fn next_hint_token(&self) -> String {
            self.token
                .map_or_else(|| crate::hinter::get_first_token(self.hint), str::to_owned)
        }
        fn continues_text_before_cursor(&self) -> bool {
            self.continues_text_before_cursor
        }
    }

    /// Answers `hint_closer_offsets` with a fixed table, whatever the line,
    /// instead of parsing a language.
    struct ClosingOffsets(Option<Vec<usize>>);
    impl Highlighter for ClosingOffsets {
        fn highlight(&self, line: &str, _cursor: usize) -> crate::StyledText {
            let mut text = crate::StyledText::new();
            text.push((nu_ansi_term::Style::new(), line.to_string()));
            text
        }

        fn hint_closer_offsets(&self, _: &str, _: usize, _: &str, _: &str) -> Option<Vec<usize>> {
            self.0.clone()
        }
    }

    /// An engine pairing `()` and offering `hinter`'s hint, after `typed` is
    /// typed in `edit_mode`, which auto-pairs each `(`.
    fn paired_line(
        edit_mode: Box<dyn EditMode>,
        typed: &str,
        hinter: RecordingHinter,
        highlighter: Box<dyn Highlighter>,
    ) -> Reedline {
        let mut rl = seam_engine(edit_mode)
            .with_auto_pairs(AutoPairs::new([('(', ')')]))
            .with_highlighter(highlighter)
            .with_hinter(Box::new(hinter));
        drive(&mut rl, &typed.chars().map(ch).collect::<Vec<_>>());
        rl
    }

    /// An emacs engine that hints `hint` with the closers' offsets in it given
    /// by `offsets`, after `typed` is typed.
    fn paired_line_with_hint(
        typed: &str,
        hint: &'static str,
        offsets: Option<&[usize]>,
    ) -> (Reedline, Asked) {
        let asked = Asked::default();
        let rl = paired_line(
            Box::<crate::Emacs>::default(),
            typed,
            RecordingHinter::new(hint, &asked),
            Box::new(ClosingOffsets(offsets.map(<[usize]>::to_vec))),
        );
        (rl, asked)
    }

    fn accept_hint(rl: &mut Reedline, whole: bool) {
        let event = if whole {
            ReedlineEvent::HistoryHintComplete
        } else {
            ReedlineEvent::HistoryHintWordComplete
        };
        rl.handle_event(&DefaultPrompt::default(), event).unwrap();
    }

    fn buffer_and_cursor(rl: &Reedline) -> (&str, usize) {
        (rl.editor.get_buffer(), rl.editor.insertion_point())
    }

    /// What one repaint writes to the terminal.
    fn repaint_output(rl: &mut Reedline) -> String {
        rl.painter = Painter::new(W::capture());
        rl.painter.force_prompt_anchored_for_test(0);
        rl.repaint(&DefaultPrompt::default()).unwrap();
        String::from_utf8_lossy(rl.painter.captured_for_test()).into_owned()
    }

    // With only auto-pair closers after the cursor, a hinter that continues the
    // text before the cursor is asked about that text alone: history holds
    // `(gstat).branch`, not `(gs)...`.
    #[test]
    fn history_hint_looks_up_the_text_before_trailing_closers() {
        let (_, asked) = paired_line_with_hint("(gs", "tat).branch", Some(&[3]));
        assert_eq!(*asked.lock().unwrap(), ("(gs".to_string(), 3));
    }

    // Any other hinter keeps being asked about the whole buffer, and its hint,
    // drawn after the buffer, is out of reach from before the closers.
    #[test]
    fn hinter_that_does_not_continue_the_text_gets_the_whole_buffer() {
        let asked = Asked::default();
        let mut hinter = RecordingHinter::new("tat).branch", &asked);
        hinter.continues_text_before_cursor = false;
        let mut rl = paired_line(
            Box::<crate::Emacs>::default(),
            "(gs",
            hinter,
            Box::new(ClosingOffsets(Some(vec![3]))),
        );
        assert_eq!(*asked.lock().unwrap(), ("(gs)".to_string(), 3));
        assert!(repaint_output(&mut rl).contains("branch"));

        accept_hint(&mut rl, true);

        assert_eq!(buffer_and_cursor(&rl), ("(gs)", 3));
    }

    // Without a highlighter that says how the hint closes them, the closers
    // are left alone: no hint is drawn over them and none is accepted.
    #[test]
    fn default_highlighter_leaves_trailing_closers_alone() {
        let asked = Asked::default();
        let mut rl = paired_line(
            Box::<crate::Emacs>::default(),
            "(gs",
            RecordingHinter::new("tat).branch", &asked),
            Box::<ExampleHighlighter>::default(),
        );
        assert!(!repaint_output(&mut rl).contains("branch"));

        accept_hint(&mut rl, true);

        assert_eq!(buffer_and_cursor(&rl), ("(gs)", 3));
    }

    // Accepting before trailing closers puts the hint in at the cursor: the
    // closers it closes, by the highlighter's offsets, are the ones in it, and
    // the rest stay after the cursor. A `)` inside a string closes nothing, so
    // it is never mistaken for the closer after the cursor.
    #[rstest]
    #[case::whole_hint("(gs", "tat).branch", &[3], true, "(gstat).branch", 14)]
    #[case::word_before_the_closer("(gs", "tat).branch", &[3], false, "(gstat)", 6)]
    #[case::word_is_the_closer("(gs", ") | lines", &[0], false, "(gs)", 4)]
    #[case::nested_whole("((a", ") c)", &[0, 3], true, "((a) c)", 7)]
    #[case::nested_word_keeps_the_outer("((a", ") c)", &[0, 3], false, "((a))", 4)]
    #[case::closer_in_a_string_whole("f(", r#"")")"#, &[3], true, r#"f(")")"#, 6)]
    #[case::closer_in_a_string_word(r#"f(""#, r#")")"#, &[2], false, r#"f("))"#, 4)]
    fn history_hint_accepts_before_trailing_closers(
        #[case] typed: &str,
        #[case] hint: &'static str,
        #[case] offsets: &[usize],
        #[case] whole: bool,
        #[case] buffer: &str,
        #[case] cursor: usize,
    ) {
        let (mut rl, _) = paired_line_with_hint(typed, hint, Some(offsets));

        accept_hint(&mut rl, whole);

        assert_eq!(buffer_and_cursor(&rl), (buffer, cursor));
    }

    // Offsets that do not hold for the hint are as good as none.
    #[rstest]
    #[case::too_few("(gs", "tat)", &[])]
    #[case::too_many("(gs", "tat)", &[3, 4])]
    #[case::not_the_closer("(gs", "tat)", &[2])]
    #[case::out_of_bounds("(gs", "tat)", &[9])]
    #[case::inside_a_character("(gs", "é)", &[1])]
    #[case::not_increasing("((a", "b))", &[2, 2])]
    fn history_hint_is_refused_for_unsound_offsets(
        #[case] typed: &str,
        #[case] hint: &'static str,
        #[case] offsets: &[usize],
    ) {
        let (mut rl, _) = paired_line_with_hint(typed, hint, Some(offsets));
        let (buffer, cursor) = (
            rl.editor.get_buffer().to_owned(),
            rl.editor.insertion_point(),
        );
        assert!(!repaint_output(&mut rl).contains(hint));

        accept_hint(&mut rl, true);

        assert_eq!(buffer_and_cursor(&rl), (buffer.as_str(), cursor));
    }

    // A word that is not the start of the hint cannot be told apart from the
    // closers' offsets, which index the hint.
    #[test]
    fn history_hint_word_must_start_the_hint() {
        let asked = Asked::default();
        let mut hinter = RecordingHinter::new("tat).branch", &asked);
        hinter.token = Some("branch");
        let mut rl = paired_line(
            Box::<crate::Emacs>::default(),
            "(gs",
            hinter,
            Box::new(ClosingOffsets(Some(vec![3]))),
        );

        accept_hint(&mut rl, false);

        assert_eq!(buffer_and_cursor(&rl), ("(gs)", 3));
    }

    // The exit path reprints what follows the cursor. While the hint is drawn
    // over the closers that is still the closers, and the paint submit makes
    // with hints hidden must not act on the hint the hinter still holds.
    #[test]
    fn trailing_closers_survive_the_exit_reprint() {
        // The text carries the highlighter's styling, so look for the closer.
        let keeps_the_closer = |rl: &Reedline| {
            rl.painter
                .exit_after_cursor_for_test()
                .is_some_and(|text| text.contains(')'))
        };
        let (mut rl, _) = paired_line_with_hint("(gs", "tat).branch", Some(&[3]));
        assert!(repaint_output(&mut rl).contains("branch"), "hint showing");
        assert!(keeps_the_closer(&rl), "hint showing");

        rl.hide_hints = true;
        rl.repaint(&DefaultPrompt::default()).unwrap();
        assert!(keeps_the_closer(&rl), "hints hidden");
    }

    // An open menu owns the area under the cursor and the painter draws no
    // hint with it, so the closers must stay in the line, not be left to a
    // hint that never shows.
    #[test]
    fn open_menu_keeps_trailing_closers_out_of_the_hint() {
        let (mut rl, asked) = paired_line_with_hint("(gs", "tat).branch", Some(&[3]));
        rl = rl.with_menu(ReedlineMenu::HistoryMenu(Box::new(
            ListMenu::default().with_name("history_menu"),
        )));
        rl.handle_event(
            &DefaultPrompt::default(),
            ReedlineEvent::Menu("history_menu".into()),
        )
        .unwrap();
        assert!(menu_is_active(&rl), "setup: menu is open");

        let output = repaint_output(&mut rl);
        assert_eq!(*asked.lock().unwrap(), ("(gs)".to_string(), 3));
        assert!(output.contains(')'), "closers dropped: {output:?}");

        accept_hint(&mut rl, true);
        assert_eq!(buffer_and_cursor(&rl), ("(gs)", 3));
    }

    #[test]
    fn one_undo_takes_back_a_hint_accepted_before_closers() {
        let (mut rl, _) = paired_line_with_hint("(gs", "tat).branch", Some(&[3]));
        accept_hint(&mut rl, true);

        rl.run_edit_commands(&[EditCommand::Undo]);

        assert_eq!(buffer_and_cursor(&rl), ("(gs)", 3));
    }

    // Vi normal rests a block caret on the closer itself; the word goes in
    // before it all the same.
    #[test]
    fn vi_normal_accepts_a_hint_word_before_trailing_closers() {
        let asked = Asked::default();
        let mut rl = paired_line(
            Box::<crate::Vi>::default(),
            "(gs",
            RecordingHinter::new("tat).branch", &asked),
            Box::new(ClosingOffsets(Some(vec![3]))),
        );
        type_each(&mut rl, &[key(KeyCode::Esc), ch('l')]);
        assert_eq!(
            buffer_and_cursor(&rl),
            ("(gs)", 3),
            "setup: caret on the closer"
        );

        accept_hint(&mut rl, false);

        assert_eq!(rl.editor.get_buffer(), "(gstat)");
    }

    #[test]
    fn selection_blocks_hint_completion_before_trailing_closers() {
        let (mut rl, _) = paired_line_with_hint("(gs", "tat).branch", Some(&[3]));
        rl.run_edit_commands(&[
            EditCommand::MoveLeft { select: false },
            EditCommand::MoveRight { select: true },
        ]);

        accept_hint(&mut rl, true);

        assert_eq!(rl.editor.get_buffer(), "(gs)");
    }

    /// A retained motion selection in helix *normal* (here `b` sweeping back
    /// over the word) is multi-grapheme, so it blocks completion even though
    /// normal is not a selection mode.
    #[test]
    fn helix_normal_motion_selection_blocks_hint_completion() {
        let mut rl = helix_with_hint("def");
        type_each(
            &mut rl,
            &[ch('a'), ch('b'), ch('c'), key(KeyCode::Esc), ch('b')],
        );
        let cursor = rl.editor.line_buffer().cursor();
        assert!(
            cursor.end() - cursor.start() > 1,
            "setup: `b` leaves a multi-grapheme selection standing"
        );
        rl.handle_event(
            &DefaultPrompt::default(),
            ReedlineEvent::HistoryHintComplete,
        )
        .unwrap();
        assert_eq!(rl.editor.get_buffer(), "abc");
    }

    /// The issue as filed pressed a key, not an event: `Right` reaches
    /// `HistoryHintComplete` through the keymap's `UntilFound` chain, so this
    /// covers the wiring the event-level tests above skip past.
    #[test]
    fn helix_normal_right_key_completes_the_hint() {
        let mut rl = helix_with_hint("def");
        type_each(&mut rl, &[ch('a'), ch('b'), ch('c'), key(KeyCode::Esc)]);
        drive(&mut rl, &[key(KeyCode::Right)]);
        assert_eq!(rl.editor.get_buffer(), "abcdef");
    }

    /// The prefix-search side of the same guard: helix normal's resting block
    /// still counts as "at buffer end", so `k` prefix-searches history like vi
    /// normal instead of walking it plainly.
    #[test]
    fn helix_normal_k_uses_prefix_search() {
        let mut rl = seam_engine(Box::<crate::Helix>::default());
        for entry in ["ls -la", "ls /tmp", "echo hi"] {
            rl.history
                .save(HistoryItem::from_command_line(entry))
                .unwrap();
        }
        type_each(&mut rl, &[ch('l'), ch('s'), key(KeyCode::Esc)]);
        drive(&mut rl, &[ch('k')]);
        assert_eq!(rl.editor.get_buffer(), "ls /tmp");
    }

    #[test]
    fn undo_removes_accepted_history_hint() {
        let mut rl = vi_with_hint("def");
        rl.run_edit_commands(&[EditCommand::InsertString("abc".into())]);
        drive(&mut rl, &[key(KeyCode::Esc)]);
        rl.handle_event(
            &DefaultPrompt::default(),
            ReedlineEvent::HistoryHintComplete,
        )
        .unwrap();
        assert_eq!(rl.editor.get_buffer(), "abcdef");
        rl.run_edit_commands(&[EditCommand::Undo]);
        assert_eq!(rl.editor.get_buffer(), "abc");
    }

    #[test]
    fn vi_normal_down_rests_on_last_grapheme() {
        // Down onto a shorter last line must rest *on* the last grapheme, not the
        // gap past it: `down_command` has to settle under the `OnGrapheme` policy.
        let mut rl = seam_engine(Box::<crate::Vi>::default());
        rl.run_edit_commands(&[EditCommand::InsertString("abc\nd".into())]); // a0 b1 c2 \n3 d4
        drive(&mut rl, &[key(KeyCode::Esc)]); // vi normal
        rl.run_edit_commands(&[
            EditCommand::MoveToStart { select: false },
            EditCommand::MoveRight { select: false },
            EditCommand::MoveRight { select: false },
        ]); // caret on 'c' (col 2 of line 1)
        rl.down_command().expect("history ok");
        assert_eq!(rl.editor.insertion_point(), 4); // on 'd', not 5 (past it)
    }

    #[test]
    fn vi_normal_to_end_rests_on_last_grapheme() {
        // `ReedlineEvent::ToEnd` — reachable through a custom binding; GNU's
        // vi maps leave `M->` unbound and emacs binds it to end-of-history —
        // must land on the last char, not the gap past it.
        let mut rl = seam_engine(Box::<crate::Vi>::default());
        rl.run_edit_commands(&[EditCommand::InsertString("abc".into())]);
        drive(&mut rl, &[key(KeyCode::Esc)]);
        rl.run_edit_commands(&[EditCommand::MoveToStart { select: false }]); // 'a'
        assert!(matches!(
            send(&mut rl, ReedlineEvent::ToEnd),
            EventStatus::Handled
        ));
        assert_eq!(rl.editor.insertion_point(), 2); // 'c', not 3 (past it)
        assert!(matches!(
            send(&mut rl, ReedlineEvent::ToEnd),
            EventStatus::Inapplicable
        ));
    }

    #[test]
    fn vi_normal_single_line_down_rests_on_last_grapheme() {
        // Single-line buffer: `down` hits the last line and routes to history nav,
        // which positions the cursor outside the command path. It must still
        // settle on the last grapheme, not the gap past it.
        let mut rl = seam_engine(Box::<crate::Vi>::default());
        rl.run_edit_commands(&[EditCommand::InsertString("abc".into())]);
        drive(&mut rl, &[key(KeyCode::Esc)]); // vi normal, on 'c'
        rl.down_command().expect("history ok"); // last line -> next_history (no forward entry -> draft)
        assert_eq!(rl.editor.insertion_point(), 2); // 'c', not 3 (past it)
    }

    #[test]
    fn vi_normal_end_of_line_rests_on_last_grapheme() {
        // `$` on an interior line lands ON the last char, not the gap before `\n`.
        let mut rl = seam_engine(Box::<crate::Vi>::default());
        rl.run_edit_commands(&[EditCommand::InsertString("abc\ndef".into())]); // a0 b1 c2 \n3
        drive(&mut rl, &[key(KeyCode::Esc)]);
        rl.run_edit_commands(&[
            EditCommand::MoveToStart { select: false },
            EditCommand::MoveToLineEnd { select: false },
        ]);
        assert_eq!(rl.editor.insertion_point(), 2); // 'c', not 3 (the newline gap)
    }

    #[test]
    fn vi_normal_k_uses_prefix_search() {
        // `j`/`k` in vi normal mode should use prefix search instead of plain
        // history traversal
        let mut rl = seam_engine(Box::<crate::Vi>::default());

        let success_cond = "ls /tmp";

        rl.history
            .save(HistoryItem::from_command_line("ls -la"))
            .unwrap();
        rl.history
            .save(HistoryItem::from_command_line(success_cond))
            .unwrap();
        rl.history
            .save(HistoryItem::from_command_line("echo hi"))
            .unwrap();

        type_each(&mut rl, &[ch('l'), ch('s'), key(KeyCode::Esc)]);
        drive(&mut rl, &[ch('k')]);

        assert_eq!(rl.editor.get_buffer(), success_cond);
    }

    #[test]
    fn vi_normal_k_off_end_uses_plain_walk() {
        // The complement of the prefix case: once the caret leaves the buffer
        // end (here `h` steps it onto 'l'), history nav falls back to plain
        // bash-style traversal and returns the most recent entry overall, not a
        // prefix match.
        let mut rl = seam_engine(Box::<crate::Vi>::default());

        rl.history
            .save(HistoryItem::from_command_line("ls -la"))
            .unwrap();
        rl.history
            .save(HistoryItem::from_command_line("ls /tmp"))
            .unwrap();
        rl.history
            .save(HistoryItem::from_command_line("echo hi"))
            .unwrap();

        type_each(&mut rl, &[ch('l'), ch('s'), key(KeyCode::Esc)]);
        drive(&mut rl, &[ch('h')]); // caret off the end, onto 'l'
        drive(&mut rl, &[ch('k')]);

        assert_eq!(rl.editor.get_buffer(), "echo hi");
    }

    #[test]
    fn vi_hl_cross_newline_at_engine_seam() {
        // `h`/`l` keys, driven through the vi parser, cross the line terminator
        // under the default cross-line policy. Buffer "ab\ncd".
        let mut rl = seam_engine(Box::<crate::Vi>::default());
        rl.run_edit_commands(&[EditCommand::InsertString("ab\ncd".into())]);
        drive(&mut rl, &[key(KeyCode::Esc)]); // vi normal
        rl.run_edit_commands(&[EditCommand::MoveToLineStart { select: false }]);
        assert_eq!(rl.editor.insertion_point(), 3); // 'c', start of line 2
        drive(&mut rl, &[ch('h')]); // crosses up to 'b' (end of line 1)
        assert_eq!(rl.editor.insertion_point(), 1);
        drive(&mut rl, &[ch('l')]); // crosses down to 'c' (start of line 2)
        assert_eq!(rl.editor.insertion_point(), 3);
    }

    fn command_from_strs(command: &[&str], current_dir: impl AsRef<Path>) -> Command {
        let (program, args) = command.split_first().unwrap();
        let mut command = Command::new(program);
        command.args(args);
        command.current_dir(current_dir);
        command.env("REEDLINE_TEST_KEEP", "kept");
        command.env_remove("REEDLINE_TEST_DROP");
        command
    }

    fn command_into_string(command: Command) -> String {
        use std::iter::once;

        once(command.get_program())
            .chain(command.get_args())
            .map(|os_str| os_str.to_str().unwrap())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[rstest]
    #[case(&["nano"], "nano foo.rs", false)]
    #[case(&["nano", "file.rs"], "nano file.rs foo.rs", false)]
    #[case(&["vim", "foo.rs"], "vim foo.rs", false)]
    #[case(&["code", "--goto", "{file}:{line}:{col}"], "code --goto foo.rs:2:4", true)]
    #[case(&["hx", "{file}:{line}:{col}"], "hx foo.rs:2:4", true)]
    #[case(&["nvim", "{file}", "\"call cursor({line}, {col})\""], "nvim foo.rs \"call cursor(2, 4)\"", true)]
    #[case(&["vim", "+{line}", "{file}"], "vim +2 foo.rs", true)]
    #[case(&["emacs", "+{line}:{col}", "{file}"], "emacs +2:4 foo.rs", true)]
    #[case(&["emacs", "+{line}:{col}"], "emacs +2:4 foo.rs", true)]
    #[case(&["emacs", "+{line}:{col}", "foo.rs"], "emacs +2:4 foo.rs", true)]
    fn render_editor_command_with_template(
        #[case] command: &[&str],
        #[case] expected: &str,
        #[case] is_template: bool,
    ) {
        let current_dir = "test-dir";

        let line_editor = Reedline::create().with_buffer_editor(
            command_from_strs(command, current_dir),
            PathBuf::from("foo.rs"),
        );
        let buffer_editor = line_editor.buffer_editor.as_ref().unwrap();

        assert_eq!(buffer_editor.is_template, is_template);

        let line_buffer = {
            let mut line_buffer = LineBuffer::new();
            line_buffer.insert_str("a mulatto\n");
            line_buffer.insert_str("an albino\n");
            line_buffer.insert_str("a mosquito\n");
            line_buffer.insert_str("my libido\n");
            line_buffer.move_line_up();
            line_buffer.move_line_up();
            line_buffer.move_left_before(' ', false);
            line_buffer
        };

        let actual = buffer_editor.render_command(&line_buffer);

        let current_dir = current_dir.as_ref();
        assert_eq!(actual.get_current_dir(), Some(current_dir));

        // nushell hands its `$env` to the editor through `envs`, so the
        // rendered command has to keep both overrides and removals.
        let envs: Vec<_> = actual.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("REEDLINE_TEST_KEEP"), Some(OsStr::new("kept")))));
        assert!(envs.contains(&(OsStr::new("REEDLINE_TEST_DROP"), None)));

        assert_eq!(command_into_string(actual), expected);
    }
    // === GNU kill ring + yank-pop (kill.c, readline.c:715-716) ===

    /// Type a string one `ch` event at a time, each its own batch.
    fn type_str(rl: &mut Reedline, s: &str) {
        type_each(rl, &s.chars().map(ch).collect::<Vec<_>>());
    }

    /// `rl_yank` (kill.c:509): C-y inserts the ring's current slot — the
    /// newest kill — at the caret.
    #[test]
    fn yank_inserts_newest_kill() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        type_str(&mut rl, "one two");
        drive(&mut rl, &[ctrl('w')]);
        assert_eq!(rl.editor.get_buffer(), "one ");
        drive(&mut rl, &[ctrl('y')]);
        assert_eq!(rl.editor.get_buffer(), "one two");
    }

    /// `_rl_copy_to_kill_ring` (kill.c:131-142): consecutive backward kills
    /// prepend into the newest slot rather than taking their own.
    #[test]
    fn consecutive_backward_kills_merge_prepending() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        type_str(&mut rl, "one two");
        drive(&mut rl, &[ctrl('w'), ctrl('w')]);
        // "two" then "one " — merged into one slot, prepended.
        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.current_kill(), ("one two".to_string(), 1));
        // …and the merged kill yanks back whole.
        drive(&mut rl, &[ctrl('y')]);
        assert_eq!(rl.editor.get_buffer(), "one two");
    }

    /// A backward kill followed by a forward kill merges the same way —
    /// `from < to` appends after the old text (kill.c:146-148).
    #[test]
    fn backward_then_forward_kill_merge_around_point() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        type_str(&mut rl, "foo bar");
        drive(
            &mut rl,
            &[key(KeyCode::Left), key(KeyCode::Left), key(KeyCode::Left)],
        );
        // Caret between the words: C-w kills "foo " backward, M-d kills
        // "bar" forward — one slot, "foo bar".
        drive(&mut rl, &[ctrl('w'), alt('d')]);
        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.current_kill(), ("foo bar".to_string(), 1));
    }

    /// A dispatched non-kill edit command clears
    /// `_rl_last_command_was_kill` (readline.c:715-716) — the next kill
    /// takes a fresh slot.
    #[test]
    fn motion_breaks_kill_merge_chain() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        type_str(&mut rl, "one two");
        drive(&mut rl, &[ctrl('w'), key(KeyCode::Left), ctrl('w')]);
        assert_eq!(rl.editor.current_kill(), ("one".to_string(), 2));
    }

    /// A handled event that ran no `EditCommand` still counts as a
    /// dispatched command in GNU — Esc between two kills breaks the chain
    /// just like a motion does.
    #[test]
    fn non_edit_event_breaks_kill_merge_chain() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        type_str(&mut rl, "one two");
        drive(&mut rl, &[ctrl('w'), key(KeyCode::Esc), ctrl('w')]);
        assert_eq!(rl.editor.current_kill(), ("one ".to_string(), 2));
    }

    /// Kills separated by non-kill commands stack up as separate slots —
    /// the ring keeps them in kill order, newest last.
    #[test]
    fn separated_kills_stack_newest_last() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        for word in ["one", "two", "three"] {
            type_str(&mut rl, word);
            drive(&mut rl, &[ctrl('w')]);
        }
        assert_eq!(rl.editor.current_kill(), ("three".to_string(), 3));
    }

    /// `rl_yank_pop` (kill.c:527-556): M-y after a yank swaps the inserted
    /// text for the next-older slot, cycling, and wraps past the oldest
    /// slot back to the newest.
    #[test]
    fn yank_pop_cycles_and_wraps_the_ring() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        for word in ["aaa", "bbb", "ccc"] {
            type_str(&mut rl, word);
            drive(&mut rl, &[ctrl('w')]);
        }
        drive(&mut rl, &[ctrl('y')]);
        assert_eq!(rl.editor.get_buffer(), "ccc");
        drive(&mut rl, &[alt('y')]);
        assert_eq!(rl.editor.get_buffer(), "bbb");
        drive(&mut rl, &[alt('y')]);
        assert_eq!(rl.editor.get_buffer(), "aaa");
        drive(&mut rl, &[alt('y')]);
        assert_eq!(rl.editor.get_buffer(), "ccc");
    }

    /// `rl_yank_pop` verifies the last yanked text literally precedes
    /// point (STREQN, kill.c:538-542): an intervening edit invalidates
    /// the pop and it dings — here, does nothing.
    #[test]
    fn yank_pop_dings_after_intervening_edit() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        for word in ["aaa", "bbb"] {
            type_str(&mut rl, word);
            drive(&mut rl, &[ctrl('w')]);
        }
        drive(&mut rl, &[ctrl('y')]);
        assert_eq!(rl.editor.get_buffer(), "bbb");
        type_str(&mut rl, "x");
        drive(&mut rl, &[alt('y')]);
        assert_eq!(rl.editor.get_buffer(), "bbbx");
    }

    /// The `rl_last_func` gate (kill.c:531): `YankPop` applies only
    /// directly after `Yank`/`YankPop` — a handled non-edit event
    /// overwrites the record too.
    #[test]
    fn yank_pop_dings_after_non_edit_event() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        for word in ["aaa", "bbb"] {
            type_str(&mut rl, word);
            drive(&mut rl, &[ctrl('w')]);
        }
        drive(&mut rl, &[ctrl('y')]);
        assert_eq!(rl.editor.get_buffer(), "bbb");
        drive(&mut rl, &[key(KeyCode::Esc)]);
        drive(&mut rl, &[alt('y')]);
        assert_eq!(rl.editor.get_buffer(), "bbb");
    }

    /// M-y with no preceding yank at all is likewise a ding.
    #[test]
    fn yank_pop_without_yank_is_inapplicable() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        type_str(&mut rl, "aaa");
        drive(&mut rl, &[ctrl('w'), alt('y')]);
        assert_eq!(rl.editor.get_buffer(), "");
    }

    /// `rl_unix_filename_rubout` (kill.c:338-397): one pathname component
    /// backward — the component, then a component plus its `/` run.
    #[test]
    fn filename_rubout_kills_one_component() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        type_str(&mut rl, "foo/bar/baz");
        rl.run_edit_commands(&[EditCommand::CutFilenameLeft]);
        assert_eq!(rl.editor.get_buffer(), "foo/bar/");
        assert_eq!(rl.editor.current_kill().0, "baz");
        rl.run_edit_commands(&[EditCommand::CutFilenameLeft]);
        assert_eq!(rl.editor.get_buffer(), "foo/");
        // Consecutive kills merge — a backward kill prepends.
        assert_eq!(rl.editor.current_kill(), ("bar/baz".to_string(), 1));
    }

    /// A `/` run that follows only whitespace (or the buffer start) is
    /// taken wholesale — `rl_point = i + whitespace (c)` (kill.c:372).
    #[test]
    fn filename_rubout_takes_slash_runs_after_whitespace() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        type_str(&mut rl, "a /");
        rl.run_edit_commands(&[EditCommand::CutFilenameLeft]);
        assert_eq!(rl.editor.get_buffer(), "a ");

        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        type_str(&mut rl, "  //x");
        rl.run_edit_commands(&[EditCommand::CutFilenameLeft]);
        assert_eq!(rl.editor.get_buffer(), "  //");
        rl.run_edit_commands(&[EditCommand::CutFilenameLeft]);
        assert_eq!(rl.editor.get_buffer(), "  ");
    }

    /// `_rl_copy_word_as_kill` (kill.c:460-506): `copy-forward-word` feeds
    /// the ring without moving point or deleting anything.
    #[test]
    fn copy_forward_word_feeds_ring_without_deleting() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        type_str(&mut rl, "foo bar");
        drive(&mut rl, &[ctrl('a')]);
        rl.run_edit_commands(&[EditCommand::CopyWordRight]);
        assert_eq!(rl.editor.get_buffer(), "foo bar");
        assert_eq!(rl.editor.current_kill().0, "foo");
    }

    /// kill.c:131 — `rl_editing_mode == vi_mode` suppresses the merge: a
    /// consecutive vi kill replaces the newest slot outright instead of
    /// merging or pushing.
    #[test]
    fn vi_kills_replace_slot_instead_of_merging() {
        let mut rl = seam_engine(Box::<crate::Vi>::default());
        // vi starts in insert; Esc into normal, then `x` twice — each a
        // kill into the ring.
        type_str(&mut rl, "ab");
        drive(&mut rl, &[key(KeyCode::Esc)]);
        drive(&mut rl, &[ch('x'), ch('x')]);
        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.current_kill(), ("a".to_string(), 1));
    }
    // ---- GNU readline feature coverage ----

    fn seed_history(rl: &mut Reedline, entries: &[&str]) {
        for entry in entries {
            rl.history
                .save(HistoryItem::from_command_line(*entry))
                .expect("failed to save history");
        }
    }

    fn ctrl_alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL | KeyModifiers::ALT)
    }

    // --- revert-line (undo.c:331) ---

    #[test]
    fn revert_line_undoes_every_change() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('a'), ch('b'), ch('c')]);
        assert_eq!(rl.editor.get_buffer(), "abc");

        drive(&mut rl, &[alt('r')]);
        assert_eq!(rl.editor.get_buffer(), "");
    }

    #[test]
    fn revert_line_meta_ctrl_r_binds_too() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('x')]);

        drive(&mut rl, &[ctrl_alt('r')]);
        assert_eq!(rl.editor.get_buffer(), "");
    }

    /// `M-<`/`M->` are GNU beginning/end-of-history (emacs_keymap.c:399/:401),
    /// not buffer edges: `M-<` recalls the OLDEST entry, `M->` returns to the
    /// live line (empty buffer). T2 F2.
    #[test]
    fn meta_angle_brackets_traverse_history_edges() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        for line in ["one", "two", "three"] {
            rl.history
                .save(HistoryItem::from_command_line(line))
                .expect("history save ok");
        }
        // C-p recalls `three` (the newest entry).
        drive(&mut rl, &[ctrl('p')]);
        assert_eq!(rl.editor.get_buffer(), "three");
        // M-< jumps to the oldest entry, not the buffer start.
        drive(&mut rl, &[alt('<')]);
        assert_eq!(rl.editor.get_buffer(), "one");
        // M-> leaves history for the live line — an empty buffer, not the
        // buffer end of the recalled line.
        drive(&mut rl, &[alt('>')]);
        assert_eq!(rl.editor.get_buffer(), "");
    }

    #[test]
    fn revert_line_on_untouched_buffer_is_inapplicable() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        let status = rl
            .handle_editor_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Edit(vec![EditCommand::RevertLine]),
            )
            .unwrap();
        assert!(matches!(status, EventStatus::Inapplicable));
    }

    // --- character-search (text.c:1829) ---

    #[test]
    fn char_search_forward_lands_on_next_match() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('h'), ch('e'), ch('l'), ch('l'), ch('o')]);
        // C-a to the line start, then C-] 'l' in one batch — the armed
        // flag consumes the following Char at parse time.
        drive(&mut rl, &[ctrl('a')]);
        drive(&mut rl, &[ctrl(']'), ch('l')]);
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    #[test]
    fn char_search_backward_lands_on_previous_match() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('h'), ch('e'), ch('l'), ch('l'), ch('o')]);
        // Cursor at end; M-C-] 'l' lands on the last 'l' before point.
        drive(&mut rl, &[ctrl_alt(']'), ch('l')]);
        assert_eq!(rl.editor.insertion_point(), 3);
    }

    #[test]
    fn char_search_miss_leaves_cursor() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('a'), ch('b')]);
        drive(&mut rl, &[ctrl('a')]);
        drive(&mut rl, &[ctrl(']'), ch('z')]);
        assert_eq!(rl.editor.insertion_point(), 0);
    }

    #[test]
    fn char_search_skips_char_under_cursor() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('l'), ch('o'), ch('l')]);
        drive(&mut rl, &[ctrl('a')]);
        // Sitting on 'l' at 0, C-] 'l' must find the NEXT 'l' at 2.
        drive(&mut rl, &[ctrl(']'), ch('l')]);
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    // --- yank-last-arg / yank-nth-arg (kill.c:603-702) ---

    #[test]
    fn yank_last_arg_inserts_last_token_of_previous_entry() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["ls one two"]);

        drive(&mut rl, &[alt('.')]);
        assert_eq!(rl.editor.get_buffer(), "two");
    }

    #[test]
    fn yank_last_arg_appends_at_cursor() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["ls one two"]);
        type_each(&mut rl, &[ch('s'), ch('u'), ch('d'), ch('o'), ch(' ')]);

        drive(&mut rl, &[alt('.')]);
        assert_eq!(rl.editor.get_buffer(), "sudo two");
    }

    #[test]
    fn yank_last_arg_repeat_walks_older_entries() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["ls a b", "echo x y", "vim f g"]);

        drive(&mut rl, &[alt('.')]);
        assert_eq!(rl.editor.get_buffer(), "g");
        // Each repeat undoes the last insert and takes the last arg
        // of the next-older entry.
        drive(&mut rl, &[alt('.')]);
        assert_eq!(rl.editor.get_buffer(), "y");
        drive(&mut rl, &[alt('.')]);
        assert_eq!(rl.editor.get_buffer(), "b");
        // Past the oldest entry GNU dings — and because the repeat
        // undoes the previous yank BEFORE retrying the extract
        // (kill.c:663-665), the failed press leaves the arg removed.
        drive(&mut rl, &[alt('.')]);
        assert_eq!(rl.editor.get_buffer(), "");
    }

    #[test]
    fn yank_last_arg_chain_breaks_on_other_input() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["ls a b", "echo x y", "vim f g"]);

        drive(&mut rl, &[alt('.')]);
        assert_eq!(rl.editor.get_buffer(), "g");
        // Typing resets the walk: the next M-. starts at the newest
        // entry again.
        drive(&mut rl, &[ch(' ')]);
        drive(&mut rl, &[alt('.')]);
        assert_eq!(rl.editor.get_buffer(), "g g");
    }

    #[test]
    fn yank_nth_arg_inserts_indexed_arg() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["cmd arg1 arg2"]);

        drive(&mut rl, &[ctrl_alt('y')]);
        assert_eq!(rl.editor.get_buffer(), "arg1");
    }

    #[test]
    fn yank_last_arg_empty_history_dings() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        drive(&mut rl, &[alt('.')]);
        assert_eq!(rl.editor.get_buffer(), "");
    }

    // --- operate-and-get-next (misc.c:782) ---

    #[test]
    fn operate_and_get_next_preloads_following_entry() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["one", "two", "three"]);

        // Recall "two" (one above the newest), then C-o: the line
        // submits, and the next read_line preloads "three" — the entry
        // after the recalled position.
        drive(&mut rl, &[key(KeyCode::Up), key(KeyCode::Up)]);
        assert_eq!(rl.editor.get_buffer(), "two");
        let flow = step_key(&mut rl, ctrl('o'));
        assert!(
            matches!(flow, ControlFlow::Break(Signal::Success(ref s)) if s == "two"),
            "C-o submits the recalled line"
        );

        rl.begin_read();
        rl.apply_operate_and_get_next();
        assert_eq!(rl.editor.get_buffer(), "three");
    }

    #[test]
    fn operate_and_get_next_on_fresh_line_stays_empty() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["one", "two"]);
        type_each(&mut rl, &[ch('x')]);

        let flow = step_key(&mut rl, ctrl('o'));
        assert!(
            matches!(flow, ControlFlow::Break(Signal::Success(ref s)) if s == "x"),
            "C-o submits the fresh line"
        );

        rl.begin_read();
        rl.apply_operate_and_get_next();
        assert_eq!(rl.editor.get_buffer(), "");
    }

    #[test]
    fn operate_and_get_next_after_walk_back_to_live_line_stays_empty() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["one", "two"]);

        // Recall an entry then walk back to the live line: the history
        // position is the end, so there is no next entry to preload.
        drive(&mut rl, &[key(KeyCode::Up), key(KeyCode::Down)]);
        type_each(&mut rl, &[ch('z')]);
        let flow = step_key(&mut rl, ctrl('o'));
        assert!(matches!(flow, ControlFlow::Break(Signal::Success(ref s)) if s == "z"));

        rl.begin_read();
        rl.apply_operate_and_get_next();
        assert_eq!(rl.editor.get_buffer(), "");
    }

    // --- history-search / history-substr-search (search.c) ---

    #[test]
    fn history_search_backward_anchored_walks_prefix_matches() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::ALT,
            KeyCode::Char('p'),
            ReedlineEvent::HistorySearch {
                direction: Direction::Backward,
                anchored: true,
            },
        ));
        seed_history(&mut rl, &["ls foo", "cd bar", "ls baz"]);
        type_each(&mut rl, &[ch('l'), ch('s')]);

        drive(&mut rl, &[alt('p')]);
        assert_eq!(rl.editor.get_buffer(), "ls baz");
        // Anchored search lands the point at the needle's end.
        assert_eq!(rl.editor.insertion_point(), 2);

        drive(&mut rl, &[alt('p')]);
        assert_eq!(rl.editor.get_buffer(), "ls foo");
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    #[test]
    fn history_search_forward_returns_through_matches() {
        let mut kb = crate::default_emacs_keybindings();
        kb.add_binding(
            KeyModifiers::ALT,
            KeyCode::Char('p'),
            ReedlineEvent::HistorySearch {
                direction: Direction::Backward,
                anchored: true,
            },
        );
        kb.add_binding(
            KeyModifiers::ALT,
            KeyCode::Char('n'),
            ReedlineEvent::HistorySearch {
                direction: Direction::Forward,
                anchored: true,
            },
        );
        let mut rl = seam_engine(Box::new(crate::Emacs::new(kb)));
        seed_history(&mut rl, &["ls foo", "cd bar", "ls baz"]);
        type_each(&mut rl, &[ch('l'), ch('s')]);

        drive(&mut rl, &[alt('p')]);
        drive(&mut rl, &[alt('p')]);
        assert_eq!(rl.editor.get_buffer(), "ls foo");
        drive(&mut rl, &[alt('n')]);
        assert_eq!(rl.editor.get_buffer(), "ls baz");
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    #[test]
    fn history_substr_search_lands_on_match_offset() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::ALT,
            KeyCode::Char('p'),
            ReedlineEvent::HistorySearch {
                direction: Direction::Backward,
                anchored: false,
            },
        ));
        seed_history(&mut rl, &["ls foo", "cd bar", "ls baz"]);
        type_each(&mut rl, &[ch('b'), ch('a')]);

        drive(&mut rl, &[alt('p')]);
        assert_eq!(rl.editor.get_buffer(), "ls baz");
        // Substring search lands the point on the match offset
        // (GNU `rl_point = newcol`).
        assert_eq!(rl.editor.insertion_point(), 3);

        drive(&mut rl, &[alt('p')]);
        assert_eq!(rl.editor.get_buffer(), "cd bar");
        assert_eq!(rl.editor.insertion_point(), 3);
    }

    /// `C-r beta Enter` — GNU `_rl_isearch_fini` (isearch.c:300-304)
    /// leaves point at `cxt->sline_index`, the match start, not at the
    /// end of the recalled line.
    #[test]
    fn incremental_history_search_accept_lands_on_match_start() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        seed_history(&mut rl, &["alpha one", "beta two", "gamma three"]);
        drive(&mut rl, &[ctrl('r')]);
        type_each(&mut rl, &[ch('b'), ch('e'), ch('t'), ch('a')]);
        drive(&mut rl, &[key(KeyCode::Enter)]);

        assert_eq!(rl.editor.get_buffer(), "beta two");
        assert_eq!(rl.editor.insertion_point(), 0);
        assert_eq!(rl.input_mode, InputMode::Regular);
    }

    /// Reverse isearch scans a history line from its right edge
    /// (isearch.c:854), so it lands on the last occurrence of the
    /// needle.
    #[test]
    fn incremental_history_search_accept_lands_on_the_last_occurrence() {
        let mut rl = seam_engine(Box::<crate::Emacs>::default());
        seed_history(&mut rl, &["beta one beta"]);
        drive(&mut rl, &[ctrl('r')]);
        type_each(&mut rl, &[ch('b'), ch('e'), ch('t'), ch('a')]);
        drive(&mut rl, &[key(KeyCode::Enter)]);

        assert_eq!(rl.editor.get_buffer(), "beta one beta");
        assert_eq!(rl.editor.insertion_point(), 9);
    }

    #[test]
    fn history_search_empty_needle_walks_history() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::ALT,
            KeyCode::Char('p'),
            ReedlineEvent::HistorySearch {
                direction: Direction::Backward,
                anchored: true,
            },
        ));
        seed_history(&mut rl, &["one", "two"]);

        // `_rl_history_search_len == 0` → plain previous-history.
        drive(&mut rl, &[alt('p')]);
        assert_eq!(rl.editor.get_buffer(), "two");
    }

    #[test]
    fn history_search_failed_keeps_last_match() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::ALT,
            KeyCode::Char('p'),
            ReedlineEvent::HistorySearch {
                direction: Direction::Backward,
                anchored: true,
            },
        ));
        seed_history(&mut rl, &["ls foo", "cd bar", "ls baz"]);
        type_each(&mut rl, &[ch('l'), ch('s')]);

        drive(&mut rl, &[alt('p')]);
        drive(&mut rl, &[alt('p')]);
        assert_eq!(rl.editor.get_buffer(), "ls foo");
        // A failing press dings: buffer keeps the last match, point
        // goes back to the needle length.
        drive(&mut rl, &[alt('p')]);
        assert_eq!(rl.editor.get_buffer(), "ls foo");
        assert_eq!(rl.editor.insertion_point(), 2);
    }

    #[test]
    fn history_search_resets_after_other_input() {
        let mut rl = seam_engine(emacs_with(
            KeyModifiers::ALT,
            KeyCode::Char('p'),
            ReedlineEvent::HistorySearch {
                direction: Direction::Backward,
                anchored: true,
            },
        ));
        seed_history(&mut rl, &["ls foo", "cd bar", "ls baz"]);
        type_each(&mut rl, &[ch('l'), ch('s')]);

        drive(&mut rl, &[alt('p')]);
        assert_eq!(rl.editor.get_buffer(), "ls baz");
        // An intervening edit reinitializes (GNU's `rl_last_func`
        // check): the search landed the point at 2, so '!' inserts
        // there and the next search re-reads the needle from the
        // current buffer — "ls!" matches no entry, and the failed
        // search leaves the edited buffer alone (search.c:626).
        drive(&mut rl, &[ch('!')]);
        assert_eq!(rl.editor.get_buffer(), "ls! baz");
        drive(&mut rl, &[alt('p')]);
        assert_eq!(rl.editor.get_buffer(), "ls! baz");
        assert_eq!(rl.editor.insertion_point(), 3);
    }

    // --- non-incremental history search (search.c:462 `noninc_search`) ---

    /// A vi seam engine with the default three tables.
    fn vi_engine() -> Reedline {
        let mut rl = Reedline::create().with_edit_mode(Box::new(crate::Vi::new(
            crate::default_vi_insert_keybindings(),
            crate::default_vi_normal_keybindings(),
            crate::default_vi_visual_keybindings(),
        )));
        rl.painter.force_prompt_anchored_for_test(0);
        rl
    }

    #[test]
    fn noninc_search_backward_finds_substring() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["echo one", "echo two", "ls three"]);
        type_each(&mut rl, &[ch('w'), ch('o'), ch('r'), ch('k')]);

        drive(&mut rl, &[alt('p')]);
        type_each(
            &mut rl,
            &[ch('e'), ch('c'), ch('h'), ch('o'), key(KeyCode::Enter)],
        );
        assert_eq!(rl.editor.get_buffer(), "echo two");
        // `rl_point = 0` on the found entry (search.c:215).
        assert_eq!(rl.editor.insertion_point(), 0);
    }

    #[test]
    fn noninc_search_forward_finds_newer_entry() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["echo one", "echo two", "ls three"]);

        drive(&mut rl, &[alt('p')]);
        type_each(&mut rl, &[ch('^'), ch('e'), key(KeyCode::Enter)]);
        assert_eq!(rl.editor.get_buffer(), "echo two");

        // M-n from the hit searches newer entries (noninc_history_pos
        // + 1, search.c:192).
        drive(&mut rl, &[alt('n')]);
        type_each(&mut rl, &[ch('l'), ch('s'), key(KeyCode::Enter)]);
        assert_eq!(rl.editor.get_buffer(), "ls three");
    }

    #[test]
    fn noninc_search_anchor_requires_prefix() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["echo one"]);
        type_each(&mut rl, &[ch('w'), ch('o'), ch('r'), ch('k')]);

        drive(&mut rl, &[alt('p')]);
        type_each(&mut rl, &[ch('^'), ch('o'), key(KeyCode::Enter)]);
        // `^o` anchors: "echo one" has no leading `o` — a failed
        // search restores the saved line (search.c:446+).
        assert_eq!(rl.editor.get_buffer(), "work");
    }

    #[test]
    fn noninc_search_empty_string_repeats_last() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["echo one", "echo two", "ls three"]);

        drive(&mut rl, &[alt('p')]);
        type_each(
            &mut rl,
            &[ch('e'), ch('c'), ch('h'), ch('o'), key(KeyCode::Enter)],
        );
        assert_eq!(rl.editor.get_buffer(), "echo two");

        // Empty Enter reuses the saved string from its last position
        // (search.c:412-425) — backward again lands on the older hit.
        drive(&mut rl, &[alt('p'), key(KeyCode::Enter)]);
        assert_eq!(rl.editor.get_buffer(), "echo one");
    }

    #[test]
    fn noninc_search_abort_restores_line_and_point() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["echo one"]);
        type_each(&mut rl, &[ch('w'), ch('o'), ch('r'), ch('k')]);
        drive(&mut rl, &[ctrl('b')]);
        assert_eq!(rl.editor.insertion_point(), 3);

        drive(&mut rl, &[alt('p')]);
        type_each(&mut rl, &[ch('e'), ch('c'), key(KeyCode::Esc)]);
        assert_eq!(rl.editor.get_buffer(), "work");
        assert_eq!(rl.editor.insertion_point(), 3);
        assert_eq!(rl.input_mode, InputMode::Regular);
    }

    /// T2 F3: `C-g` is GNU's `abort` key (`bind -p`: `"\C-g": abort`) —
    /// in an incremental search it ends the search (`_rl_isearch_fini`
    /// restores the pre-search line) instead of feeding the query. Keys
    /// typed afterwards land in the regular buffer.
    #[test]
    fn ctrl_g_aborts_history_search_and_frees_typing() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["echo one", "echo two"]);
        drive(&mut rl, &[ctrl('r')]);
        assert_eq!(rl.input_mode, InputMode::HistorySearch);
        // A failing query keeps the search open waiting for input.
        type_each(&mut rl, &[ch('z'), ch('z'), ch('z')]);
        assert_eq!(rl.input_mode, InputMode::HistorySearch);

        drive(&mut rl, &[ctrl('g')]);
        assert_eq!(rl.input_mode, InputMode::Regular);
        assert_eq!(rl.editor.get_buffer(), "");

        // The next keys insert into the live line; they must not extend
        // the dead search query (the T2 "type-after-Cg" check).
        type_each(&mut rl, &[ch('e'), ch('c'), ch('h'), ch('o')]);
        assert_eq!(rl.editor.get_buffer(), "echo");
    }

    /// `C-g` also aborts a NON-incremental search (`_rl_nsearch_abort`,
    /// search.c:270): the saved line and point come back. T2 F3.
    #[test]
    fn ctrl_g_aborts_noninc_search_and_restores_the_line() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["echo one"]);
        type_each(&mut rl, &[ch('w'), ch('o'), ch('r'), ch('k')]);
        drive(&mut rl, &[ctrl('b')]);
        assert_eq!(rl.editor.insertion_point(), 3);

        drive(&mut rl, &[alt('p')]);
        type_each(&mut rl, &[ch('e'), ch('c'), ctrl('g')]);
        assert_eq!(rl.editor.get_buffer(), "work");
        assert_eq!(rl.editor.insertion_point(), 3);
        assert_eq!(rl.input_mode, InputMode::Regular);
    }

    #[test]
    fn noninc_search_backspace_on_empty_aborts() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('x'), ch('y')]);

        drive(&mut rl, &[alt('p')]);
        // Rubout at `rl_point == 0` is an abort (search.c:331).
        drive(&mut rl, &[key(KeyCode::Backspace)]);
        assert_eq!(rl.editor.get_buffer(), "xy");
        assert_eq!(rl.input_mode, InputMode::Regular);
    }

    #[test]
    fn noninc_search_collects_editable_string() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["echo one"]);

        drive(&mut rl, &[alt('p')]);
        // The buffer is the string being typed; it edits normally.
        type_each(&mut rl, &[ch('z'), key(KeyCode::Backspace), ch('e')]);
        assert_eq!(rl.editor.get_buffer(), "e");
        drive(&mut rl, &[key(KeyCode::Enter)]);
        assert_eq!(rl.editor.get_buffer(), "echo one");
    }

    #[test]
    fn noninc_again_without_previous_search_dings() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        let status = rl
            .handle_editor_event(
                &DefaultPrompt::default(),
                ReedlineEvent::NonIncrementalSearchAgain {
                    direction: Direction::Backward,
                },
            )
            .unwrap();
        assert!(matches!(status, EventStatus::Inapplicable));
    }

    #[test]
    fn vi_search_again_walks_matches() {
        // `/`+`n`/`N` — `rl_vi_search`/`rl_vi_search_again`
        // (vi_mode.c:348/:365), pattern matching on.
        let mut rl = vi_engine();
        seed_history(&mut rl, &["echo one", "echo two", "ls three"]);

        type_each(
            &mut rl,
            &[
                key(KeyCode::Esc),
                ch('/'),
                ch('e'),
                ch('c'),
                ch('h'),
                ch('o'),
                key(KeyCode::Enter),
            ],
        );
        assert_eq!(rl.editor.get_buffer(), "echo two");
        drive(&mut rl, &[ch('n')]);
        assert_eq!(rl.editor.get_buffer(), "echo one");
        // `N` — a shifted char so the vi parser sees the uppercase.
        drive(
            &mut rl,
            &[KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)],
        );
        assert_eq!(rl.editor.get_buffer(), "echo two");
    }

    #[test]
    fn vi_percent_matches_bracket() {
        // `%` — `rl_vi_match` (vi_mode.c:1951).
        let mut rl = vi_engine();
        type_each(
            &mut rl,
            &[ch('('), ch('a'), ch(')'), ch('b'), key(KeyCode::Esc)],
        );
        // Cursor sits on `b` — not a bracket, so GNU scans forward:
        // no bracket after `b`, and the scan only counts openers.
        // `h` back to `)` first.
        drive(&mut rl, &[ch('h'), ch('%')]);
        assert_eq!(rl.editor.insertion_point(), 0);
    }

    #[test]
    fn vi_percent_scans_forward_for_opener() {
        let mut rl = vi_engine();
        type_each(
            &mut rl,
            &[ch('x'), ch('('), ch('a'), ch(')'), key(KeyCode::Esc)],
        );
        // On `)` — a closer — match backward to `(`.
        drive(&mut rl, &[ch('%')]);
        assert_eq!(rl.editor.insertion_point(), 1);
    }

    #[test]
    fn vi_d_percent_cuts_through_bracket() {
        let mut rl = vi_engine();
        type_each(
            &mut rl,
            &[
                ch('('),
                ch('a'),
                ch('b'),
                ch(')'),
                ch('z'),
                key(KeyCode::Esc),
            ],
        );
        drive(&mut rl, &[ch('0'), ch('d'), ch('%')]);
        assert_eq!(rl.editor.get_buffer(), "z");
    }

    #[test]
    fn vi_column_moves_to_nth_character() {
        // `|` — `rl_vi_column` (vi_mode.c:1053).
        let mut rl = vi_engine();
        type_each(
            &mut rl,
            &[
                ch('a'),
                ch('b'),
                ch('c'),
                ch('d'),
                ch('e'),
                key(KeyCode::Esc),
            ],
        );
        drive(&mut rl, &[ch('3'), ch('|')]);
        assert_eq!(rl.editor.insertion_point(), 2);
        // Past the end → the line end (GNU `rl_end_of_line`).
        drive(&mut rl, &[ch('9'), ch('|')]);
        assert_eq!(rl.editor.insertion_point(), 4);
    }

    #[test]
    fn vi_capital_u_reverts_line() {
        // `U` — `rl_revert_line` (vi_keymap.c:129).
        let mut rl = vi_engine();
        type_each(
            &mut rl,
            &[
                ch('a'),
                ch('b'),
                ch('c'),
                key(KeyCode::Esc),
                KeyEvent::new(KeyCode::Char('U'), KeyModifiers::SHIFT),
            ],
        );
        assert_eq!(rl.editor.get_buffer(), "");
    }

    #[test]
    fn vi_underscore_yanks_last_arg() {
        // `_` — `rl_vi_yank_arg` (vi_mode.c:326).
        let mut rl = vi_engine();
        seed_history(&mut rl, &["ls /tmp dir"]);
        type_each(&mut rl, &[key(KeyCode::Esc), ch('_')]);
        assert_eq!(rl.editor.get_buffer(), "dir");
    }

    #[test]
    fn vi_count_underscore_yanks_nth_arg() {
        // `3_` → `rl_yank_nth_arg(count - 1)` = arg 2 ("dir").
        let mut rl = vi_engine();
        seed_history(&mut rl, &["ls /tmp dir"]);
        type_each(&mut rl, &[key(KeyCode::Esc), ch('3'), ch('_')]);
        assert_eq!(rl.editor.get_buffer(), "dir");

        // `2_` → arg 1 on a fresh line.
        let mut rl = vi_engine();
        seed_history(&mut rl, &["ls /tmp dir"]);
        type_each(&mut rl, &[key(KeyCode::Esc), ch('2'), ch('_')]);
        assert_eq!(rl.editor.get_buffer(), "/tmp");
    }

    // --- set-mark / exchange-point-and-mark / region (text.c:1880, kill.c:442) ---

    #[test]
    fn set_mark_and_exchange_point_roundtrip() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('a'), ch('b'), ch('c'), ch('d')]);
        drive(&mut rl, &[ctrl('b'), ctrl('b')]);
        // C-@ drops the mark at point (text.c:1880).
        drive(&mut rl, &[ctrl(' ')]);
        drive(&mut rl, &[ctrl('e')]);
        assert_eq!(rl.editor.insertion_point(), 4);
        // C-x C-x swaps point and mark (text.c:1885).
        drive(&mut rl, &[ctrl('x'), ctrl('x')]);
        assert_eq!(rl.editor.insertion_point(), 2);
        // And swaps back — the mark holds the old point.
        drive(&mut rl, &[ctrl('x'), ctrl('x')]);
        assert_eq!(rl.editor.insertion_point(), 4);
    }

    #[test]
    fn kill_region_feeds_kill_ring() {
        // `kill-region` — `rl_kill_region` (kill.c:449): point lands on
        // the region start, the killed text goes to the ring.
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('h'), ch('e'), ch('l'), ch('l'), ch('o')]);
        drive(&mut rl, &[ctrl('a'), ctrl(' '), ctrl('e')]);
        let status = rl
            .handle_editor_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Edit(vec![EditCommand::KillRegion]),
            )
            .unwrap();
        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(rl.editor.get_buffer(), "");
        assert_eq!(rl.editor.insertion_point(), 0);
        drive(&mut rl, &[ctrl('y')]);
        assert_eq!(rl.editor.get_buffer(), "hello");
    }

    #[test]
    fn copy_region_as_kill_keeps_buffer() {
        // `copy-region-as-kill` — `rl_copy_region_to_kill` (kill.c:442):
        // the region lands in the ring but the buffer is untouched.
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('x'), ch('y'), ch('z')]);
        drive(&mut rl, &[ctrl('a'), ctrl(' '), ctrl('e')]);
        let status = rl
            .handle_editor_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Edit(vec![EditCommand::CopyRegionAsKill]),
            )
            .unwrap();
        assert!(matches!(status, EventStatus::Handled));
        assert_eq!(rl.editor.get_buffer(), "xyz");
        assert_eq!(rl.editor.insertion_point(), 3);
        drive(&mut rl, &[ctrl('y')]);
        assert_eq!(rl.editor.get_buffer(), "xyzxyz");
    }

    #[test]
    fn kill_region_without_mark_is_inapplicable() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('x')]);
        let status = rl
            .handle_editor_event(
                &DefaultPrompt::default(),
                ReedlineEvent::Edit(vec![EditCommand::KillRegion]),
            )
            .unwrap();
        assert!(matches!(status, EventStatus::Inapplicable));
    }

    // --- print-last-kbd-macro (macro.c:309) ---

    #[test]
    fn print_last_kbd_macro_without_recording_dings() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        let status = rl
            .handle_event(&DefaultPrompt::default(), ReedlineEvent::PrintLastKbdMacro)
            .unwrap();
        assert!(matches!(status, EventStatus::Inapplicable));
    }

    #[test]
    fn key_sequence_spelling_uses_gnu_bind_forms() {
        use crate::edit_mode::key_sequence_spelling;
        use crate::KeyCombination;
        let keys = vec![
            KeyCombination {
                modifier: KeyModifiers::CONTROL,
                key_code: KeyCode::Char('x'),
            },
            KeyCombination {
                modifier: KeyModifiers::ALT,
                key_code: KeyCode::Char('y'),
            },
            KeyCombination {
                modifier: KeyModifiers::NONE,
                key_code: KeyCode::Char('a'),
            },
            KeyCombination {
                modifier: KeyModifiers::NONE,
                key_code: KeyCode::Enter,
            },
            KeyCombination {
                modifier: KeyModifiers::NONE,
                key_code: KeyCode::Backspace,
            },
        ];
        assert_eq!(key_sequence_spelling(&keys), "\\C-x\\M-ya^M^?");
    }

    // --- insert-comment (text.c:1384 `rl_insert_comment`) ---

    #[test]
    fn meta_hash_comments_out_and_submits() {
        let mut rl = seam_engine(Box::<Emacs>::default());
        type_each(&mut rl, &[ch('f'), ch('o'), ch('o')]);
        let signal = drive_until_signal(&mut rl, &[alt('#')]);
        assert!(matches!(signal, Some(Signal::Success(ref s)) if s == "# foo"));
    }

    // --- quote-aware argument tokenization (histexpand.c:1638) ---

    #[test]
    fn history_tokenize_splits_like_gnu() {
        // `history_tokenize` keeps quotes in the tokens and treats
        // `|`, `()` etc. as operator tokens (histexpand.c:1483-1692).
        assert_eq!(
            history_tokenize("cmd a \"b c\" 'd e' f\\ g"),
            vec!["cmd", "a", "\"b c\"", "'d e'", "f\\ g"]
        );
        assert_eq!(
            history_tokenize("a | b; (c)"),
            vec!["a", "|", "b", ";", "(", "c", ")"]
        );
        assert_eq!(
            history_tokenize("git commit -m \"two words\""),
            vec!["git", "commit", "-m", "\"two words\""]
        );
        // `'"'` — a double quote inside single quotes is literal.
        assert_eq!(history_tokenize("echo '\"'"), vec!["echo", "'\"'"]);
    }

    #[test]
    fn yank_last_arg_respects_shell_quotes() {
        // `!$` on `echo "two words"` is `"two words"` — one argument
        // under `history_arg_extract`, not two words.
        let mut rl = seam_engine(Box::<Emacs>::default());
        seed_history(&mut rl, &["echo \"two words\""]);
        drive(&mut rl, &[alt('.')]);
        assert_eq!(rl.editor.get_buffer(), "\"two words\"");
    }
}
