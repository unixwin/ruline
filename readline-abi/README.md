# readline-abi

Clean-room GNU readline 8.x C ABI adapter for ruline.  The crate builds
`libreadline.so` / `libreadline.a` and exports only the symbols declared in
`include/readline/readline.h` and `include/readline/history.h`.

The compatibility target is GNU readline; thank you to Chet Ramey and its
contributors for the documented interface. We do not copy GPL code or header
text: the declarations are clean-room and the symbol inventory is names only.
See [`../ACKNOWLEDGMENTS.md`](../ACKNOWLEDGMENTS.md).

## Build

```sh
cargo build -p readline-abi
```

Artifacts:

| Artifact | Purpose |
| --- | --- |
| `target/debug/libreadline.so` | `cdylib` for `dlopen` / `-lreadline` |
| `target/debug/libreadline.a` | `staticlib` |
| `target/debug/libreadline.rlib` | Rust consumers/tests |

`build.rs` emits `-Wl,-soname,libreadline.so.8` (or the equivalent
`install_name` on macOS), so binaries linked against the shared object record
the GNU ABI major at runtime.  `scripts/package.sh` stages the versioned file
name:

```sh
readline-abi/scripts/package.sh release
# dist/release/lib/libreadline.so.8
# dist/release/lib/libreadline.so -> libreadline.so.8
```

## Differential conformance

`tests/conformance/run.sh` compiles the same C probe against the system GNU
readline 8.3 and against this library, then diffs the output. The pure ABI
surface (state/text/keymap/bind/inputrc/variables/history/completion) is
expected to match line-for-line; the PTY `readline()` driver is a separate
last-mile oracle. See `tests/conformance/README.md`.

## Tests and audit

```sh
readline-abi/scripts/smoke.sh debug
```

The script builds the crate, creates the `libreadline.so.8` runtime symlink,
compiles `tests/readline_smoke.c` against `-lreadline`, runs it, and then runs
the symbol audit.  The audit parses our headers as the implemented set and
checks each name against the vendored GNU headers (or the checked-in
`abi/readline-symbols.txt` inventory on CI) and against `nm -D`.

Regenerate the name-only inventory when the vendored headers move:

```sh
READLINE_ABI_VENDOR_HEADERS=/path/to/readline \
  python3 readline-abi/scripts/symbol-audit.py --generate-inventory
```

## Coverage and benchmarks

`scripts/abi-coverage.py` parses the vendored GNU 8.3 headers, our clean-room
headers, and the built shared object.  It reports functions, globals, coverage,
and the missing names grouped by area.  The checked-in snapshot is
[`abi/coverage.md`](abi/coverage.md):

```sh
cargo build -p readline-abi --release
readline-abi/scripts/abi-coverage.py                 # human-readable
readline-abi/scripts/abi-coverage.py --json          # machine-readable
readline-abi/scripts/abi-coverage.py --snapshot      # refresh abi/coverage.md
readline-abi/scripts/abi-coverage.py --snapshot --check  # CI staleness check
```

The ABI benchmark harness calls the exported C surface through `dlopen`.  It
covers key-sequence translation, keymap bind/lookup, function-of-keyseq,
completion with 1,000 candidates, filename completion, line insert/delete at
1 KiB and 64 KiB, and history add/get at 1k/10k entries:

```sh
readline-abi/scripts/bench.sh
BENCH_SAMPLES=41 readline-abi/scripts/bench.sh
```

The script writes [`BENCH.md`](BENCH.md) with the baseline table, the exact
command, and notes on complexity, allocations, and observed hot spots.
[`abi/issue-8-comment.md`](abi/issue-8-comment.md) is a draft comment that turns
the coverage gaps into implementation slices.

## Implemented surface

State globals: `rl_line_buffer`, `rl_point`, `rl_end`, `rl_mark`,
`rl_readline_state`, `rl_done`, `rl_pending_input`, `rl_prompt`,
`rl_display_prompt`, `rl_readline_name`, `rl_echoing_p`, `rl_insert_mode`,
`rl_last_func`, `rl_key_sequence_length`, `rl_executing_keyseq`,
`rl_library_version`, `rl_readline_version`.

Lifecycle/text/display: `readline`, `rl_initialize`, `rl_set_prompt`,
`rl_expand_prompt`, `rl_replace_line`, `rl_insert_text`, `rl_delete_text`,
`rl_copy_text`, `rl_kill_text`, `rl_extend_line_buffer`, `rl_ding`, `rl_crlf`,
`rl_on_new_line`, `rl_redisplay`, `rl_forced_update_display`,
`rl_clear_message`, `rl_clear_visible_line`, `rl_free_line_state`,
`rl_reset_line_state`, `rl_save_prompt`, `rl_restore_prompt`,
`rl_set_keyboard_input_timeout`.

Completion: `rl_complete`, `rl_complete_internal`, `rl_possible_completions`,
`rl_insert_completions`, `rl_menu_complete`, `rl_backward_menu_complete`,
`rl_completion_matches`, `rl_filename_completion_function`,
`rl_username_completion_function`, `rl_display_match_list`,
`rl_completion_mode`, plus the completion state variables (entry/attempted
functions, append/quote characters, word-break and quote characters, the
filename rewrite/dequote/stat/directory hooks, the display and word-break
hooks, and `rl_filename_completion_desired`/`rl_filename_quoting_desired`).
Filename completion reads the filesystem, expands `~`/`~user`, applies the
quoting/dequoting hooks, marks directories with `/`, and returns
`free()`-compatible match arrays.  Username completion reads `/etc/passwd`;
platforms without it (notably Windows) report no username matches.

Hooks: `rl_startup_hook` and `rl_pre_input_hook` are invoked by `readline()`
and by the callback install path; `rl_redisplay_function` is the repaint
indirection used by `rl_forced_update_display`, `rl_clear_message`, completion
displays and `rl_resize_terminal`.  `rl_event_hook` is polled while
`rl_callback_read_char` waits for input, `rl_input_available_hook` reports
readiness, `rl_getc_function` supplies bytes, and `rl_prep_term_function` /
`rl_deprep_term_function` wrap terminal preparation when they are set.

Callback and terminal: `rl_callback_handler_install`, `rl_callback_handler_remove`,
`rl_callback_read_char`, `rl_callback_sigcleanup`, `rl_prep_terminal`,
`rl_deprep_terminal`, `rl_get_screen_size`, `rl_set_screen_size`,
`rl_reset_screen_size`, `rl_set_signals`, `rl_clear_signals`,
`rl_cleanup_after_signal`, `rl_reset_after_signal`, `rl_clear_pending_input`.
`rl_callback_read_char` dispatches through the active keymap (one sub-keymap
level and macro expansion), honours `rl_pending_input`, and delivers completed
lines, EOF (Ctrl-D) and interrupts (Ctrl-C) to the installed handler.  On Unix,
`rl_set_signals` catches `SIGINT` and `SIGWINCH`; the resize path refreshes the
cached size and repaints, and the interrupt path restores the terminal before
delivering the signal to the application's previous handler.  Other GNU-caught
signals and non-Unix console signal mapping are not part of this slice yet.

History: `add_history`, `add_history_time`, `clear_history`, `using_history`,
`stifle_history`, `unstifle_history`, `history_is_stifled`, `where_history`,
`history_get`, `history_get_time`, `history_set_pos`, `current_history`,
`previous_history`, `next_history`, `history_total_bytes`, `read_history`,
`read_history_range`, `write_history`, `append_history`,
`history_truncate_file`, plus the standard exported history state variables.

Keymaps and funmap: `emacs_standard_keymap`, `emacs_meta_keymap`,
`emacs_ctlx_keymap`, `vi_insertion_keymap`, `vi_movement_keymap`,
`rl_get_keymap`, `rl_set_keymap`, `rl_get_keymap_by_name`,
`rl_get_keymap_name`, `rl_set_keymap_name`, `rl_make_bare_keymap`,
`rl_make_keymap`, `rl_copy_keymap`, `rl_discard_keymap`, `rl_free_keymap`,
`rl_empty_keymap`, `rl_add_defun`, `rl_add_funmap_entry`, `rl_funmap_names`,
`rl_initialize_funmap`, `rl_list_funmap_names`, `rl_named_function`,
`rl_function_of_keyseq`, `rl_function_of_keyseq_len`, `rl_invoking_keyseqs`,
`rl_invoking_keyseqs_in_map`.

Binding, variables, and inputrc: `rl_bind_key`, `rl_bind_key_in_map`,
`rl_bind_key_if_unbound`, `rl_bind_key_if_unbound_in_map`, `rl_bind_keyseq`,
`rl_bind_keyseq_in_map`, `rl_bind_keyseq_if_unbound`,
`rl_bind_keyseq_if_unbound_in_map`, `rl_generic_bind`, `rl_macro_bind`,
`rl_unbind_key`, `rl_unbind_key_in_map`, `rl_unbind_function_in_map`,
`rl_unbind_command_in_map`, `rl_translate_keyseq`, `rl_untranslate_keyseq`,
`rl_variable_value`, `rl_variable_bind`, `rl_parse_and_bind`,
`rl_read_init_file`, `rl_re_read_init_file`, `rl_print_keybinding`,
`rl_function_dumper`, `rl_macro_dumper`, `rl_variable_dumper`,
`rl_dump_functions`, `rl_dump_macros`, `rl_dump_variables`.

The default Emacs and vi keymap slots are materialised from the GNU 8.3
tables.  The public funmap command bodies are inert in this standalone crate
(they are not exported ABI functions); a host that owns the live editor can
bind its own functions with `rl_add_defun`.  `rl_dump_*` treat a non-zero
`count` as the readable flag because the `rl_explicit_arg` global is not yet
part of this slice.  `$if` supports `term=`, `mode=`, `version`, the
application name, and boolean/string variable comparisons; `$else`, `$endif`,
and `$include` are also handled.

Unimplemented GNU entry points are intentionally absent from both the headers
and the library; the audit rejects any exported `rl_*`/`history_*` symbol that
is not declared.  There are no silent stubs.

## Known limits / remaining work

- `readline()` runs a fresh ruline `Reedline` per call and does not yet share
  the ABI history store with the editor.
- Live key dispatch is not routed yet: binding a key does not change niu's
  editor (tracked by niubash#775). Completion is a standalone API for the same
  reason.
- Direct callers of `rl_menu_complete` must set `rl_last_func` the way the
  readline dispatcher does, otherwise each call starts a new menu cycle.
- `rl_event_hook`, `rl_input_available_hook`, `rl_getc_function`,
  `rl_prep_term_function` and `rl_deprep_term_function` are live in the
  callback input loop, but `readline()` still reads through Reedline rather
  than that loop.
- `rl_push_macro_input`, the keyboard-macro recording commands,
  `rl_get_termcap`, `rl_change_environment`, and `rl_explicit_arg` are not
  implemented yet. The rest of the signal set (SIGTERM/SIGHUP/SIGQUIT/
  SIGALRM/SIGTSTP/SIGTTIN/SIGTTOU), kill ring, undo, and history expansion/
  search remain future slices.
- The history store is an in-process vector; it does not use
  `FileBackedHistory` or persist automatically.
- Only `libreadline.so.8` packaging exists; a separate `libhistory.so.8`
  split is still to do.
