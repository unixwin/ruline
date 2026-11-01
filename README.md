# ruline — a GNU readline-compatible line editor

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
![CI](https://github.com/unixwin/ruline/actions/workflows/ci.yml/badge.svg?branch=main)

ruline is an independent line editor for shells and REPLs. It is **not a fork**
and does not track an upstream: the code base started from
[nushell/reedline](https://github.com/nushell/reedline) 0.52.1 (MIT), and
everything since is ruline's own.

The product target is GNU readline 8.x compatibility, for two audiences:

- **niubash**, the shell that embeds ruline as its interactive editor;
- **C readline consumers** — bash loadable builtins, completion scripts and
  plugins that expect the `rl_*` / `history_*` ABI, hooks, keymaps, funmap
  names and inputrc behaviour.

GNU is the oracle: where ruline and GNU disagree, GNU wins. The compatibility
work is tracked in
[unixwin/ruline#8](https://github.com/unixwin/ruline/issues/8); the vendored
bash 5.3.20 tree in `unixwin/rubash` (`third_party/bash/lib/readline`) is the
specification.

## Quick start

```sh
cargo build
cargo run --example basic
cargo test --all
```

The default keybindings are documented in [`KEYBINDINGS.md`](./KEYBINDINGS.md).
The C ABI crate lives in [`readline-abi/`](./readline-abi) and builds
`libreadline.so` / `libreadline.a` with clean-room headers; see
[`readline-abi/README.md`](./readline-abi/README.md).

## What ruline adds

- emacs and vi editing modes, configurable keybindings, chords and macros;
- configurable prompts, syntax highlighting, completion menus;
- file-backed history with interactive, incremental and non-incremental
  search;
- fish-style history hints, undo, selection, clipboard integration;
- multiline validation and Unicode-aware editing;
- a GNU-layout keymap/funmap/completion ABI, and an exported C surface for
  in-process plugins.

Those extras are additive. They never change ABI-visible state, hook order or
the bytes a plugin sees.

## Status and roadmap

The readline ABI is being built in slices (tracker
[#8](https://github.com/unixwin/ruline/issues/8)):

- [x] ABI package, clean-room headers, SONAME, C smoke test, symbol audit
- [x] core state, prompt/text operations, history store, minimal `readline()`
- [x] keymaps, funmap, bind/inputrc, variables, dump forms
- [x] completion API, hooks, filename/username completion
- [ ] core editing command bodies, callbacks, signals, terminal prep
- [ ] complete history API and input-loop hook invocation
- [ ] live niu integration — [unixwin/niubash#775](https://github.com/unixwin/niubash/issues/775)
- [ ] plugin acceptance (`enable -f`, ble.sh, fzf, bash-completion) — [#7](https://github.com/unixwin/ruline/issues/7)
- [ ] default-keymap parity burn-down — [#5](https://github.com/unixwin/ruline/issues/5)

`readline-abi` exports 274 symbols today (62.2% of the GNU-spec surface). The
coverage audit against the vendored GNU headers tracks the remaining API and
drives the implementation to full coverage. The hard definition-of-done gates
— coverage, GNU differential conformance, plugin matrix, tests and
performance — are in [`COMPATIBILITY.md`](./COMPATIBILITY.md). The project
does not claim 100% compatibility until every gate is green on the same
commit.

## Independence, license and thanks

ruline is independent. We do not merge nushell/reedline, do not follow its
release cadence and do not treat its API as a compatibility constraint.

The original MIT license and copyright notice are retained in full in
[`LICENSE`](./LICENSE). **Thank you** to the Nushell Project and every
reedline contributor — their MIT-licensed work is the foundation of this
project. Thank you also to Chet Ramey and the GNU readline contributors for
the interface we target. No GPL code or headers are copied; the C declarations
are clean-room.

See [`ACKNOWLEDGMENTS.md`](./ACKNOWLEDGMENTS.md) and
[`UPSTREAM.md`](./UPSTREAM.md) for the full attribution.

The Rust package is still named `reedline` during the ABI transition so
existing source keeps compiling; the product is **ruline**. The crate rename
is tracked in [#9](https://github.com/unixwin/ruline/issues/9).

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md). The ABI phases in the tracker are
the best starting points right now.

## Alternatives

- [GNU readline](https://tiswww.case.edu/php/chet/readline/rltop.html) — the
  compatibility target.
- [rustyline](https://crates.io/crates/rustyline) — another mature Rust line
  editor.
