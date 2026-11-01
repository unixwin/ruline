/* Clean-room GNU readline 8.x ABI declarations for ruline.
 *
 * This header is an independent description of the public C ABI.  It is not
 * derived from GNU readline's copyrighted header text; only the documented
 * names, types, and values that form the binary interface are reproduced.
 *
 * Only the symbols actually provided by this library are declared.  A
 * declaration here is a promise: the matching symbol is exported by
 * libreadline and covered by the C smoke test and symbol audit.
 */

#ifndef RULINE_READLINE_ABI_READLINE_H
#define RULINE_READLINE_ABI_READLINE_H

#include <stdio.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Hex-encoded interface version.  8.3 is the ABI family described by the
 * vendored GNU 8.3 headers. */
#define RL_READLINE_VERSION 0x0803
#define RL_VERSION_MAJOR 8
#define RL_VERSION_MINOR 3

/* Prompt markers.  Bytes between these markers are emitted but do not count
 * toward the visible prompt width. */
#define RL_PROMPT_START_IGNORE '\001'
#define RL_PROMPT_END_IGNORE '\002'

/* Type of a bindable editing command: (count, key) -> status. */
typedef int rl_command_func_t(int, int);

/* Completion callback types.  These are the documented GNU spellings so that
 * application code can declare its own hooks without casts. */
typedef char *rl_compentry_func_t(const char *, int);
typedef char **rl_completion_func_t(const char *, int, int);
typedef char *rl_quote_func_t(char *, int, char *);
typedef char *rl_dequote_func_t(char *, int);
typedef void rl_compdisp_func_t(char **, int, int);
typedef int rl_hook_func_t(void);
typedef int rl_getc_func_t(FILE *);
typedef int rl_linebuf_func_t(char *, int);
typedef int rl_icppfunc_t(char **);
typedef char *rl_cpvfunc_t(void);
typedef void rl_voidfunc_t(void);
typedef void rl_vintfunc_t(int);

/* Values passed to rl_filename_quoting_function as the match kind. */
#define NO_MATCH 0
#define SINGLE_MATCH 1
#define MULT_MATCH 2

/* Values for the rl_readline_state bit mask. */
#define RL_STATE_NONE 0x00000000
#define RL_STATE_INITIALIZING 0x00000001
#define RL_STATE_INITIALIZED 0x00000002
#define RL_STATE_TERMPREPPED 0x00000004
#define RL_STATE_READCMD 0x00000008
#define RL_STATE_METANEXT 0x00000010
#define RL_STATE_DISPATCHING 0x00000020
#define RL_STATE_MOREINPUT 0x00000040
#define RL_STATE_ISEARCH 0x00000080
#define RL_STATE_NSEARCH 0x00000100
#define RL_STATE_SEARCH 0x00000200
#define RL_STATE_NUMERICARG 0x00000400
#define RL_STATE_MACROINPUT 0x00000800
#define RL_STATE_MACRODEF 0x00001000
#define RL_STATE_OVERWRITE 0x00002000
#define RL_STATE_COMPLETING 0x00004000
#define RL_STATE_SIGHANDLER 0x00008000
#define RL_STATE_UNDOING 0x00010000
#define RL_STATE_INPUTPENDING 0x00020000
#define RL_STATE_TTYCSAVED 0x00040000
#define RL_STATE_CALLBACK 0x00080000
#define RL_STATE_VIMOTION 0x00100000
#define RL_STATE_MULTIKEY 0x00200000
#define RL_STATE_VICMDONCE 0x00400000
#define RL_STATE_CHARSEARCH 0x00800000
#define RL_STATE_REDISPLAYING 0x01000000
#define RL_STATE_DONE 0x02000000
#define RL_STATE_TIMEOUT 0x04000000
#define RL_STATE_EOF 0x08000000
#define RL_STATE_READSTR 0x10000000

#define RL_SETSTATE(x) (rl_readline_state |= (x))
#define RL_UNSETSTATE(x) (rl_readline_state &= ~(x))
#define RL_ISSTATE(x) (rl_readline_state & (x))

/* Version and running state. */
extern const char *rl_library_version;
extern int rl_readline_version;
extern unsigned long rl_readline_state;
extern const char *rl_readline_name;
extern int rl_echoing_p;
extern int rl_insert_mode;
extern int rl_done;
extern int rl_pending_input;

/* Current line, cursor, and mark. */
extern char *rl_line_buffer;
extern int rl_point;
extern int rl_end;
extern int rl_mark;

/* Prompt strings.  rl_prompt is owned by the library after rl_set_prompt();
 * rl_display_prompt normally aliases rl_prompt. */
extern char *rl_prompt;
extern char *rl_display_prompt;

/* Last dispatched command and the key sequence being dispatched. */
extern rl_command_func_t *rl_last_func;
extern char *rl_executing_keyseq;
extern int rl_key_sequence_length;

/* Lifecycle and prompt management. */
extern int rl_initialize(void);
extern int rl_set_prompt(const char *);
extern int rl_expand_prompt(char *);
extern void rl_save_prompt(void);
extern void rl_restore_prompt(void);

/* Line-buffer and display operations. */
extern char *readline(const char *);
extern void rl_replace_line(const char *, int);
extern int rl_insert_text(const char *);
extern int rl_delete_text(int, int);
extern char *rl_copy_text(int, int);
extern int rl_kill_text(int, int);
extern void rl_extend_line_buffer(int);
extern int rl_ding(void);
extern int rl_crlf(void);
extern int rl_on_new_line(void);
extern void rl_redisplay(void);
extern int rl_forced_update_display(void);
extern int rl_clear_message(void);
extern int rl_clear_visible_line(void);
extern void rl_free_line_state(void);
extern int rl_reset_line_state(void);

/* Input timing.  Returns the previous timeout in microseconds. */
extern int rl_set_keyboard_input_timeout(int);

/* Completion entry points.  rl_complete_internal takes a command character:
 * '\t' (complete), '?' (list), '*' (insert all), '!'/'@' (complete and list).
 * The menu functions cycle through the match list. */
extern int rl_complete(int, int);
extern int rl_complete_internal(int);
extern int rl_possible_completions(int, int);
extern int rl_insert_completions(int, int);
extern int rl_menu_complete(int, int);
extern int rl_backward_menu_complete(int, int);
extern int rl_completion_mode(rl_command_func_t *);

/* Match generators.  Both follow the state protocol: state 0 starts a new
 * query and each subsequent call returns one malloc-compatible string, with
 * NULL marking the end.  rl_completion_matches returns a malloc-compatible,
 * NULL-terminated char ** whose slot 0 is the substitution text. */
extern char **rl_completion_matches(const char *, rl_compentry_func_t *);
extern char *rl_filename_completion_function(const char *, int);
extern char *rl_username_completion_function(const char *, int);

/* Print an argv-style match list in columns.  Application display hooks
 * receive (matches, num_matches, max_length).  rl_display_match_list prints
 * matches[1..len]; slot 0 is the common prefix. */
extern void rl_display_match_list(char **, int, int);

/* Completion state.  rl_filename_completion_desired and
 * rl_filename_quoting_desired are reset by each completion attempt and may
 * only be changed inside an application completion function. */
extern rl_compentry_func_t *rl_completion_entry_function;
extern rl_compentry_func_t *rl_menu_completion_entry_function;
extern rl_completion_func_t *rl_attempted_completion_function;
extern rl_compdisp_func_t *rl_completion_display_matches_hook;
extern rl_cpvfunc_t *rl_completion_word_break_hook;

extern rl_dequote_func_t *rl_filename_rewrite_hook;
extern rl_icppfunc_t *rl_filename_stat_hook;
extern rl_icppfunc_t *rl_directory_rewrite_hook;
extern rl_dequote_func_t *rl_filename_dequoting_function;
extern rl_quote_func_t *rl_filename_quoting_function;
extern rl_linebuf_func_t *rl_char_is_quoted_p;

extern const char *rl_basic_word_break_characters;
extern const char *rl_completer_word_break_characters;
extern const char *rl_basic_quote_characters;
extern const char *rl_completer_quote_characters;
extern const char *rl_filename_quote_characters;
extern const char *rl_special_prefixes;

extern int rl_attempted_completion_over;
extern int rl_completion_type;
extern int rl_completion_invoking_key;
extern int rl_completion_suppress_append;
extern int rl_completion_suppress_quote;
extern int rl_completion_append_character;
extern int rl_completion_quote_character;
extern int rl_completion_found_quote;
extern int rl_completion_mark_symlink_dirs;
extern int rl_completion_query_items;
extern int rl_filename_completion_desired;
extern int rl_filename_quoting_desired;
extern int rl_full_quoting_desired;
extern int rl_ignore_completion_duplicates;
extern int rl_sort_completion_matches;
extern int rl_inhibit_completion;

/* Readline hooks.  Hooks that belong to a host input loop (event, input
 * availability, character input, terminal prep/deprep) are storage in the
 * standalone library; the host readline integration calls them. */
extern rl_hook_func_t *rl_startup_hook;
extern rl_hook_func_t *rl_pre_input_hook;
extern rl_hook_func_t *rl_event_hook;
extern rl_hook_func_t *rl_input_available_hook;
extern rl_getc_func_t *rl_getc_function;
extern rl_voidfunc_t *rl_redisplay_function;
extern rl_vintfunc_t *rl_prep_term_function;
extern rl_voidfunc_t *rl_deprep_term_function;

/* Refresh screen dimensions and repaint through rl_redisplay_function. */
extern void rl_resize_terminal(void);

#ifdef __cplusplus
}
#endif

#endif /* RULINE_READLINE_ABI_READLINE_H */
