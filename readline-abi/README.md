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

Hooks: `rl_startup_hook` and `rl_pre_input_hook` are invoked by `readline()`;
`rl_redisplay_function` is the repaint indirection used by
`rl_forced_update_display`, `rl_clear_message`, completion displays and
`rl_resize_terminal`.  `rl_event_hook`, `rl_input_available_hook`,
`rl_getc_function`, `rl_prep_term_function` and `rl_deprep_term_function` are
stored but belong to the host input loop / core readline milestone, which owns
the event loop and terminal preparation.

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
  `rl_prep_term_function` and `rl_deprep_term_function` have storage but no
  invocation point in this standalone crate; they belong to the core input
  loop milestone.
- `rl_push_macro_input`, the keyboard-macro recording commands,
  `rl_get_termcap`, `rl_change_environment`, and `rl_explicit_arg` are not
  implemented yet. Callbacks, signals, kill ring, undo, and history
  expansion/search remain future slices.
- The history store is an in-process vector; it does not use
  `FileBackedHistory` or persist automatically.
- Only `libreadline.so.8` packaging exists; a separate `libhistory.so.8`
  split is still to do.
