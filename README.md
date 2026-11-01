# ruline — a GNU readline-compatible line editor

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
![CI](https://github.com/unixwin/ruline/actions/workflows/ci.yml/badge.svg?branch=main)

## What ruline is

ruline is an independent line editor. It is **not a fork** of any other
project and does not track an upstream: the code base started from
[nushell/reedline](https://github.com/nushell/reedline) 0.52.1 (MIT), and
everything since is ours.

The product target is GNU readline 8.x compatibility, for two audiences:

- **niubash**, the shell that embeds ruline as its interactive editor;
- **C readline consumers** — bash loadable builtins, completion scripts and
  plugins that expect the `rl_*` / `history_*` ABI, hooks, keymaps, funmap
  names and inputrc behaviour.

The ABI work is tracked in
[unixwin/ruline#8](https://github.com/unixwin/ruline/issues/8). The GNU source
vendored in `unixwin/rubash` (`third_party/bash/lib/readline`, bash 5.3.20) is
the behavioural oracle: where ruline and GNU disagree, GNU wins.

What stays ruline-specific is additive: syntax highlighting, completion
menus, history hints, multiline editing and the other modern conveniences.
Those extras never change ABI-visible state or the bytes a plugin sees.

## Relationship to reedline

- The initial code base is reedline 0.52.1, MIT licensed. The original
  copyright notice is retained in full in [`LICENSE`](./LICENSE); the history
  and attribution are recorded in [`UPSTREAM.md`](./UPSTREAM.md).
- **Thank you** to the Nushell Project and every reedline contributor. Their
  MIT-licensed work is the foundation of this project. See
  [`ACKNOWLEDGMENTS.md`](./ACKNOWLEDGMENTS.md) for the full acknowledgement.
- ruline is an independent project. We do not merge nushell/reedline, do not
  send fixes upstream as a matter of course, and do not follow its edition,
  feature or API policy.
- The crate is still published under the package name `reedline` while the
  ABI transition is in flight, so existing source keeps compiling. The
  product name is **ruline**; the crate rename is tracked separately.

## Outline

- [Examples](#examples)
  - [Basic example](#basic-example)
  - [Integrate with custom keybindings](#integrate-with-custom-keybindings)
  - [Integrate with `History`](#integrate-with-history)
  - [Integrate with custom syntax `Highlighter`](#integrate-with-custom-syntax-highlighter)
  - [Integrate with custom tab completion](#integrate-with-custom-tab-completion)
  - [Integrate with `Hinter` for fish-style history autosuggestions](#integrate-with-hinter-for-fish-style-history-autosuggestions)
  - [Integrate with custom line completion `Validator`](#integrate-with-custom-line-completion-validator)
  - [Use custom `EditMode`](#use-custom-editmode)
- [Default keybindings](./KEYBINDINGS.md)
- [Crate features](#crate-features)
- [Status and roadmap](#status-and-roadmap)
- [Acknowledgements](./ACKNOWLEDGMENTS.md)
- [Contributing](./CONTRIBUTING.md)
- [Alternatives](#alternatives)

## Examples

The examples show the main extension points. The Rust API is still imported as `reedline` while the crate rename is pending; the product is ruline.

### Basic example

```rust,no_run
// Create a default reedline object to handle user input

use reedline::{DefaultPrompt, Reedline, Signal};

let mut line_editor = Reedline::create();
let prompt = DefaultPrompt::default();

loop {
    let sig = line_editor.read_line(&prompt);
    match sig {
        Ok(Signal::Success(buffer)) => {
            println!("We processed: {}", buffer);
        }
        Ok(Signal::CtrlD) | Ok(Signal::CtrlC) => {
            println!("\nAborted!");
            break;
        }
        x => {
            println!("Event: {:?}", x);
        }
    }
}
```

### Integrate with custom keybindings

```rust
// Configure reedline with custom keybindings

//Cargo.toml
//    [dependencies]
//    crossterm = "*"

use {
  crossterm::event::{KeyCode, KeyModifiers},
  reedline::{default_emacs_keybindings, EditCommand, Reedline, Emacs, ReedlineEvent},
};

let mut keybindings = default_emacs_keybindings();
keybindings.add_binding(
    KeyModifiers::ALT,
    KeyCode::Char('m'),
    ReedlineEvent::Edit(vec![EditCommand::BackspaceWord]),
);
let edit_mode = Box::new(Emacs::new(keybindings));

let mut line_editor = Reedline::create().with_edit_mode(edit_mode);
```

### Integrate with `History`

```rust,no_run
// Create a reedline object with history support, including history size limits

use reedline::{FileBackedHistory, Reedline};

let history = Box::new(
  FileBackedHistory::with_file(5, "history.txt".into())
    .expect("Error configuring history with file"),
);
let mut line_editor = Reedline::create()
  .with_history(history);
```

### Integrate with custom syntax `Highlighter`

```rust
// Create a reedline object with highlighter support

use reedline::{ExampleHighlighter, Reedline};

let commands = vec![
  "test".into(),
  "hello world".into(),
  "hello world reedline".into(),
  "this is the reedline crate".into(),
];
let mut line_editor =
Reedline::create().with_highlighter(Box::new(ExampleHighlighter::new(commands)));
```

### Integrate with custom tab completion

```rust
// Create a reedline object with tab completions support

use reedline::{default_emacs_keybindings, ColumnarMenu, DefaultCompleter, Emacs, KeyCode, KeyModifiers, Reedline, ReedlineEvent, ReedlineMenu, MenuBuilder};

let commands = vec![
  "test".into(),
  "hello world".into(),
  "hello world reedline".into(),
  "this is the reedline crate".into(),
];
let completer = Box::new(DefaultCompleter::new_with_wordlen(commands.clone(), 2));
// Use the interactive menu to select options from the completer
let completion_menu = Box::new(ColumnarMenu::default().with_name("completion_menu"));
// Set up the required keybindings
let mut keybindings = default_emacs_keybindings();
keybindings.add_binding(
    KeyModifiers::NONE,
    KeyCode::Tab,
    ReedlineEvent::UntilFound(vec![
        ReedlineEvent::Menu("completion_menu".to_string()),
        ReedlineEvent::MenuNext,
    ]),
);

let edit_mode = Box::new(Emacs::new(keybindings));

let mut line_editor = Reedline::create()
    .with_completer(completer)
    .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
    .with_edit_mode(edit_mode);
```

### Integrate with `Hinter` for fish-style history autosuggestions

```rust
// Create a reedline object with in-line hint support

//Cargo.toml
//  [dependencies]
//  nu-ansi-term = "*"

use {
  nu_ansi_term::{Color, Style},
  reedline::{DefaultHinter, Reedline},
};

let mut line_editor = Reedline::create().with_hinter(Box::new(
  DefaultHinter::default()
  .with_style(Style::new().italic().fg(Color::LightGray)),
));
```

### Integrate with custom line completion `Validator`

```rust
// Create a reedline object with line completion validation support

use reedline::{DefaultValidator, Reedline};

let validator = Box::new(DefaultValidator);

let mut line_editor = Reedline::create().with_validator(validator);
```

### Use custom `EditMode`

```rust
// Create a reedline object with custom edit mode
// This can define a keybinding setting or enable vi-emulation

use reedline::{
    default_vi_insert_keybindings, default_vi_normal_keybindings, default_vi_visual_keybindings,
    EditMode, Reedline, Vi,
};

let mut line_editor = Reedline::create().with_edit_mode(Box::new(Vi::new(
    default_vi_insert_keybindings(),
    default_vi_normal_keybindings(),
    default_vi_visual_keybindings(),
)));
```

### Use `Helix` edit mode

```rust
// Selection-first editing: motions carry the selection, verbs act on it.

use reedline::{default_helix_normal_keybindings, Helix, Reedline};

let mut normal_keybindings = default_helix_normal_keybindings();
// normal_keybindings.add_binding(..);

let mut line_editor = Reedline::create()
    .with_edit_mode(Box::new(Helix::default().with_normal_keybindings(normal_keybindings)));
```

Run `cargo run --example helix` for the mode on its own, or
`cargo run --example demo -- --helix` to exercise it against the demo's
history and menus.

## Crate features

- `clipboard`: use the system clipboard instead of the local clipboard.
- `bashisms`: support for history recall sequences such as `!!` and `!$`.
- `external_printer`: no-op, kept for source compatibility; `ExternalPrinter`
  is always available.
- `system_clipboard`: enable the `arboard`-backed clipboard backend.

The sqlite history backends are gone: ruline ships `FileBackedHistory` only.

## Status and roadmap

ruline already provides the modern editing surface:

- emacs and vi editing modes, configurable keybindings, chords and macros;
- configurable prompts, syntax highlighting, completion menus;
- file-backed history with interactive, incremental and non-incremental
  search;
- fish-style history hints, undo, selection, clipboard integration;
- multiline validation and Unicode-aware editing.

The GNU readline compatibility work is the current focus:

- [ ] C ABI package (`libreadline.so.8`, clean-room headers, symbol audit) —
  [#3](https://github.com/unixwin/ruline/issues/3)
- [ ] core state, `readline()`, callbacks and history — [#2](https://github.com/unixwin/ruline/issues/2)
- [ ] keymap/funmap/bind/inputrc — [#4](https://github.com/unixwin/ruline/issues/4)
- [ ] completion API and hooks — [#6](https://github.com/unixwin/ruline/issues/6)
- [ ] plugin acceptance (`enable -f`, ble.sh, fzf, bash-completion) — [#7](https://github.com/unixwin/ruline/issues/7)
- [ ] keybinding parity burn-down — [#5](https://github.com/unixwin/ruline/issues/5)
- [ ] niubash in-process integration — [unixwin/niubash#775](https://github.com/unixwin/niubash/issues/775)

Non-goals: byte-identical internals of GNU readline, zsh/zle plugin support,
and sharing ABI state with a separately linked `libreadline` instance.

## Acknowledgements

ruline is independent, but it stands on other people's work. Thank you to:

- the **Nushell Project and every reedline contributor** — the MIT-licensed
  code base we started from, and the original copyright notice we keep in
  [`LICENSE`](./LICENSE);
- **Chet Ramey and the GNU readline contributors** — the behavioural oracle
  for the ABI work. No GPL code or headers are copied; the C declarations are
  clean-room;
- the Rust crates in `Cargo.lock` and their maintainers.

See [`ACKNOWLEDGMENTS.md`](./ACKNOWLEDGMENTS.md) for details.

### Alternatives

- [GNU readline](https://tiswww.case.edu/php/chet/readline/rltop.html) — the
  compatibility target.
- [rustyline](https://crates.io/crates/rustyline) — another mature Rust line
  editor.
