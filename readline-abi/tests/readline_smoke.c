/* C ABI smoke test for ruline's readline-compatible library.
 *
 * Build (from the repository root, after `cargo build -p readline-abi`):
 *   cc -std=c11 -Wall -Wextra -Ireadline-abi/include \
 *      readline-abi/tests/readline_smoke.c \
 *      -Ltarget/debug -lreadline -Wl,-rpath,"$PWD/target/debug" \
 *      -o target/readline-smoke
 *
 * The shared object's SONAME is libreadline.so.8, so scripts/smoke.sh also
 * creates a libreadline.so.8 symlink before running the binary.
 */

#define _POSIX_C_SOURCE 200809L

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <readline/history.h>
#include <readline/readline.h>

#define CHECK(condition)                                                       \
    do {                                                                       \
        if (!(condition)) {                                                    \
            fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__,            \
                    #condition);                                               \
            return 1;                                                          \
        }                                                                      \
    } while (0)

static void free_matches(char **matches) {
    size_t index;
    if (matches == NULL) {
        return;
    }
    for (index = 0; matches[index] != NULL; index++) {
        free(matches[index]);
    }
    free(matches);
}

static char *duplicate_string(const char *value) {
    size_t length = strlen(value) + 1;
    char *copy = malloc(length);
    if (copy != NULL) {
        memcpy(copy, value, length);
    }
    return copy;
}

/* --- Completion fixtures ------------------------------------------------- */

static int generator_calls;

static char *two_name_generator(const char *text, int state) {
    static const char *names[] = {"alpha", "alpine"};
    (void)text;
    generator_calls++;
    if (state >= 0 && state < 2) {
        return duplicate_string(names[state]);
    }
    return NULL;
}

static char *empty_generator(const char *text, int state) {
    (void)text;
    (void)state;
    return NULL;
}

static char *single_name_generator(const char *text, int state) {
    (void)text;
    if (state == 0) {
        return duplicate_string("solo");
    }
    return NULL;
}

/* Attempted completion returning one replacement. */
static char **attempted_single(const char *text, int start, int end) {
    char **matches = malloc(2 * sizeof(char *));
    (void)text;
    (void)start;
    (void)end;
    if (matches == NULL) {
        return NULL;
    }
    matches[0] = duplicate_string("foobar");
    matches[1] = NULL;
    return matches;
}

/* Attempted completion returning a common prefix plus two candidates. */
static char **attempted_two(const char *text, int start, int end) {
    char **matches = malloc(4 * sizeof(char *));
    (void)text;
    (void)start;
    (void)end;
    if (matches == NULL) {
        return NULL;
    }
    matches[0] = duplicate_string("alp");
    matches[1] = duplicate_string("alpha");
    matches[2] = duplicate_string("alpine");
    matches[3] = NULL;
    return matches;
}

static char *recorded_text;
static int recorded_start;
static int recorded_end;
static int recorded_calls;
static int recorded_found_quote;
static int recorded_quote_character;

static char **attempted_recording(const char *text, int start, int end) {
    free(recorded_text);
    recorded_text = duplicate_string(text);
    recorded_start = start;
    recorded_end = end;
    recorded_calls++;
    recorded_found_quote = rl_completion_found_quote;
    recorded_quote_character = rl_completion_quote_character;
    rl_attempted_completion_over = 1;
    return NULL;
}

/* Attempted completion that opts into filename quoting for a spaced match. */
static char **attempted_spaced(const char *text, int start, int end) {
    char **matches = malloc(2 * sizeof(char *));
    (void)text;
    (void)start;
    (void)end;
    if (matches == NULL) {
        return NULL;
    }
    matches[0] = duplicate_string("space name");
    matches[1] = NULL;
    rl_filename_completion_desired = 1;
    return matches;
}

static char hook_prefix[64];
static char hook_first[64];
static char hook_second[64];
static int hook_count;
static int hook_max;

static void copy_hook_string(char *destination, size_t size, const char *source) {
    if (source == NULL) {
        destination[0] = '\0';
        return;
    }
    snprintf(destination, size, "%s", source);
}

/* The match array is only valid during the hook call, so copy what the test
 * needs before returning. */
static void display_hook(char **matches, int count, int max) {
    copy_hook_string(hook_prefix, sizeof(hook_prefix), matches[0]);
    copy_hook_string(hook_first, sizeof(hook_first), count >= 1 ? matches[1] : NULL);
    copy_hook_string(hook_second, sizeof(hook_second), count >= 2 ? matches[2] : NULL);
    hook_count = count;
    hook_max = max;
}

static int redisplay_calls;

static void redisplay_hook(void) { redisplay_calls++; }

static int test_state_and_text(void) {
    CHECK(rl_readline_version == RL_READLINE_VERSION);
    CHECK(RL_READLINE_VERSION == 0x0803);
    CHECK(RL_VERSION_MAJOR == 8);
    CHECK(RL_VERSION_MINOR == 3);
    CHECK(rl_library_version != NULL);
    CHECK(strcmp(rl_library_version, "8.3") == 0);
    CHECK(rl_readline_name != NULL);
    CHECK(rl_echoing_p == 1);
    CHECK(rl_last_func == NULL);
    CHECK(rl_key_sequence_length == 0);
    CHECK(rl_executing_keyseq == NULL);

    CHECK(rl_initialize() == 0);
    CHECK(rl_line_buffer != NULL);
    CHECK(rl_point == 0);
    CHECK(rl_end == 0);
    CHECK(rl_mark == 0);
    CHECK(rl_done == 0);
    CHECK(rl_pending_input == 0);
    CHECK(rl_insert_mode == 1);
    CHECK((rl_readline_state & RL_STATE_INITIALIZED) != 0);

    /* Pending EOF must short-circuit without touching the terminal. */
    rl_pending_input = EOF;
    CHECK(readline(NULL) == NULL);
    CHECK(rl_pending_input == 0);
    CHECK(rl_done == 1);
    CHECK((rl_readline_state & RL_STATE_EOF) != 0);
    CHECK(rl_initialize() == 0);
    CHECK(rl_done == 0);

    CHECK(rl_set_prompt("$ ") == 0);
    CHECK(rl_prompt != NULL);
    CHECK(strcmp(rl_prompt, "$ ") == 0);
    CHECK(rl_display_prompt != NULL);
    CHECK(strcmp(rl_display_prompt, "$ ") == 0);
    CHECK(rl_expand_prompt(rl_prompt) == 2);

    CHECK(rl_insert_text("hello world") == 11);
    CHECK(rl_end == 11);
    CHECK(rl_point == 11);
    CHECK(strcmp(rl_line_buffer, "hello world") == 0);

    rl_point = 5;
    CHECK(rl_insert_text("!") == 1);
    CHECK(rl_end == 12);
    CHECK(rl_point == 6);
    CHECK(strcmp(rl_line_buffer, "hello! world") == 0);

    rl_replace_line("abc", 0);
    CHECK(rl_end == 3);
    CHECK(rl_point == 3);
    CHECK(strcmp(rl_line_buffer, "abc") == 0);

    rl_point = 1;
    CHECK(rl_delete_text(1, 2) == 1);
    CHECK(rl_end == 2);
    CHECK(strcmp(rl_line_buffer, "ac") == 0);

    {
        char *copy = rl_copy_text(0, 2);
        CHECK(copy != NULL);
        CHECK(strcmp(copy, "ac") == 0);
        free(copy);
    }

    rl_replace_line("abc", 0);
    rl_point = 1;
    rl_mark = 2;
    CHECK(rl_kill_text(0, 1) == 1);
    CHECK(rl_end == 2);
    CHECK(strcmp(rl_line_buffer, "bc") == 0);
    CHECK(rl_mark <= rl_end);

    rl_replace_line("xy", 0);
    CHECK(rl_delete_text(0, 2) == 2);
    CHECK(rl_end == 0);
    CHECK(strcmp(rl_line_buffer, "") == 0);

    rl_extend_line_buffer(64);

    {
        int previous = rl_set_keyboard_input_timeout(1234);
        CHECK(previous == 100000);
        CHECK(rl_set_keyboard_input_timeout(previous) == 1234);
    }

    rl_save_prompt();
    CHECK(rl_set_prompt("temporary") == 0);
    CHECK(strcmp(rl_prompt, "temporary") == 0);
    rl_restore_prompt();
    CHECK(rl_prompt != NULL);
    CHECK(strcmp(rl_prompt, "$ ") == 0);

    CHECK(rl_on_new_line() == 0);
    CHECK(rl_reset_line_state() == 0);
    CHECK(rl_clear_message() == 0);
    CHECK(rl_clear_visible_line() == 0);
    CHECK(rl_forced_update_display() == 0);
    CHECK(rl_ding() == 0);
    CHECK(rl_crlf() == 0);
    rl_free_line_state();

    return 0;
}

static int keymap_test_command(int count, int key) {
    (void)count;
    (void)key;
    return 0;
}

static int test_keymap_and_binding(void) {
    char path[512];
    char buffer[64];
    const char *tmp = getenv("TMPDIR");
    FILE *file;
    Keymap map;
    Keymap copy;
    rl_command_func_t *function;
    char **sequences;
    int length = 0;
    int type = -1;
    int found;
    int i;

    if (tmp == NULL || *tmp == '\0') {
        tmp = "/tmp";
    }

    CHECK(KEYMAP_SIZE == 257);
    CHECK(ANYOTHERKEY == 256);
    CHECK(ISFUNC == 0);
    CHECK(ISKMAP == 1);
    CHECK(ISMACR == 2);

    CHECK(rl_initialize() == 0);
    CHECK(rl_get_keymap() == emacs_standard_keymap);
    CHECK(rl_get_keymap_by_name("emacs") == emacs_standard_keymap);
    CHECK(rl_get_keymap_by_name("emacs-standard") == emacs_standard_keymap);
    CHECK(rl_get_keymap_by_name("EMACS-META") == emacs_meta_keymap);
    CHECK(rl_get_keymap_by_name("emacs-ctlx") == emacs_ctlx_keymap);
    CHECK(rl_get_keymap_by_name("vi") == vi_movement_keymap);
    CHECK(rl_get_keymap_by_name("vi-insert") == vi_insertion_keymap);
    CHECK(rl_get_keymap_by_name("no-such-map") == NULL);
    CHECK(strcmp(rl_get_keymap_name(emacs_standard_keymap), "emacs") == 0);
    CHECK(strcmp(rl_get_keymap_name(vi_insertion_keymap), "vi-insert") == 0);
    CHECK(rl_get_keymap_name(NULL) == NULL);

    /* GNU's C-x and C-[ slots indirect through the sub-keymaps. */
    CHECK(emacs_standard_keymap[24].type == ISKMAP);
    CHECK((Keymap)emacs_standard_keymap[24].function == emacs_ctlx_keymap);
    CHECK(emacs_standard_keymap[27].type == ISKMAP);
    CHECK((Keymap)emacs_standard_keymap[27].function == emacs_meta_keymap);

    function = rl_named_function("beginning-of-line");
    CHECK(function != NULL);
    CHECK(rl_function_of_keyseq("\\C-a", emacs_standard_keymap, &type) == function);
    CHECK(type == ISFUNC);
    CHECK(rl_function_of_keyseq("\001", emacs_standard_keymap, &type) == function);
    CHECK(rl_named_function("BEGINNING-OF-LINE") == function);
    CHECK(rl_named_function("no-such-function") == NULL);

    CHECK(rl_add_defun("ruline-test-command", keymap_test_command, -1) == 0);
    CHECK(rl_named_function("ruline-test-command") == keymap_test_command);

    CHECK(rl_bind_keyseq("\\C-xy", keymap_test_command) == 0);
    CHECK(rl_function_of_keyseq("\\C-xy", NULL, &type) == keymap_test_command);
    CHECK(type == ISFUNC);

    sequences = rl_invoking_keyseqs(keymap_test_command);
    CHECK(sequences != NULL);
    found = 0;
    for (i = 0; sequences[i]; i++) {
        if (strcmp(sequences[i], "\\C-xy") == 0) {
            found = 1;
        }
        free(sequences[i]);
    }
    free(sequences);
    CHECK(found);

    CHECK(rl_parse_and_bind("\"\\C-x\\C-r\": re-read-init-file") == 0);
    CHECK(rl_function_of_keyseq("\\C-x\\C-r", NULL, &type) ==
          rl_named_function("re-read-init-file"));
    CHECK(type == ISFUNC);

    CHECK(rl_parse_and_bind("\"\\C-xm\": \"hi\"") == 0);
    function = rl_function_of_keyseq("\\C-xm", NULL, &type);
    CHECK(type == ISMACR);
    CHECK(function != NULL);
    {
        union {
            rl_command_func_t *function;
            char *text;
        } macro;
        macro.function = function;
        CHECK(strcmp(macro.text, "hi") == 0);
    }

    CHECK(rl_translate_keyseq("\\C-x", buffer, &length) == 0);
    CHECK(length == 1);
    CHECK(buffer[0] == 0x18);
    CHECK(buffer[1] == 0);
    CHECK(rl_translate_keyseq("\\M-x", buffer, &length) == 0);
    CHECK(length == 2);
    CHECK(buffer[0] == 0x1b);
    CHECK(buffer[1] == 'x');
    CHECK(rl_translate_keyseq("\\e[1;5C", buffer, &length) == 0);
    CHECK(length == 6);
    CHECK(memcmp(buffer, "\033[1;5C", 6) == 0);
    CHECK(rl_translate_keyseq("^?", buffer, &length) == 0);
    CHECK(length == 1);
    CHECK((unsigned char)buffer[0] == 0x7f);
    CHECK(rl_translate_keyseq("\"\\C-x\\C-r\"", buffer, &length) == 0);
    CHECK(length == 2);
    CHECK(buffer[0] == 0x18);
    CHECK(buffer[1] == 0x12);

    CHECK(strcmp(rl_variable_value("convert-meta"), "on") == 0);
    CHECK(rl_variable_bind("convert-meta", "off") == 0);
    CHECK(strcmp(rl_variable_value("convert-meta"), "off") == 0);
    CHECK(rl_translate_keyseq("\\M-x", buffer, &length) == 0);
    CHECK(length == 1);
    CHECK((unsigned char)buffer[0] == 0xf8);
    CHECK(rl_variable_bind("convert-meta", "on") == 0);
    CHECK(rl_variable_bind("editing-mode", "vi") == 0);
    CHECK(rl_get_keymap() == vi_insertion_keymap);
    CHECK(strcmp(rl_variable_value("editing-mode"), "vi") == 0);
    CHECK(rl_variable_bind("editing-mode", "emacs") == 0);
    CHECK(rl_get_keymap() == emacs_standard_keymap);
    CHECK(rl_variable_bind("keymap", "emacs-meta") == 0);
    CHECK(rl_get_keymap() == emacs_meta_keymap);
    CHECK(rl_variable_bind("keymap", "emacs") == 0);
    CHECK(rl_get_keymap() == emacs_standard_keymap);
    CHECK(rl_variable_value("no-such-variable") == NULL);

    map = rl_make_bare_keymap();
    CHECK(map != NULL);
    CHECK(rl_empty_keymap(map) == 1);
    CHECK(rl_bind_key_in_map('a', keymap_test_command, map) == 0);
    CHECK(rl_empty_keymap(map) == 0);
    CHECK(map['a'].type == ISFUNC);
    CHECK(map['a'].function == keymap_test_command);
    copy = rl_copy_keymap(map);
    CHECK(copy != NULL);
    CHECK(copy['a'].function == keymap_test_command);
    CHECK(rl_unbind_function_in_map(keymap_test_command, copy) == 1);
    CHECK(copy['a'].function == NULL);
    CHECK(rl_unbind_key_in_map('a', map) == 0);
    CHECK(map['a'].function == NULL);
    rl_free_keymap(copy);
    rl_free_keymap(map);

    snprintf(path, sizeof(path), "%s/ruline-inputrc-%ld", tmp, (long)getpid());
    unlink(path);
    file = fopen(path, "w");
    CHECK(file != NULL);
    fputs("set editing-mode emacs\n", file);
    fputs("\"\\C-xt\": transpose-chars\n", file);
    fputs("$if mode=emacs\n", file);
    fputs("\"\\C-xd\": dump-functions\n", file);
    fputs("$else\n", file);
    fputs("\"\\C-xk\": kill-line\n", file);
    fputs("$endif\n", file);
    fclose(file);

    CHECK(rl_read_init_file(path) == 0);
    CHECK(rl_function_of_keyseq("\\C-xt", NULL, &type) ==
          rl_named_function("transpose-chars"));
    CHECK(rl_function_of_keyseq("\\C-xd", NULL, &type) ==
          rl_named_function("dump-functions"));
    CHECK(rl_re_read_init_file(0, 0) == 0);
    unlink(path);

    rl_print_keybinding("ruline-test-command", emacs_standard_keymap, 1);
    rl_print_keybinding("ruline-test-command", emacs_standard_keymap, 0);
    return 0;
}

static int test_completion(void) {
    char **matches;
    char directory[512];
    char text[600];
    char expected[700];
    char *item;
    int state;
    int saw_alpha = 0;
    int saw_alpine = 0;
    const char *tmp = getenv("TMPDIR");

    if (tmp == NULL || *tmp == '\0') {
        tmp = "/tmp";
    }

    /* Defaults. */
    CHECK(rl_completion_query_items == 100);
    CHECK(rl_completion_append_character == ' ');
    CHECK(rl_completion_type == 0);
    CHECK(rl_completion_found_quote == 0);
    CHECK(rl_completer_word_break_characters != NULL);
    CHECK(rl_filename_completion_desired == 0);
    CHECK(rl_filename_quoting_desired == 1);
    CHECK(rl_completion_entry_function == NULL);
    CHECK(rl_attempted_completion_function == NULL);
    CHECK(rl_startup_hook == NULL);
    CHECK(rl_pre_input_hook == NULL);
    CHECK(rl_event_hook == NULL);
    CHECK(rl_redisplay_function != NULL);
    CHECK(rl_getc_function == NULL);

    /* The generator protocol: state 0, then 1.., then NULL. */
    generator_calls = 0;
    matches = rl_completion_matches("al", two_name_generator);
    CHECK(matches != NULL);
    CHECK(generator_calls == 3);
    CHECK(strcmp(matches[0], "alp") == 0);
    CHECK(strcmp(matches[1], "alpha") == 0);
    CHECK(strcmp(matches[2], "alpine") == 0);
    CHECK(matches[3] == NULL);
    free_matches(matches);

    matches = rl_completion_matches("none", empty_generator);
    CHECK(matches == NULL);

    matches = rl_completion_matches("solo", single_name_generator);
    CHECK(matches != NULL);
    CHECK(strcmp(matches[0], "solo") == 0);
    CHECK(matches[1] == NULL);
    free_matches(matches);

    /* rl_complete_internal uses the attempted completion function. */
    rl_replace_line("fo", 0);
    rl_point = rl_end;
    rl_attempted_completion_function = attempted_single;
    CHECK(rl_complete_internal('\t') == 0);
    CHECK(strcmp(rl_line_buffer, "foobar ") == 0);
    CHECK((rl_readline_state & RL_STATE_COMPLETING) == 0);
    rl_attempted_completion_function = NULL;

    /* Display hooks receive the candidate array, count and max width. */
    rl_replace_line("al", 0);
    rl_point = rl_end;
    rl_attempted_completion_function = attempted_two;
    rl_completion_display_matches_hook = display_hook;
    hook_prefix[0] = '\0';
    hook_first[0] = '\0';
    hook_second[0] = '\0';
    hook_count = 0;
    hook_max = 0;
    CHECK(rl_complete_internal('?') == 0);
    CHECK(strcmp(hook_prefix, "alp") == 0);
    CHECK(strcmp(hook_first, "alpha") == 0);
    CHECK(strcmp(hook_second, "alpine") == 0);
    CHECK(hook_count == 2);
    CHECK(hook_max == 6);
    rl_completion_display_matches_hook = NULL;
    rl_attempted_completion_function = NULL;

    /* The redisplay hook is the repaint indirection. */
    {
        rl_voidfunc_t *saved_redisplay = rl_redisplay_function;
        redisplay_calls = 0;
        rl_redisplay_function = redisplay_hook;
        CHECK(rl_forced_update_display() == 0);
        CHECK(redisplay_calls == 1);
        rl_resize_terminal();
        CHECK(redisplay_calls == 2);
        rl_redisplay_function = saved_redisplay;
    }

    /* Quote characters: the attempted function sees the quoted substring and
     * the quote character that opened it. */
    rl_replace_line("cat 'word", 0);
    rl_point = rl_end;
    rl_completer_quote_characters = "\"'";
    recorded_calls = 0;
    rl_attempted_completion_function = attempted_recording;
    CHECK(rl_complete_internal('\t') == 0);
    CHECK(recorded_calls == 1);
    CHECK(recorded_text != NULL);
    CHECK(strcmp(recorded_text, "word") == 0);
    CHECK(recorded_start == 5);
    CHECK(recorded_end == 9);
    CHECK(recorded_found_quote != 0);
    CHECK(recorded_quote_character == '\'');
    CHECK(strcmp(rl_line_buffer, "cat 'word") == 0);
    rl_attempted_completion_function = NULL;
    rl_completer_quote_characters = NULL;

    /* Filename completion joins the typed directory with each entry. */
    snprintf(directory, sizeof(directory), "%s/ruline-completion-%ld", tmp,
             (long)getpid());
    (void)mkdir(directory, 0700);
    snprintf(text, sizeof(text), "%s/alpha", directory);
    {
        FILE *file = fopen(text, "w");
        CHECK(file != NULL);
        fputs("a", file);
        fclose(file);
    }
    snprintf(text, sizeof(text), "%s/alpine", directory);
    {
        FILE *file = fopen(text, "w");
        CHECK(file != NULL);
        fputs("a", file);
        fclose(file);
    }
    snprintf(text, sizeof(text), "%s/sub", directory);
    (void)mkdir(text, 0700);

    rl_completion_found_quote = 0;
    rl_completion_quote_character = 0;
    rl_completer_quote_characters = NULL;
    snprintf(text, sizeof(text), "%s/al", directory);
    state = 0;
    while ((item = rl_filename_completion_function(text, state++)) != NULL) {
        snprintf(expected, sizeof(expected), "%s/alpha", directory);
        if (strcmp(item, expected) == 0) {
            saw_alpha = 1;
        }
        snprintf(expected, sizeof(expected), "%s/alpine", directory);
        if (strcmp(item, expected) == 0) {
            saw_alpine = 1;
        }
        free(item);
    }
    CHECK(saw_alpha);
    CHECK(saw_alpine);

    /* Directories are marked with the append character replaced by '/'. */
    snprintf(text, sizeof(text), "%s/su", directory);
    rl_replace_line(text, 0);
    rl_point = rl_end;
    CHECK(rl_complete_internal('\t') == 0);
    snprintf(expected, sizeof(expected), "%s/sub/", directory);
    CHECK(strcmp(rl_line_buffer, expected) == 0);

    /* Filename quoting uses rl_filename_quote_characters and the completer
     * quote characters. */
    rl_completer_quote_characters = "\"'";
    rl_filename_quote_characters = " ";
    rl_replace_line("space", 0);
    rl_point = rl_end;
    rl_attempted_completion_function = attempted_spaced;
    CHECK(rl_complete_internal('\t') == 0);
    CHECK(rl_line_buffer[0] == '"');
    CHECK(strstr(rl_line_buffer, "space name") != NULL);
    CHECK(rl_line_buffer[strlen(rl_line_buffer) - 1] == '"' ||
          rl_line_buffer[strlen(rl_line_buffer) - 2] == '"');
    rl_attempted_completion_function = NULL;
    rl_filename_quote_characters = NULL;
    rl_completer_quote_characters = NULL;

    /* Menu completion cycles through the candidate list. */
    rl_replace_line("al", 0);
    rl_point = rl_end;
    rl_attempted_completion_function = attempted_two;
    rl_last_func = rl_menu_complete;
    CHECK(rl_menu_complete(1, '\t') == 0);
    CHECK(strncmp(rl_line_buffer, "alpha", 5) == 0);
    rl_last_func = rl_menu_complete;
    CHECK(rl_menu_complete(1, '\t') == 0);
    CHECK(strncmp(rl_line_buffer, "alpine", 6) == 0);
    rl_last_func = rl_backward_menu_complete;
    CHECK(rl_backward_menu_complete(1, '\t') == 0);
    CHECK(strncmp(rl_line_buffer, "alpha", 5) == 0);
    rl_last_func = NULL;
    rl_attempted_completion_function = NULL;

    /* Empty word + filename completion is stable (no crash). */
    rl_replace_line("", 0);
    rl_point = rl_end;
    rl_completer_word_break_characters = " ";
    CHECK(rl_completion_mode(rl_complete) == '\t');
    rl_completer_word_break_characters = NULL;
    rl_initialize();
    CHECK(rl_completer_word_break_characters != NULL);
    CHECK(strcmp(rl_completer_word_break_characters,
                 rl_basic_word_break_characters) == 0);

    snprintf(text, sizeof(text), "%s/alpha", directory);
    unlink(text);
    snprintf(text, sizeof(text), "%s/alpine", directory);
    unlink(text);
    snprintf(text, sizeof(text), "%s/sub", directory);
    rmdir(text);
    rmdir(directory);
    free(recorded_text);
    recorded_text = NULL;
    return 0;
}

static int test_history(void) {
    char path[512];
    char timestamp_path[512];
    const char *tmp = getenv("TMPDIR");
    HIST_ENTRY *entry;

    if (tmp == NULL || *tmp == '\0') {
        tmp = "/tmp";
    }

    CHECK(history_expansion_char == '!');
    CHECK(history_subst_char == '^');
    CHECK(history_write_timestamps == 0);
    CHECK(history_multiline_entries == 0);
    CHECK(history_file_version == 1);
    CHECK(history_word_delimiters != NULL);
    CHECK(history_no_expand_chars != NULL);

    clear_history();
    CHECK(history_length == 0);
    CHECK(history_offset == 0);
    CHECK(history_base == 1);
    CHECK(where_history() == 0);
    CHECK(current_history() == NULL);
    CHECK(previous_history() == NULL);
    CHECK(next_history() == NULL);

    add_history("one");
    add_history("two");
    add_history("three");
    CHECK(history_length == 3);
    CHECK(history_total_bytes() > 0);

    using_history();
    CHECK(where_history() == 3);
    CHECK(current_history() == NULL);

    entry = previous_history();
    CHECK(entry != NULL);
    CHECK(strcmp(entry->line, "three") == 0);
    CHECK(where_history() == 2);

    entry = previous_history();
    CHECK(entry != NULL);
    CHECK(strcmp(entry->line, "two") == 0);
    CHECK(where_history() == 1);

    entry = next_history();
    CHECK(entry != NULL);
    CHECK(strcmp(entry->line, "three") == 0);
    CHECK(where_history() == 2);

    entry = history_get(1);
    CHECK(entry != NULL);
    CHECK(strcmp(entry->line, "one") == 0);
    CHECK(history_get(0) == NULL);
    CHECK(history_get(4) == NULL);

    add_history_time("#123");
    entry = history_get(3);
    CHECK(entry != NULL);
    CHECK(history_get_time(entry) == (time_t)123);

    CHECK(history_set_pos(0) == 1);
    CHECK(where_history() == 0);
    CHECK(history_set_pos(99) == 0);

    stifle_history(2);
    CHECK(history_is_stifled() == 1);
    CHECK(history_length == 2);
    CHECK(history_max_entries == 2);
    CHECK(max_input_history == 2);
    CHECK(history_get(1) != NULL);
    CHECK(strcmp(history_get(1)->line, "two") == 0);
    CHECK(history_get(2) != NULL);
    CHECK(strcmp(history_get(2)->line, "three") == 0);
    CHECK(unstifle_history() == 2);
    CHECK(history_is_stifled() == 0);

    snprintf(path, sizeof(path), "%s/ruline-history-%ld", tmp, (long)getpid());
    unlink(path);
    CHECK(write_history(path) == 0);
    CHECK(history_lines_written_to_file == 2);

    clear_history();
    CHECK(history_length == 0);
    CHECK(read_history(path) == 0);
    CHECK(history_lines_read_from_file == 2);
    CHECK(history_length == 2);
    using_history();
    entry = history_get(1);
    CHECK(entry != NULL);
    CHECK(strcmp(entry->line, "two") == 0);
    entry = history_get(2);
    CHECK(entry != NULL);
    CHECK(strcmp(entry->line, "three") == 0);

    CHECK(history_truncate_file(path, 1) == 0);
    clear_history();
    CHECK(read_history(path) == 0);
    CHECK(history_length == 1);
    using_history();
    entry = history_get(1);
    CHECK(entry != NULL);
    CHECK(strcmp(entry->line, "three") == 0);

    snprintf(timestamp_path, sizeof(timestamp_path), "%s/ruline-history-ts-%ld",
             tmp, (long)getpid());
    unlink(timestamp_path);
    history_write_timestamps = 1;
    CHECK(write_history(timestamp_path) == 0);
    history_write_timestamps = 0;
    clear_history();
    CHECK(read_history(timestamp_path) == 0);
    CHECK(history_length == 1);
    using_history();
    entry = history_get(1);
    CHECK(entry != NULL);
    CHECK(strcmp(entry->line, "three") == 0);

    unlink(timestamp_path);
    unlink(path);
    clear_history();
    return 0;
}

int main(void) {
    if (test_state_and_text() != 0) {
        return 1;
    }
    if (test_keymap_and_binding() != 0) {
        return 1;
    }
    if (test_completion() != 0) {
        return 1;
    }
    if (test_history() != 0) {
        return 1;
    }
    puts("readline ABI smoke test: OK");
    return 0;
}
