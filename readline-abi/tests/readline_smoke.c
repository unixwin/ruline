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
#include <signal.h>
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

/* --- Callback / terminal / signal fixtures ------------------------------- */

static const unsigned char *stream_data;
static size_t stream_length;
static size_t stream_position;

static void feed_stream(const void *data, size_t length) {
    stream_data = (const unsigned char *)data;
    stream_length = length;
    stream_position = 0;
}

static int stream_getc(FILE *stream) {
    (void)stream;
    if (stream_position < stream_length) {
        return stream_data[stream_position++];
    }
    return EOF;
}

static char callback_line[256];
static int callback_calls;
static int callback_last_null;
static int callback_last_done;
static int callback_last_eof;

static void reset_callback_records(void) {
    callback_line[0] = '\0';
    callback_calls = 0;
    callback_last_null = -1;
    callback_last_done = -1;
    callback_last_eof = -1;
}

static void callback_handler(char *line) {
    callback_calls++;
    callback_last_null = (line == NULL);
    if (line != NULL) {
        snprintf(callback_line, sizeof(callback_line), "%s", line);
        free(line);
    } else {
        callback_line[0] = '\0';
    }
    callback_last_done = rl_done;
    callback_last_eof = (rl_readline_state & RL_STATE_EOF) != 0;
}

static int callback_redisplay_calls;
static int prep_hook_calls;
static int deprep_hook_calls;

static void callback_redisplay(void) { callback_redisplay_calls++; }
static void callback_prep_hook(int meta) {
    (void)meta;
    prep_hook_calls++;
}
static void callback_deprep_hook(void) { deprep_hook_calls++; }

static int event_hook_calls;
static int available_after;

static int event_counter(void) {
    event_hook_calls++;
    return 0;
}

static int availability_check(void) {
    return event_hook_calls >= available_after;
}

static int probe_calls;
static int probe_saw_dispatching;
static int probe_last_func_matches;
static int probe_key;

static int probe_command(int count, int key) {
    probe_calls++;
    probe_key = key;
    probe_saw_dispatching = (rl_readline_state & RL_STATE_DISPATCHING) != 0;
    probe_last_func_matches = (rl_last_func == probe_command);
    (void)count;
    return 0;
}

static int app_sigint_calls;
static int app_sigint_saw_sighandler;

static void app_sigint_handler(int signal) {
    (void)signal;
    app_sigint_calls++;
    app_sigint_saw_sighandler = (rl_readline_state & RL_STATE_SIGHANDLER) != 0;
}

static void step_callback(void) { rl_callback_read_char(); }

static int test_terminal_and_screen_state(void) {
    int rows = 0;
    int columns = 0;
    int pinned_rows = 0;
    int pinned_columns = 0;
    int is_tty;

    CHECK(rl_initialize() == 0);

    rl_get_screen_size(&rows, &columns);
    CHECK(rows > 0);
    CHECK(columns > 0);

    rl_set_screen_size(41, 123);
    rl_get_screen_size(&pinned_rows, &pinned_columns);
    CHECK(pinned_rows == 41);
    CHECK(pinned_columns == 123);
    rl_reset_screen_size();
    rl_get_screen_size(&pinned_rows, &pinned_columns);
    CHECK(pinned_rows > 0);
    CHECK(pinned_columns > 0);

    is_tty = isatty(STDIN_FILENO);
    rl_prep_terminal(0);
    rl_prep_terminal(0); /* idempotent */
    CHECK(((rl_readline_state & RL_STATE_TERMPREPPED) != 0) == (is_tty != 0));
    if (!is_tty) {
        CHECK(rl_echoing_p == 1);
    }

    rl_deprep_terminal();
    rl_deprep_terminal(); /* idempotent */
    CHECK((rl_readline_state & RL_STATE_TERMPREPPED) == 0);

    rl_pending_input = 'q';
    RL_SETSTATE(RL_STATE_INPUTPENDING);
    CHECK(rl_clear_pending_input() == 0);
    CHECK(rl_pending_input == 0);
    CHECK((rl_readline_state & RL_STATE_INPUTPENDING) == 0);
    return 0;
}

static int test_callback_stream(void) {
    rl_getc_func_t *saved_getc = rl_getc_function;
    rl_voidfunc_t *saved_redisplay = rl_redisplay_function;
    rl_vintfunc_t *saved_prep = rl_prep_term_function;
    rl_voidfunc_t *saved_deprep = rl_deprep_term_function;
    int i;

    rl_initialize();
    prep_hook_calls = 0;
    deprep_hook_calls = 0;
    callback_redisplay_calls = 0;
    rl_getc_function = stream_getc;
    rl_redisplay_function = callback_redisplay;
    rl_prep_term_function = callback_prep_hook;
    rl_deprep_term_function = callback_deprep_hook;

    reset_callback_records();
    feed_stream("helX\x7flo\n", 8);
    rl_callback_handler_install("> ", callback_handler);
    CHECK((rl_readline_state & RL_STATE_CALLBACK) != 0);
    CHECK(prep_hook_calls == 1);
    CHECK(callback_redisplay_calls >= 1);

    rl_callback_sigcleanup(); /* no transient state: must be harmless */

    for (i = 0; i < 8; i++) {
        step_callback();
    }

    CHECK(callback_calls == 1);
    CHECK(callback_last_null == 0);
    CHECK(strcmp(callback_line, "hello") == 0);
    CHECK(callback_last_done != 0);
    CHECK(callback_last_eof == 0);
    CHECK(rl_line_buffer[0] == '\0');
    CHECK(rl_done == 0);
    CHECK(rl_point == 0);
    CHECK(rl_end == 0);
    CHECK((rl_readline_state & RL_STATE_CALLBACK) != 0);
    CHECK(prep_hook_calls == 2);
    CHECK(deprep_hook_calls == 1);

    /* Reinstall replaces the handler without re-prepping the terminal. */
    reset_callback_records();
    rl_callback_handler_install("$ ", callback_handler);
    CHECK(prep_hook_calls == 2);
    feed_stream("z\n", 2);
    step_callback();
    step_callback();
    CHECK(callback_calls == 1);
    CHECK(strcmp(callback_line, "z") == 0);

    /* Ctrl-C aborts: NULL line, not EOF. */
    reset_callback_records();
    feed_stream("\003", 1);
    step_callback();
    CHECK(callback_calls == 1);
    CHECK(callback_last_null != 0);
    CHECK(callback_last_eof == 0);
    CHECK(rl_line_buffer[0] == '\0');

    /* Ctrl-D on an empty line is EOF. */
    reset_callback_records();
    feed_stream("\004", 1);
    step_callback();
    CHECK(callback_calls == 1);
    CHECK(callback_last_null != 0);
    CHECK(callback_last_eof != 0);

    /* Ctrl-D on a non-empty line runs delete-char instead of EOF. */
    reset_callback_records();
    feed_stream("abc\004\004\n", 6);
    for (i = 0; i < 6; i++) {
        step_callback();
    }
    CHECK(callback_calls == 1);
    CHECK(callback_last_null == 0);
    CHECK(strcmp(callback_line, "abc") == 0);

    /* rl_pending_input is replayed before the stream. */
    reset_callback_records();
    rl_pending_input = 'x';
    RL_SETSTATE(RL_STATE_INPUTPENDING);
    step_callback();
    CHECK(strcmp(rl_line_buffer, "x") == 0);
    CHECK(rl_pending_input == 0);
    CHECK((rl_readline_state & RL_STATE_INPUTPENDING) == 0);
    feed_stream("\n", 1);
    step_callback();
    CHECK(callback_calls == 1);
    CHECK(strcmp(callback_line, "x") == 0);

    /* Pending EOF completes the fresh line as EOF. */
    reset_callback_records();
    rl_pending_input = EOF;
    step_callback();
    CHECK(callback_calls == 1);
    CHECK(callback_last_null != 0);
    CHECK(callback_last_eof != 0);

    /* Event hook polls availability before reading a byte. */
    reset_callback_records();
    event_hook_calls = 0;
    available_after = 2;
    rl_event_hook = event_counter;
    rl_input_available_hook = availability_check;
    feed_stream("e\n", 2);
    step_callback();
    CHECK(event_hook_calls == 2);
    CHECK(strcmp(rl_line_buffer, "e") == 0);
    step_callback();
    CHECK(event_hook_calls == 2);
    CHECK(callback_calls == 1);
    CHECK(strcmp(callback_line, "e") == 0);
    rl_event_hook = NULL;
    rl_input_available_hook = NULL;

    /* A real command binding is dispatched with the dispatching state bit. */
    reset_callback_records();
    probe_calls = 0;
    probe_saw_dispatching = 0;
    probe_last_func_matches = 0;
    CHECK(rl_bind_key('!', probe_command) == 0);
    feed_stream("!\n", 2);
    step_callback();
    step_callback();
    CHECK(probe_calls == 1);
    CHECK(probe_key == '!');
    CHECK(probe_saw_dispatching != 0);
    CHECK(probe_last_func_matches != 0);
    CHECK(callback_calls == 1);
    CHECK(strcmp(callback_line, "") == 0);
    rl_bind_key('!', rl_named_function("self-insert"));

    rl_callback_handler_remove();
    CHECK((rl_readline_state & RL_STATE_CALLBACK) == 0);
    CHECK(prep_hook_calls == deprep_hook_calls);
    rl_callback_handler_remove(); /* idempotent */
    CHECK(prep_hook_calls == deprep_hook_calls);

    rl_getc_function = saved_getc;
    rl_redisplay_function = saved_redisplay;
    rl_prep_term_function = saved_prep;
    rl_deprep_term_function = saved_deprep;
    return 0;
}

static int test_callback_signals(void) {
    struct sigaction action;
    struct sigaction previous;
    rl_getc_func_t *saved_getc = rl_getc_function;
    rl_voidfunc_t *saved_redisplay = rl_redisplay_function;
    rl_vintfunc_t *saved_prep = rl_prep_term_function;
    rl_voidfunc_t *saved_deprep = rl_deprep_term_function;

    rl_initialize();
    prep_hook_calls = 0;
    deprep_hook_calls = 0;
    callback_redisplay_calls = 0;
    rl_getc_function = stream_getc;
    rl_redisplay_function = callback_redisplay;
    rl_prep_term_function = callback_prep_hook;
    rl_deprep_term_function = callback_deprep_hook;

    reset_callback_records();
    feed_stream("", 0);
    rl_callback_handler_install("> ", callback_handler);

    memset(&action, 0, sizeof(action));
    action.sa_handler = app_sigint_handler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = SA_RESTART;
    CHECK(sigaction(SIGINT, &action, &previous) == 0);

    app_sigint_calls = 0;
    app_sigint_saw_sighandler = 0;
    CHECK(rl_set_signals() == 0);
    CHECK(raise(SIGINT) == 0);
    step_callback();

    CHECK(app_sigint_calls == 1);
    CHECK(app_sigint_saw_sighandler != 0);
    CHECK(callback_calls == 1);
    CHECK(callback_last_null != 0);
    CHECK(callback_last_eof == 0);
    CHECK((rl_readline_state & RL_STATE_SIGHANDLER) == 0);
    CHECK((rl_readline_state & RL_STATE_CALLBACK) != 0);

    /* SIGWINCH refreshes the size and repaints before consuming a key. */
    callback_redisplay_calls = 0;
    CHECK(rl_set_signals() == 0);
    CHECK(raise(SIGWINCH) == 0);
    rl_pending_input = 'w';
    step_callback();
    CHECK(callback_redisplay_calls >= 1);
    CHECK(strcmp(rl_line_buffer, "w") == 0);

    feed_stream("\n", 1);
    step_callback();
    CHECK(callback_calls == 2);
    CHECK(strcmp(callback_line, "w") == 0);

    /* GNU cleanup/reset order, also covered directly. */
    rl_cleanup_after_signal();
    rl_reset_after_signal();
    CHECK((rl_readline_state & RL_STATE_TERMPREPPED) == 0);
    rl_clear_signals();
    CHECK(sigaction(SIGINT, &previous, NULL) == 0);

    rl_callback_handler_remove();
    CHECK(prep_hook_calls == deprep_hook_calls);

    rl_getc_function = saved_getc;
    rl_redisplay_function = saved_redisplay;
    rl_prep_term_function = saved_prep;
    rl_deprep_term_function = saved_deprep;
    return 0;
}

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
    /* GNU's rl_kill_text saves the text on the kill ring and returns 0. */
    CHECK(rl_kill_text(0, 1) == 0);
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

/* GNU rl_function_of_keyseq takes RAW key bytes, not a bind spelling.
 * Translate first; this is what the ABI contract requires. */
static rl_command_func_t *lookup_keyseq(const char *spelling, Keymap map, int *type) {
    char raw[128];
    int length = -1;
    if (rl_translate_keyseq(spelling, raw, &length) != 0 || length < 0) {
        return NULL;
    }
    raw[length] = '\0';
    return rl_function_of_keyseq(raw, map, type);
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
    CHECK(lookup_keyseq("\\C-a", emacs_standard_keymap, &type) == function);
    CHECK(type == ISFUNC);
    CHECK(lookup_keyseq("\001", emacs_standard_keymap, &type) == function);
    CHECK(rl_named_function("BEGINNING-OF-LINE") == function);
    CHECK(rl_named_function("no-such-function") == NULL);

    CHECK(rl_add_defun("ruline-test-command", keymap_test_command, -1) == 0);
    CHECK(rl_named_function("ruline-test-command") == keymap_test_command);

    CHECK(rl_bind_keyseq("\\C-xy", keymap_test_command) == 0);
    CHECK(lookup_keyseq("\\C-xy", NULL, &type) == keymap_test_command);
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
    CHECK(lookup_keyseq("\\C-x\\C-r", NULL, &type) ==
          rl_named_function("re-read-init-file"));
    CHECK(type == ISFUNC);

    CHECK(rl_parse_and_bind("\"\\C-xm\": \"hi\"") == 0);
    function = lookup_keyseq("\\C-xm", NULL, &type);
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
    /* GNU rl_translate_keyseq does not implement caret notation or strip
     * quotes; the bind/inputrc parser handles those before translation. */
    CHECK(rl_translate_keyseq("^?", buffer, &length) == 0);
    CHECK(length == 2);
    CHECK(buffer[0] == '^');
    CHECK(buffer[1] == '?');
    CHECK(rl_translate_keyseq("\"\\C-x\\C-r\"", buffer, &length) == 0);
    CHECK(length == 4);
    CHECK(buffer[0] == '"');
    CHECK(buffer[1] == 0x18);
    CHECK(buffer[2] == 0x12);
    CHECK(buffer[3] == '"');

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
    CHECK(lookup_keyseq("\\C-xt", NULL, &type) ==
          rl_named_function("transpose-chars"));
    CHECK(lookup_keyseq("\\C-xd", NULL, &type) ==
          rl_named_function("dump-functions"));
    CHECK(rl_re_read_init_file(0, 0) == 0);
    unlink(path);

    rl_print_keybinding("ruline-test-command", emacs_standard_keymap, 1);
    rl_print_keybinding("ruline-test-command", emacs_standard_keymap, 0);
    return 0;
}

/* Invoke a bindable command resolved through the funmap. */
static int call_command(int line, const char *name, int count, int key) {
    rl_command_func_t *function = rl_named_function(name);
    if (function == NULL) {
        fprintf(stderr, "FAIL %s:%d: no command named %s\n", __FILE__, line,
                name);
        return 1;
    }
    return function(count, key);
}

static int test_editing_commands(void) {
    rl_command_func_t *forward;
    rl_command_func_t *alias;

    CHECK(rl_initialize() == 0);
    rl_replace_line("", 0);

    /* self-insert uses the dispatched key. */
    CHECK(call_command(__LINE__, "self-insert", 1, 'a') == 0);
    CHECK(call_command(__LINE__, "self-insert", 1, 'b') == 0);
    CHECK(call_command(__LINE__, "self-insert", 1, 'c') == 0);
    CHECK(strcmp(rl_line_buffer, "abc") == 0);
    CHECK(rl_point == 3 && rl_end == 3);

    /* Character and line movement. */
    CHECK(call_command(__LINE__, "backward-char", 1, 0) == 0);
    CHECK(rl_point == 2);
    CHECK(call_command(__LINE__, "forward-char", 1, 0) == 0);
    CHECK(rl_point == 3);
    CHECK(call_command(__LINE__, "beginning-of-line", 1, 0) == 0);
    CHECK(rl_point == 0);
    CHECK(call_command(__LINE__, "end-of-line", 1, 0) == 0);
    CHECK(rl_point == 3);

    /* Word movement follows GNU's alphabetic boundaries. */
    rl_replace_line("one two", 0);
    rl_point = 0;
    CHECK(call_command(__LINE__, "forward-word", 1, 0) == 0);
    CHECK(rl_point == 3);
    CHECK(call_command(__LINE__, "forward-word", 1, 0) == 0);
    CHECK(rl_point == 7);
    CHECK(call_command(__LINE__, "backward-word", 1, 0) == 0);
    CHECK(rl_point == 4);

    /* Mark, explicit set-mark count, and exchange. */
    rl_replace_line("hello", 0);
    rl_point = 2;
    CHECK(call_command(__LINE__, "set-mark", 1, 0) == 0);
    CHECK(rl_mark == 2);
    rl_explicit_arg = 1;
    CHECK(call_command(__LINE__, "set-mark", 0, 0) == 0);
    CHECK(rl_mark == 0);
    CHECK(rl_explicit_arg == 0);
    CHECK(call_command(__LINE__, "exchange-point-and-mark", 1, 0) == 0);
    CHECK(rl_point == 0 && rl_mark == 2);
    CHECK(call_command(__LINE__, "exchange-point-and-mark", 1, 0) == 0);
    CHECK(rl_point == 2 && rl_mark == 0);

    /* Delete-char and backward-delete-char. */
    rl_replace_line("abc", 0);
    rl_point = 1;
    CHECK(call_command(__LINE__, "delete-char", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "ac") == 0);
    CHECK(rl_point == 1);
    CHECK(call_command(__LINE__, "backward-delete-char", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "c") == 0);
    CHECK(rl_point == 0);
    CHECK(call_command(__LINE__, "backward-delete-char", 1, 0) == 1);
    CHECK(call_command(__LINE__, "delete-char", 1, 0) == 0);
    CHECK(rl_end == 0);

    /* delete-horizontal-space removes surrounding spaces and tabs. */
    rl_replace_line("a  \t b", 0);
    rl_point = 4;
    CHECK(call_command(__LINE__, "delete-horizontal-space", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "ab") == 0);
    CHECK(rl_point == 1);

    /* kill-line and yank round-trip. */
    rl_replace_line("hello world", 0);
    rl_point = 5;
    rl_mark = 0;
    CHECK(call_command(__LINE__, "kill-line", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "hello") == 0);
    CHECK(rl_point == 5);
    CHECK(call_command(__LINE__, "yank", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "hello world") == 0);
    CHECK(rl_point == 11);

    /* Two separate kills, then yank-pop rotates to the older entry. */
    rl_replace_line("one two", 0);
    rl_point = 0;
    CHECK(call_command(__LINE__, "kill-word", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, " two") == 0);
    CHECK(call_command(__LINE__, "self-insert", 1, 'x') == 0);
    rl_replace_line("three four", 0);
    rl_point = 0;
    CHECK(call_command(__LINE__, "kill-word", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, " four") == 0);
    CHECK(call_command(__LINE__, "yank", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "three four") == 0);
    CHECK(call_command(__LINE__, "yank-pop", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "one four") == 0);

    /* Unix-style rubouts and whole-line/region kills. */
    rl_replace_line("foo bar", 0);
    rl_point = 7;
    CHECK(call_command(__LINE__, "unix-word-rubout", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "foo ") == 0);
    CHECK(rl_point == 4);

    rl_replace_line("abc", 0);
    rl_point = 3;
    CHECK(call_command(__LINE__, "unix-line-discard", 1, 0) == 0);
    CHECK(rl_end == 0 && rl_point == 0);

    rl_replace_line("abc", 0);
    rl_point = 3;
    CHECK(call_command(__LINE__, "backward-kill-line", 1, 0) == 0);
    CHECK(rl_end == 0 && rl_point == 0);

    rl_replace_line("abc", 0);
    rl_point = 1;
    CHECK(call_command(__LINE__, "kill-whole-line", 1, 0) == 0);
    CHECK(rl_end == 0 && rl_point == 0 && rl_mark == 0);

    rl_replace_line("abcdef", 0);
    rl_point = 1;
    rl_mark = 4;
    CHECK(call_command(__LINE__, "kill-region", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "aef") == 0);
    CHECK(rl_point == 1);

    /* Transposition. */
    rl_replace_line("ab", 0);
    rl_point = 2;
    CHECK(call_command(__LINE__, "transpose-chars", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "ba") == 0);
    CHECK(rl_point == 2);

    /* Case changes. */
    rl_replace_line("hello world", 0);
    rl_point = 0;
    CHECK(call_command(__LINE__, "upcase-word", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "HELLO world") == 0);
    CHECK(rl_point == 5);

    rl_replace_line("HELLO WORLD", 0);
    rl_point = 0;
    CHECK(call_command(__LINE__, "downcase-word", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "hello WORLD") == 0);

    rl_replace_line("hello wORLD", 0);
    rl_point = 0;
    CHECK(call_command(__LINE__, "capitalize-word", 2, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "Hello World") == 0);
    CHECK(rl_point == 11);

    /* Overwrite mode toggles the flag and overwrites on self-insert. */
    rl_replace_line("abc", 0);
    rl_point = 1;
    CHECK(call_command(__LINE__, "overwrite-mode", 1, 0) == 0);
    CHECK(rl_insert_mode == 0);
    CHECK((rl_readline_state & RL_STATE_OVERWRITE) != 0);
    CHECK(call_command(__LINE__, "self-insert", 1, 'X') == 0);
    CHECK(strcmp(rl_line_buffer, "aXc") == 0);
    CHECK(rl_point == 2);
    CHECK(call_command(__LINE__, "overwrite-mode", 1, 0) == 0);
    CHECK(rl_insert_mode == 1);
    CHECK((rl_readline_state & RL_STATE_OVERWRITE) == 0);

    /* Numeric argument state is recorded and reset by the next command. */
    CHECK(call_command(__LINE__, "digit-argument", 1, '3') == 0);
    CHECK(rl_numeric_arg == 3);
    CHECK(rl_explicit_arg == 1);
    CHECK((rl_readline_state & RL_STATE_NUMERICARG) != 0);
    CHECK(call_command(__LINE__, "beginning-of-line", 1, 0) == 0);
    CHECK(rl_explicit_arg == 0);
    CHECK(rl_numeric_arg == 1);
    CHECK((rl_readline_state & RL_STATE_NUMERICARG) == 0);
    CHECK(call_command(__LINE__, "universal-argument", 1, 0) == 0);
    CHECK(rl_numeric_arg == 4);
    CHECK(rl_explicit_arg == 1);
    CHECK(call_command(__LINE__, "beginning-of-line", 1, 0) == 0);
    CHECK(rl_numeric_arg == 1 && rl_explicit_arg == 0);

    /* quoted-insert consumes a pending input byte. */
    rl_replace_line("", 0);
    rl_pending_input = 'Q';
    CHECK(call_command(__LINE__, "quoted-insert", 1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "Q") == 0);
    CHECK(rl_pending_input == 0);

    /* rl_add_defun resolves and reuses a real command body. */
    forward = rl_named_function("forward-char");
    CHECK(forward != NULL);
    CHECK(rl_add_defun("ruline-forward-char", forward, -1) == 0);
    alias = rl_named_function("ruline-forward-char");
    CHECK(alias == forward);
    rl_replace_line("abc", 0);
    rl_point = 0;
    CHECK(alias(1, 0) == 0);
    CHECK(rl_point == 1);

    /* accept-line sets the done flag and state bit without a terminal loop. */
    CHECK(rl_done == 0);
    CHECK(call_command(__LINE__, "accept-line", 1, 0) == 0);
    CHECK(rl_done == 1);
    CHECK((rl_readline_state & RL_STATE_DONE) != 0);

    /* abort clears a pending input byte and returns. */
    rl_pending_input = 'z';
    CHECK(call_command(__LINE__, "abort", 1, 0) == 0);
    CHECK(rl_pending_input == 0);

    /* The implemented commands are also callable as exported rl_* symbols. */
    rl_replace_line("one two", 0);
    rl_point = 0;
    CHECK(rl_forward_word(1, 0) == 0 && rl_point == 3);
    CHECK(rl_forward(1, 0) == 0 && rl_point == 4);
    CHECK(rl_backward(1, 0) == 0 && rl_point == 3);
    CHECK(rl_beg_of_line(1, 0) == 0 && rl_point == 0);
    CHECK(rl_end_of_line(1, 0) == 0 && rl_point == 7);
    CHECK(rl_backward_byte(1, 0) == 0 && rl_point == 6);
    CHECK(rl_forward_byte(1, 0) == 0 && rl_point == 7);

    rl_replace_line("ac", 0);
    rl_point = 1;
    CHECK(rl_insert(1, 'b') == 0);
    CHECK(strcmp(rl_line_buffer, "abc") == 0 && rl_point == 2);
    CHECK(rl_tab_insert(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "ab\tc") == 0);
    CHECK(rl_delete(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "ab\t") == 0 && rl_point == 3);
    CHECK(rl_rubout(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "ab") == 0 && rl_point == 2);
    CHECK(rl_rubout_or_delete(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "a") == 0 && rl_point == 1);

    rl_deactivate_mark();
    CHECK(rl_mark_active_p() == 0);
    rl_activate_mark();
    CHECK(rl_mark_active_p() == 1);
    rl_keep_mark_active();
    CHECK(rl_mark_active_p() == 1);
    rl_deactivate_mark();
    CHECK(rl_alphabetic('a') == 1);
    CHECK(rl_alphabetic(' ') == 0);
    CHECK(alphabetic('7') == 1);
    CHECK(rl_do_lowercase_version(1, 'A') == 99999);

    rl_explicit_arg = 1;
    rl_numeric_arg = 9;
    CHECK(rl_discard_argument() == 0);
    CHECK(rl_explicit_arg == 0 && rl_numeric_arg == 1);

    {
        char *copy = rl_copy_text(0, 1);
        CHECK(copy != NULL);
        rl_free(copy);
    }

    CHECK(rl_variable_bind("editing-mode", "vi") == 0);
    CHECK(rl_emacs_editing_mode(1, 0) == 0);
    CHECK(rl_insert_mode == 1);
    CHECK(strcmp(rl_get_keymap_name(rl_get_keymap()), "emacs") == 0);

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
    if (test_editing_commands() != 0) {
        return 1;
    }
    if (test_completion() != 0) {
        return 1;
    }
    if (test_history() != 0) {
        return 1;
    }
    if (test_terminal_and_screen_state() != 0) {
        return 1;
    }
    if (test_callback_stream() != 0) {
        return 1;
    }
    if (test_callback_signals() != 0) {
        return 1;
    }
    puts("readline ABI smoke test: OK");
    return 0;
}
