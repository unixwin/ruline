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

#include <stddef.h>

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

/* Keymap layout.  One entry exists for every byte value plus a final
 * ANYOTHERKEY shadow slot used when a longer sequence does not match. */
typedef struct _keymap_entry {
    unsigned char type;
    rl_command_func_t *function;
} KEYMAP_ENTRY;

#define KEYMAP_SIZE 257
#define ANYOTHERKEY (KEYMAP_SIZE - 1)

typedef KEYMAP_ENTRY KEYMAP_ENTRY_ARRAY[KEYMAP_SIZE];
typedef KEYMAP_ENTRY *Keymap;

#define ISFUNC 0
#define ISKMAP 1
#define ISMACR 2

extern KEYMAP_ENTRY_ARRAY emacs_standard_keymap;
extern KEYMAP_ENTRY_ARRAY emacs_meta_keymap;
extern KEYMAP_ENTRY_ARRAY emacs_ctlx_keymap;
extern KEYMAP_ENTRY_ARRAY vi_insertion_keymap;
extern KEYMAP_ENTRY_ARRAY vi_movement_keymap;

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

/* Keymap and funmap management. */
extern Keymap rl_get_keymap(void);
extern void rl_set_keymap(Keymap);
extern Keymap rl_get_keymap_by_name(const char *);
extern char *rl_get_keymap_name(Keymap);
extern int rl_set_keymap_name(const char *, Keymap);
extern void rl_set_keymap_from_edit_mode(void);
extern char *rl_get_keymap_name_from_edit_mode(void);
extern Keymap rl_make_bare_keymap(void);
extern Keymap rl_make_keymap(void);
extern Keymap rl_copy_keymap(Keymap);
extern void rl_discard_keymap(Keymap);
extern void rl_free_keymap(Keymap);
extern int rl_empty_keymap(Keymap);

extern int rl_add_defun(const char *, rl_command_func_t *, int);
extern int rl_add_funmap_entry(const char *, rl_command_func_t *);
extern const char **rl_funmap_names(void);
extern void rl_initialize_funmap(void);
extern void rl_list_funmap_names(void);
extern rl_command_func_t *rl_named_function(const char *);
extern rl_command_func_t *rl_function_of_keyseq(const char *, Keymap, int *);
extern rl_command_func_t *rl_function_of_keyseq_len(const char *, size_t, Keymap, int *);
extern char **rl_invoking_keyseqs(rl_command_func_t *);
extern char **rl_invoking_keyseqs_in_map(rl_command_func_t *, Keymap);

/* Binding and translation. */
extern int rl_bind_key(int, rl_command_func_t *);
extern int rl_bind_key_in_map(int, rl_command_func_t *, Keymap);
extern int rl_bind_key_if_unbound(int, rl_command_func_t *);
extern int rl_bind_key_if_unbound_in_map(int, rl_command_func_t *, Keymap);
extern int rl_bind_keyseq(const char *, rl_command_func_t *);
extern int rl_bind_keyseq_in_map(const char *, rl_command_func_t *, Keymap);
extern int rl_bind_keyseq_if_unbound(const char *, rl_command_func_t *);
extern int rl_bind_keyseq_if_unbound_in_map(const char *, rl_command_func_t *, Keymap);
extern int rl_generic_bind(int, const char *, char *, Keymap);
extern int rl_macro_bind(const char *, const char *, Keymap);
extern int rl_unbind_key(int);
extern int rl_unbind_key_in_map(int, Keymap);
extern int rl_unbind_function_in_map(rl_command_func_t *, Keymap);
extern int rl_unbind_command_in_map(const char *, Keymap);
extern int rl_translate_keyseq(const char *, char *, int *);
extern char *rl_untranslate_keyseq(int);

/* Variables, inputrc, and dumping. */
extern char *rl_variable_value(const char *);
extern int rl_variable_bind(const char *, const char *);
extern int rl_parse_and_bind(char *);
extern int rl_read_init_file(const char *);
extern int rl_re_read_init_file(int, int);
extern void rl_print_keybinding(const char *, Keymap, int);
extern void rl_function_dumper(int);
extern void rl_macro_dumper(int);
extern void rl_variable_dumper(int);
extern int rl_dump_functions(int, int);
extern int rl_dump_macros(int, int);
extern int rl_dump_variables(int, int);

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

#ifdef __cplusplus
}
#endif

#endif /* RULINE_READLINE_ABI_READLINE_H */
