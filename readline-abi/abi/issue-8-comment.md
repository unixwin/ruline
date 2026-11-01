# Draft comment for issue #8 (ABI coverage)

This is a draft, not a posted comment. It records the measurement slice for
issue #8 so implementation work can be split without overlapping edits.

## Baseline

`readline-abi/abi/coverage.md` is generated from the vendored GNU readline 8.3
headers and the built `libreadline`:

| Surface | Implemented | Total | Coverage |
| --- | ---: | ---: | ---: |
| Functions | 105 | 329 | 31.9% |
| Globals | 80 | 110 | 72.7% |
| **All** | **185** | **439** | **42.1%** |

254 public names are still missing (224 functions, 30 globals). The measurement
PR adds no ABI symbols, headers, or smoke-test cases.

## Top missing groups

| Group | Missing | Implemented / total |
| --- | ---: | ---: |
| text | 133 | 7 / 140 |
| history | 37 | 38 / 75 |
| terminal | 20 | 2 / 22 |
| display | 16 | 10 / 26 |
| state | 15 | 16 / 31 |
| completion | 10 | 44 / 54 |
| misc (signals) | 10 | 1 / 11 |
| keymap | 6 | 59 / 65 |
| callbacks | 4 | 0 / 4 |
| hooks | 3 | 8 / 11 |

## Suggested implementation slices

These are ordered by leverage and by keeping related state together. Each slice
can be a separate PR; none of them depends on the measurement files.

1. **Keymap/state quick wins (24 symbols).**
   - Keymap: `funmap`, `rl_executing_keymap`, `rl_binding_keymap`,
     `rl_executing_key`, `rl_set_key`, `rl_trim_arg_from_keyseq`.
   - Hooks: `rl_macro_display_hook`, `rl_signal_event_hook`,
     `rl_timeout_event_hook`.
   - State: `rl_editing_mode`, `rl_eof_found`, `rl_dispatching`,
     `rl_explicit_arg`, `rl_numeric_arg`, `rl_gnu_readline_p`,
     `rl_terminal_name`, `rl_instream`, `rl_outstream`,
     `rl_prefer_env_winsize`, `rl_executing_macro`, `rl_undo_list`.
   - Lifecycle/state: `rl_save_state`, `rl_restore_state`,
     `rl_discard_argument`.
   Most are storage plus small accessors; `rl_set_key` is the documented alias
   for `rl_bind_keyseq_in_map`.

2. **History list API (17 symbols).**
   `alloc_history_entry`, `copy_history_entry`, `free_history_entry`,
   `remove_history`, `remove_history_range`, `replace_history_entry`,
   `history_list`, `history_get_history_state`, `history_set_history_state`,
   `history_search`, `history_search_prefix`, `history_search_pos`,
   `history_expand`, `history_arg_extract`, `get_history_event`,
   `history_tokenize`, `history_inhibit_expansion_function`.
   This is the list/expansion part of the history ABI, separate from the
   key-bound navigation commands.

3. **Terminal and input plumbing (30 symbols including signals).**
   `rl_prep_terminal`, `rl_deprep_terminal`, `rl_tty_set_default_bindings`,
   `rl_tty_unset_default_bindings`, `rl_tty_set_echoing`, `rl_reset_terminal`,
   `rl_set_screen_size`, `rl_get_screen_size`,
   `rl_reset_screen_size`, `rl_get_termcap`, `rl_reparse_colors`,
   `rl_read_key`, `rl_getc`, `rl_stuff_char`, `rl_execute_next`,
   `rl_clear_pending_input`, `rl_set_timeout`, `rl_timeout_remaining`, `rl_set_paren_blink_timeout`,
   `rl_echo_signal_char`, and the signal group (`rl_set_signals`,
   `rl_clear_signals`, `rl_cleanup_after_signal`, `rl_reset_after_signal`,
   `rl_pending_signal`, `rl_check_signals`,
   `rl_catch_signals`, `rl_catch_sigwinch`, `rl_change_environment`,
   `rl_persistent_signal_handlers`). This belongs with the core input loop.

4. **Display helpers (16 symbols).**
   `rl_message`, `rl_show_char`, `rl_character_len`, `rl_on_new_line_with_prompt`,
   `rl_refresh_line`, `rl_clear_screen`, `rl_clear_display`,
   `rl_skip_csi_sequence`, `rl_arrow_keys`, `rl_previous_screen_line`,
   `rl_next_screen_line`, `rl_redraw_prompt_last_line`, `rl_already_prompted`,
   `rl_erase_empty_line`, `rl_num_chars_to_read`, `crlf`.

5. **Callbacks (4 symbols).**
   `rl_callback_handler_install`, `rl_callback_read_char`,
   `rl_callback_handler_remove`, `rl_callback_sigcleanup`.
   Small surface, but depends on the input loop from slice 3.

6. **Text editing core (48 symbols).**
   Movement/editing commands: `rl_insert`, `rl_quoted_insert`, `rl_tab_insert`,
   `rl_newline`, `rl_rubout`, `rl_delete`, `rl_rubout_or_delete`,
   `rl_delete_horizontal_space`, `rl_forward`, `rl_forward_byte`,
   `rl_forward_char`, `rl_forward_word`, `rl_backward`, `rl_backward_byte`,
   `rl_backward_char`, `rl_backward_word`, `rl_beg_of_line`, `rl_end_of_line`,
   `rl_transpose_chars`, `rl_transpose_words`, `rl_upcase_word`,
   `rl_downcase_word`, `rl_capitalize_word`, `rl_char_search`,
   `rl_backward_char_search`, `rl_set_mark`, `rl_exchange_point_and_mark`,
   `rl_activate_mark`, `rl_deactivate_mark`, `rl_mark_active_p`,
   `rl_keep_mark_active`, `rl_insert_comment`, `rl_insert_close`,
   `rl_tilde_expand`, `rl_abort`, `rl_tty_status`, `rl_execute_named_command`,
   `rl_digit_argument`, `rl_universal_argument`, `rl_emacs_editing_mode`,
   `rl_vi_editing_mode`, `rl_overwrite_mode`, plus the legacy aliases
   `alphabetic`, `ding`, `rl_alphabetic`.

7. **Kill ring, undo, and macros (38 symbols).**
   Kill/yank: `rl_kill_line`, `rl_backward_kill_line`, `rl_kill_full_line`,
   `rl_kill_word`, `rl_backward_kill_word`, `rl_unix_word_rubout`,
   `rl_unix_filename_rubout`, `rl_unix_line_discard`, `rl_copy_region_to_kill`,
   `rl_kill_region`, `rl_copy_forward_word`, `rl_copy_backward_word`,
   `rl_yank`, `rl_yank_pop`, `rl_yank_nth_arg`, `rl_yank_last_arg`.
   Undo: `rl_add_undo`, `rl_do_undo`, `rl_begin_undo_group`,
   `rl_end_undo_group`, `rl_modifying`, `rl_free_undo_list`, `rl_undo_command`,
   `rl_revert_line`, `rl_free`, and the legacy `free_undo_list`,
   `maybe_save_line`, `maybe_unsave_line`, `maybe_replace_line`.
   Macros/large-input: `rl_start_kbd_macro`, `rl_end_kbd_macro`,
   `rl_call_last_kbd_macro`, `rl_print_last_kbd_macro`, `rl_push_macro_input`,
   `rl_bracketed_paste_begin`, `rl_paste_from_clipboard`,
   `rl_restart_output`, `rl_stop_output`.

8. **History navigation commands (20 symbols).**
   `rl_beginning_of_history`, `rl_end_of_history`, `rl_get_next_history`,
   `rl_get_previous_history`, `rl_operate_and_get_next`, `rl_fetch_history`,
   `rl_reverse_search_history`, `rl_forward_search_history`,
   `rl_history_search_forward`, `rl_history_search_backward`,
   `rl_history_substr_search_forward`, `rl_history_substr_search_backward`,
   `rl_noninc_forward_search`, `rl_noninc_forward_search_again`,
   `rl_noninc_reverse_search`, `rl_noninc_reverse_search_again`,
   `rl_vi_fetch_history`, `rl_vi_search`, `rl_vi_search_again`,
   `rl_clear_history`.

9. **vi command set (47 symbols).**
   The `rl_vi_*` block, best done after slices 1 and 6 give it a command
   dispatch and text-motion API to call.

10. **Completion tail (10 symbols).**
    `completion_matches`, `filename_completion_function`,
    `username_completion_function`, `rl_old_menu_complete`,
    `rl_export_completions`, `rl_delete_or_show_completions`, `rl_vi_complete`,
    `rl_directory_completion_hook`, `rl_ignore_some_completions_function`,
    `rl_completion_rewrite_hook`. Several are legacy aliases or existing hook
    storage, so this should be a short slice after the text commands exist.

## Notes for implementers

- Keep the invariant enforced by `scripts/symbol-audit.py`: a name is added to
  the clean-room header, the Rust export, and `tests/readline_smoke.c` in the
  same change. Do not add silent stubs.
- Refresh `abi/coverage.md` after each slice with:
  `readline-abi/scripts/abi-coverage.py --snapshot`.
- `readline-abi/scripts/abi-coverage.py --snapshot --check` is suitable for CI
  to catch a stale snapshot.
- Benchmark before and after the hot-path slices (keymap lookup, completion,
  history add) with `readline-abi/scripts/bench.sh`; see `BENCH.md`.
