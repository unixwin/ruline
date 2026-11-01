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

static int test_display_and_state(void) {
    FILE *saved_out;
    FILE *capture;
    char buffer[128];
    size_t count;
    struct readline_state saved;
    rl_getc_func_t *saved_getc;
    static const unsigned char csi_sequence[] = "12~";
    static const unsigned char csi_incomplete[] = "1";

    CHECK(rl_initialize() == 0);
    CHECK(rl_gnu_readline_p == 1);
    CHECK(rl_editing_mode == 1);
    CHECK(rl_eof_found == 0);
    CHECK(rl_dispatching == 0);
    CHECK(rl_prefer_env_winsize == 0);
    CHECK(rl_executing_macro == NULL);
    CHECK(rl_undo_list == NULL);
    CHECK(rl_erase_empty_line == 0);
    CHECK(rl_already_prompted == 0);
    CHECK(rl_num_chars_to_read == 0);
    CHECK(rl_instream == stdin);
    CHECK(rl_outstream == stdout);

    CHECK(rl_character_len('a', 0) == 1);
    CHECK(rl_character_len(0x01, 0) == 2);
    CHECK(rl_character_len(0x7f, 0) == 2);
    CHECK(rl_character_len(0x80, 0) == 1);
    CHECK(rl_character_len('\t', 0) == 8);
    CHECK(rl_character_len('\t', 3) == 5);

    CHECK(rl_set_prompt("$ ") == 0);
    capture = tmpfile();
    CHECK(capture != NULL);
    saved_out = rl_outstream;
    rl_outstream = capture;

    CHECK(rl_show_char(0x01) == 3);
    fflush(capture);
    rewind(capture);
    count = fread(buffer, 1, sizeof(buffer), capture);
    CHECK(count == 3);
    CHECK(memcmp(buffer, "C-A", 3) == 0);

    fflush(capture);
    rewind(capture);
    CHECK(rl_message("value=%d/%s", 7, "text") == 0);
    CHECK(rl_display_prompt != NULL);
    CHECK(strcmp(rl_display_prompt, "value=7/text") == 0);
    CHECK(rl_clear_message() == 0);
    CHECK(rl_display_prompt != NULL);
    CHECK(strcmp(rl_display_prompt, "$ ") == 0);

    CHECK(rl_refresh_line(1, 0) == 0);
    CHECK(rl_clear_screen(1, 0) == 0);
    rl_explicit_arg = 1;
    CHECK(rl_clear_screen(1, 0) == 0);
    rl_explicit_arg = 0;
    CHECK(rl_clear_display(1, 0) == 0);
    CHECK(crlf() == 0);

    rl_outstream = saved_out;
    fclose(capture);

    CHECK(rl_set_prompt("one\ntwo") == 0);
    CHECK(rl_on_new_line_with_prompt() == 0);
    CHECK(rl_display_prompt != NULL);
    CHECK(strcmp(rl_display_prompt, "one\ntwo") == 0);
    rl_redraw_prompt_last_line();
    CHECK(strcmp(rl_display_prompt, "one\ntwo") == 0);

    rl_set_screen_size(24, 8);
    rl_replace_line("abcdefghij", 0);
    rl_point = 0;
    CHECK(rl_next_screen_line(1, 0) == 0);
    CHECK(rl_point == 8);
    CHECK(rl_previous_screen_line(1, 0) == 0);
    CHECK(rl_point == 0);

    rl_replace_line("abcd", 0);
    rl_point = 0;
    rl_pending_input = 'C';
    CHECK(rl_arrow_keys(1, 'C') == 0);
    CHECK(rl_point == 1);
    rl_pending_input = 'D';
    CHECK(rl_arrow_keys(1, 'D') == 0);
    CHECK(rl_point == 0);

    saved_getc = rl_getc_function;
    rl_getc_function = stream_getc;
    feed_stream(csi_sequence, sizeof(csi_sequence) - 1);
    CHECK(rl_skip_csi_sequence(1, 0) == 0);
    feed_stream(csi_incomplete, sizeof(csi_incomplete) - 1);
    CHECK(rl_skip_csi_sequence(1, 0) == 1);
    rl_getc_function = saved_getc;

    rl_replace_line("snapshot", 0);
    rl_point = 3;
    rl_mark = 2;
    rl_done = 0;
    CHECK(rl_save_state(&saved) == 0);
    {
        char *original = rl_line_buffer;
        char *replacement = strdup("changed");
        CHECK(replacement != NULL);
        rl_line_buffer = replacement;
        rl_end = 7;
        rl_point = 1;
        rl_mark = 0;
        rl_done = 1;
        CHECK(rl_restore_state(&saved) == 0);
        CHECK(rl_line_buffer == original);
        free(replacement);
    }
    CHECK(strcmp(rl_line_buffer, "snapshot") == 0);
    CHECK(rl_point == 3);
    CHECK(rl_mark == 2);
    CHECK(rl_done == 0);

    CHECK(rl_variable_bind("editing-mode", "vi") == 0);
    CHECK(rl_editing_mode == 0);
    CHECK(rl_emacs_editing_mode(1, 0) == 0);
    CHECK(rl_editing_mode == 1);

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

static int test_history_abi(void) {
    char path[512];
    const char *tmp = getenv("TMPDIR");
    HIST_ENTRY *entry;
    HIST_ENTRY **list;
    HISTORY_STATE *state;

    if (tmp == NULL || *tmp == '\0') {
        tmp = "/tmp";
    }

    /* Earlier probes may have added history; clear it before asserting the
     * empty-list contract. */
    clear_history();
    CHECK(history_list() == NULL);

    add_history("one");
    add_history("two");
    add_history("three");
    CHECK(history_length == 3);

    list = history_list();
    CHECK(list != NULL);
    CHECK(list[0] != NULL && strcmp(list[0]->line, "one") == 0);
    CHECK(list[1] != NULL && strcmp(list[1]->line, "two") == 0);
    CHECK(list[2] != NULL && strcmp(list[2]->line, "three") == 0);
    CHECK(list[3] == NULL);

    using_history();
    state = history_get_history_state();
    CHECK(state != NULL);
    CHECK(state->entries == list);
    CHECK(state->offset == 3);
    CHECK(state->length == 3);
    CHECK(state->size >= state->length);
    CHECK(state->flags == 0);
    state->offset = 1;
    history_set_history_state(state);
    CHECK(where_history() == 1);
    free(state);
    stifle_history(4);
    state = history_get_history_state();
    CHECK(state != NULL && (state->flags & HS_STIFLED) != 0);
    free(state);
    unstifle_history();

    entry = remove_history(1);
    CHECK(entry != NULL && strcmp(entry->line, "two") == 0);
    CHECK(history_length == 2);
    CHECK(strcmp(history_list()[1]->line, "three") == 0);
    CHECK(free_history_entry(entry) == NULL);
    CHECK(remove_history(9) == NULL);

    entry = replace_history_entry(1, "THREE", (histdata_t)0x1234);
    CHECK(entry != NULL && strcmp(entry->line, "three") == 0);
    CHECK(strcmp(history_list()[1]->line, "THREE") == 0);
    CHECK(history_list()[1]->data == (histdata_t)0x1234);
    free_history_entry(entry);
    CHECK(replace_history_entry(-1, "x", NULL) == NULL);

    entry = alloc_history_entry("allocated", strdup("#123"));
    CHECK(entry != NULL);
    CHECK(strcmp(entry->line, "allocated") == 0);
    {
        HIST_ENTRY *copy = copy_history_entry(entry);
        CHECK(copy != NULL);
        CHECK(copy->line != entry->line);
        CHECK(strcmp(copy->timestamp, "#123") == 0);
        free_history_entry(copy);
    }
    history_comment_char = '#';
    CHECK(history_get_time(entry) == (time_t)123);
    history_comment_char = 0;
    free_history_entry(entry);

    {
        HIST_ENTRY **range = remove_history_range(0, 0);
        CHECK(range != NULL);
        CHECK(strcmp(range[0]->line, "one") == 0);
        CHECK(range[1] == NULL);
        CHECK(history_length == 1);
        free_history_entry(range[0]);
        free(range);
    }
    {
        HIST_ENTRY **range = remove_history_range(0, 0);
        CHECK(range != NULL);
        CHECK(strcmp(range[0]->line, "THREE") == 0);
        CHECK(range[1] == NULL);
        CHECK(history_length == 0);
        free_history_entry(range[0]);
        free(range);
    }
    CHECK(remove_history_range(0, 0) == NULL);

    clear_history();
    add_history("alpha one");
    add_history("beta two");
    add_history("alphabet");
    add_history("gamma alpha");
    using_history();

    CHECK(history_search("alpha", -1) == 6);
    CHECK(where_history() == 3);
    CHECK(history_set_pos(2) == 1);
    CHECK(history_search("alpha", -1) == 0);
    CHECK(where_history() == 2);
    CHECK(history_set_pos(0) == 1);
    CHECK(history_search("alpha", 1) == 0);
    CHECK(where_history() == 0);
    CHECK(history_set_pos(1) == 1);
    CHECK(history_search("alpha", 1) == 0);
    CHECK(where_history() == 2);
    CHECK(history_set_pos(4) == 1);
    CHECK(history_search_prefix("alpha", -1) == 0);
    CHECK(where_history() == 2);
    CHECK(history_set_pos(0) == 1);
    CHECK(history_search_prefix("alpha", 1) == 0);
    CHECK(history_search_prefix("zzz", 1) == -1);
    CHECK(history_search_pos("alpha", -1, 2) == 2);
    CHECK(where_history() == 0);
    CHECK(history_search_pos("zzz", -1, 2) == -1);
    CHECK(where_history() == 0);

    clear_history();
    add_history("first");
    add_history("second arg");
    add_history("third thing here");
    using_history();
    {
        char *output = NULL;
        CHECK(history_expand("echo !!", &output) == 1);
        CHECK(output != NULL && strcmp(output, "echo third thing here") == 0);
        free(output);
        output = NULL;
        CHECK(history_expand("echo !2", &output) == 1);
        CHECK(output != NULL && strcmp(output, "echo second arg") == 0);
        free(output);
        output = NULL;
        CHECK(history_expand("echo !nosuch", &output) == -1);
        CHECK(output != NULL && strcmp(output, "!nosuch: event not found") == 0);
        free(output);
        output = NULL;
        CHECK(history_expand("echo !!:$", &output) == 1);
        CHECK(output != NULL && strcmp(output, "echo here") == 0);
        free(output);
        output = NULL;
        CHECK(history_expand("echo !!:q", &output) == 1);
        CHECK(output != NULL && strcmp(output, "echo 'third thing here'") == 0);
        free(output);
        output = NULL;
        CHECK(history_expand("echo !!:p", &output) == 2);
        CHECK(output != NULL && strcmp(output, "echo third thing here") == 0);
        free(output);
        output = NULL;
        CHECK(history_expand("echo !!:s/third/THIRD/", &output) == 1);
        CHECK(output != NULL && strcmp(output, "echo THIRD thing here") == 0);
        free(output);
        output = NULL;
        CHECK(history_expand("echo \\!!", &output) == 0);
        CHECK(output != NULL && strcmp(output, "echo \\!!") == 0);
        free(output);
    }

    {
        char **words = history_tokenize("echo 'a b' \"c d\"");
        CHECK(words != NULL);
        CHECK(words[0] != NULL && strcmp(words[0], "echo") == 0);
        CHECK(words[1] != NULL && strcmp(words[1], "'a b'") == 0);
        CHECK(words[2] != NULL && strcmp(words[2], "\"c d\"") == 0);
        CHECK(words[3] == NULL);
        for (int i = 0; words[i] != NULL; i++) {
            free(words[i]);
        }
        free(words);
        words = history_tokenize("foo|bar;baz");
        CHECK(words != NULL);
        CHECK(strcmp(words[1], "|") == 0);
        CHECK(strcmp(words[3], ";") == 0);
        for (int i = 0; words[i] != NULL; i++) {
            free(words[i]);
        }
        free(words);
        CHECK(history_tokenize("") == NULL);
    }

    {
        char *arguments = history_arg_extract(1, 2, "cmd one two three");
        CHECK(arguments != NULL && strcmp(arguments, "one two") == 0);
        free(arguments);
        arguments = history_arg_extract((int)'$', (int)'$', "cmd one two three");
        CHECK(arguments != NULL && strcmp(arguments, "three") == 0);
        free(arguments);
        CHECK(history_arg_extract(9, 9, "cmd one") == NULL);
    }

    snprintf(path, sizeof(path), "%s/ruline-history-abi-%ld", tmp, (long)getpid());
    {
        FILE *file = fopen(path, "w");
        CHECK(file != NULL);
        fputs("one\ntwo\nthree\nfour\n", file);
        fclose(file);
    }
    CHECK(history_truncate_file(path, 2) == 0);
    {
        FILE *file = fopen(path, "r");
        char line[128];
        CHECK(file != NULL);
        CHECK(fgets(line, sizeof(line), file) != NULL && strcmp(line, "three\n") == 0);
        CHECK(fgets(line, sizeof(line), file) != NULL && strcmp(line, "four\n") == 0);
        CHECK(fgets(line, sizeof(line), file) == NULL);
        fclose(file);
    }
    unlink(path);

    clear_history();
    return 0;
}

static void queue_byte(int key) { rl_pending_input = key; }

static void feed_text(int (*command)(int, int), const char *text) {
    for (; *text != '\0'; text++) {
        rl_pending_input = (unsigned char)*text;
        command(1, 0);
    }
}

static int test_history_nav(void) {
    HIST_ENTRY *entry;

    clear_history();
    add_history("one");
    add_history("two");
    add_history("three");
    using_history();

    /* No non-incremental search string has been recorded yet. */
    CHECK(rl_noninc_reverse_search_again(1, 0) == 1);

    rl_replace_line("draft", 0);
    rl_point = rl_end;
    CHECK(rl_get_previous_history(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "three") == 0);
    CHECK(rl_point == rl_end);
    CHECK(rl_mark == 0);

    rl_replace_line("THREE", 0);
    rl_point = rl_end;
    CHECK(rl_get_next_history(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "draft") == 0);
    CHECK(rl_point == rl_end);
    entry = history_get(3);
    CHECK(entry != NULL && strcmp(entry->line, "THREE") == 0);

    CHECK(rl_get_previous_history(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "THREE") == 0);
    CHECK(rl_beginning_of_history(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "one") == 0);
    CHECK(rl_end_of_history(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "draft") == 0);

    rl_replace_line("draft", 0);
    rl_point = rl_end;
    rl_explicit_arg = 1;
    CHECK(rl_fetch_history(2, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "two") == 0);
    CHECK(rl_end_of_history(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "draft") == 0);
    CHECK(rl_fetch_history(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "one") == 0);
    CHECK(rl_end_of_history(1, 0) == 0);

    clear_history();
    add_history("alpha one");
    add_history("beta two");
    add_history("alphabet");
    add_history("gamma alpha");
    using_history();

    rl_replace_line("al", 0);
    rl_point = rl_end;
    CHECK(rl_history_search_backward(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "alphabet") == 0);
    CHECK(rl_point == 2);
    CHECK(rl_history_search_backward(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "alpha one") == 0);
    CHECK(rl_history_search_forward(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "alphabet") == 0);

    using_history();
    rl_replace_line("alpha", 0);
    rl_point = rl_end;
    CHECK(rl_history_substr_search_backward(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "gamma alpha") == 0);
    CHECK(rl_point == 6);
    CHECK(rl_mark == rl_end);

    clear_history();
    add_history("alpha one");
    add_history("beta two");
    add_history("alphabet");
    add_history("gamma alpha");
    using_history();

    rl_replace_line("draft", 0);
    rl_point = rl_end;
    feed_text(rl_noninc_reverse_search, "alphabet");
    queue_byte('\n');
    CHECK(rl_noninc_reverse_search(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "alphabet") == 0);
    CHECK(rl_point == 0);
    CHECK(rl_mark == rl_end);
    CHECK(rl_noninc_reverse_search_again(1, 0) == 1);
    CHECK(strcmp(rl_line_buffer, "alphabet") == 0);

    using_history();
    history_set_pos(0);
    rl_replace_line("draft", 0);
    rl_point = rl_end;
    feed_text(rl_noninc_forward_search, "gamma");
    queue_byte('\n');
    CHECK(rl_noninc_forward_search(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "gamma alpha") == 0);

    using_history();
    rl_replace_line("draft", 0);
    rl_point = rl_end;
    CHECK(rl_reverse_search_history(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "") == 0);
    feed_text(rl_reverse_search_history, "alph");
    queue_byte('\n');
    CHECK(rl_reverse_search_history(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "gamma alpha") == 0);
    CHECK(rl_point == 6);

    using_history();
    rl_replace_line("draft", 0);
    rl_point = rl_end;
    CHECK(rl_reverse_search_history(1, 0) == 0);
    queue_byte(7); /* C-g */
    CHECK(rl_reverse_search_history(1, 0) == 1);
    CHECK(strcmp(rl_line_buffer, "draft") == 0);

    clear_history();
    add_history("one");
    add_history("two");
    add_history("three");
    using_history();
    rl_replace_line("three", 0);
    rl_point = rl_end;
    rl_done = 0;
    CHECK(rl_operate_and_get_next(1, 0) == 0);
    CHECK(rl_done == 1);

    clear_history();
    {
        int base = history_base;
        rl_clear_history();
        CHECK(history_length == 0);
        CHECK(history_offset == 0);
        CHECK(history_base == base);
    }

    clear_history();
    rl_done = 0;
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

static void noop_redisplay(void) {}

static int test_undo_macro_and_remaining_text(void) {
    rl_voidfunc_t *saved_redisplay;
    char *macro;
    HIST_ENTRY **list;

    CHECK(rl_initialize() == 0);

    /* Undo list layout and basic insert/delete replay. */
    rl_free_undo_list();
    rl_replace_line("abcdef", 0);
    rl_point = 2;
    CHECK(rl_insert_text("XY") == 2);
    CHECK(strcmp(rl_line_buffer, "abXYcdef") == 0);
    CHECK(rl_undo_list != NULL);
    CHECK(rl_undo_list->what == UNDO_INSERT);
    CHECK(rl_do_undo() == 1);
    CHECK(strcmp(rl_line_buffer, "abcdef") == 0);
    CHECK(rl_point == 2);
    CHECK(rl_do_undo() == 0);

    rl_point = 3;
    CHECK(rl_delete_text(1, 3) == 2);
    CHECK(strcmp(rl_line_buffer, "adef") == 0);
    CHECK(rl_do_undo() == 1);
    CHECK(strcmp(rl_line_buffer, "abcdef") == 0);
    CHECK(rl_point == 3);

    /* Groups are undone atomically. */
    rl_free_undo_list();
    rl_replace_line("abc", 0);
    rl_point = 3;
    CHECK(rl_begin_undo_group() == 0);
    CHECK(rl_insert_text("1") == 1);
    CHECK(rl_insert_text("2") == 1);
    CHECK(rl_end_undo_group() == 0);
    CHECK(strcmp(rl_line_buffer, "abc12") == 0);
    CHECK(rl_do_undo() == 1);
    CHECK(strcmp(rl_line_buffer, "abc") == 0);

    /* rl_modifying records a reversible replace pair. */
    rl_free_undo_list();
    rl_replace_line("abc", 0);
    CHECK(rl_modifying(0, 2) == 0);
    CHECK(rl_do_undo() == 1);
    CHECK(strcmp(rl_line_buffer, "abc") == 0);
    CHECK(rl_point == 2);

    /* revert-line and undo-command. */
    rl_free_undo_list();
    rl_replace_line("abc", 0);
    rl_point = 3;
    CHECK(rl_insert_text("d") == 1);
    CHECK(rl_revert_line(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "abc") == 0);
    CHECK(rl_undo_command(1, 0) == 0);

    /* Saved line helpers and their compat.c spellings. */
    rl_free_undo_list();
    rl_replace_line("original", 0);
    CHECK(rl_maybe_save_line() == 0);
    rl_replace_line("changed", 0);
    CHECK(rl_maybe_unsave_line() == 0);
    CHECK(strcmp(rl_line_buffer, "original") == 0);
    CHECK(rl_point == 8);
    CHECK(rl_maybe_unsave_line() == 0);
    CHECK(maybe_save_line() == 0);
    CHECK(maybe_unsave_line() == 0);
    CHECK(maybe_replace_line() == 0);

    /* The current history entry tracks the undo list. */
    clear_history();
    add_history("hist");
    using_history();
    CHECK(history_set_pos(0) == 1);
    rl_replace_line("edited", 0);
    rl_add_undo(UNDO_BEGIN, 0, 0, NULL);
    CHECK(rl_undo_list != NULL);
    CHECK(rl_maybe_replace_line() == 0);
    list = history_list();
    CHECK(list != NULL && list[0] != NULL);
    CHECK(strcmp(list[0]->line, "edited") == 0);
    free_undo_list();
    CHECK(rl_undo_list == NULL);
    clear_history();

    /* Character search reads its target from rl_pending_input. */
    rl_free_undo_list();
    rl_replace_line("a-b-c-d", 0);
    rl_point = 0;
    rl_pending_input = '-';
    CHECK(rl_char_search(2, 0) == 0);
    CHECK(rl_point == 3);
    rl_pending_input = '-';
    CHECK(rl_backward_char_search(1, 0) == 0);
    CHECK(rl_point == 1);
    rl_point = 6;
    rl_pending_input = '-';
    CHECK(rl_char_search(-1, 0) == 0);
    CHECK(rl_point == 5);
    rl_point = 0;
    rl_pending_input = 'z';
    CHECK(rl_char_search(1, 0) == 1);
    CHECK(rl_point == 0);

    /* Keyboard macro state; capture needs the dispatcher, but the state
     * machine and replay seam are real. */
    CHECK(rl_start_kbd_macro(1, 0) == 0);
    CHECK((rl_readline_state & RL_STATE_MACRODEF) != 0);
    CHECK(rl_start_kbd_macro(1, 0) == 1);
    CHECK((rl_readline_state & RL_STATE_MACRODEF) == 0);
    CHECK(rl_print_last_kbd_macro(1, 0) == 0);
    CHECK(rl_call_last_kbd_macro(1, 0) == 1);
    /* Ending without a captured macro aborts (our standalone equivalent of
     * GNU's longjmp out of the dispatcher). */
    CHECK(rl_end_kbd_macro(1, 0) == 1);

    macro = strdup("abc");
    CHECK(macro != NULL);
    rl_push_macro_input(macro);
    CHECK(rl_executing_macro != NULL);
    CHECK(strcmp(rl_executing_macro, "abc") == 0);
    CHECK((rl_readline_state & RL_STATE_MACROINPUT) != 0);

    /* execute-named-command consumes macro input. */
    rl_replace_line("one two", 0);
    rl_point = 0;
    macro = strdup("forward-word\n");
    CHECK(macro != NULL);
    rl_push_macro_input(macro);
    CHECK(rl_execute_named_command(1, 0) == 0);
    CHECK(rl_point == 3);
    macro = strdup("no-such-command\n");
    CHECK(macro != NULL);
    rl_push_macro_input(macro);
    CHECK(rl_execute_named_command(1, 0) == 1);
    CHECK(rl_execute_named_command(1, 0) == 1);

    /* Bracketed paste inserts whatever input source is available. */
    rl_free_line_state();
    rl_replace_line("", 0);
    rl_point = 0;
    rl_pending_input = 'x';
    CHECK(rl_bracketed_paste_begin(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "x") == 0);
    CHECK(rl_paste_from_clipboard(1, 0) == 0);

    /* Comment insertion and close-paren insertion. */
    saved_redisplay = rl_redisplay_function;
    rl_redisplay_function = noop_redisplay;
    rl_free_undo_list();
    rl_replace_line("echo hi", 0);
    rl_point = 0;
    rl_explicit_arg = 0;
    CHECK(rl_insert_comment(1, '#') == 0);
    CHECK(strcmp(rl_line_buffer, "#echo hi") == 0);
    CHECK(rl_point == 1);
    rl_done = 0;
    rl_readline_state &= ~RL_STATE_DONE;
    rl_replace_line("#echo hi", 0);
    rl_point = 0;
    rl_explicit_arg = 1;
    CHECK(rl_insert_comment(1, '#') == 0);
    CHECK(strcmp(rl_line_buffer, "echo hi") == 0);
    rl_done = 0;
    rl_readline_state &= ~RL_STATE_DONE;
    rl_explicit_arg = 0;
    rl_replace_line("", 0);
    rl_point = 0;
    CHECK(rl_insert_close(1, ')') == 0);
    CHECK(strcmp(rl_line_buffer, ")") == 0);
    rl_redisplay_function = saved_redisplay;

    /* Tilde expansion and terminal helpers. */
    CHECK(setenv("HOME", "/tmp", 1) == 0);
    rl_free_undo_list();
    rl_replace_line("echo ~", 0);
    rl_point = 6;
    CHECK(rl_tilde_expand(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "echo /tmp") == 0);
    rl_replace_line("echo ~/src", 0);
    rl_point = 10;
    CHECK(rl_tilde_expand(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "echo /tmp/src") == 0);
    CHECK(rl_restart_output(1, 0) == 0);
    CHECK(rl_stop_output(1, 0) == 0);
    CHECK(rl_tty_status(1, 0) == 0);

    /* History-argument yanks. */
    clear_history();
    add_history("cmd four five six");
    add_history("cmd one two three");
    using_history();
    rl_free_undo_list();
    rl_replace_line("", 0);
    rl_point = 0;
    CHECK(rl_yank_nth_arg(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "one") == 0);
    CHECK(rl_point == 3 && rl_mark == 0);
    rl_replace_line("", 0);
    rl_point = 0;
    CHECK(rl_yank_nth_arg('$', 0) == 0);
    CHECK(strcmp(rl_line_buffer, "three") == 0);
    rl_replace_line("", 0);
    rl_point = 0;
    rl_free_undo_list();
    CHECK(rl_yank_last_arg(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "three") == 0);
    CHECK(rl_yank_last_arg(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "six") == 0);
    rl_replace_line("", 0);
    rl_point = 0;
    CHECK(rl_yank_nth_arg(9, 0) == 1);
    clear_history();
    rl_free_undo_list();

    return 0;
}

static void set_pending_key(int key) {
    rl_pending_input = key;
}

static int test_vi_commands(void) {
    int ignored = 0;

    rl_initialize();
    CHECK(rl_vi_editing_mode(1, 0) == 0);
    CHECK(strcmp(rl_get_keymap_name(rl_get_keymap()), "vi-insert") == 0);
    CHECK(rl_vi_check() == 0);
    rl_vi_start_inserting('i', 1, 1);
    CHECK(strcmp(rl_get_keymap_name(rl_get_keymap()), "vi-insert") == 0);
    CHECK(rl_vi_insertion_mode(1, 'i') == 0);
    CHECK(rl_vi_insert_mode(1, 'i') == 0);
    CHECK(rl_vi_movement_mode(1, 0) == 0);
    CHECK(strcmp(rl_get_keymap_name(rl_get_keymap()), "vi") == 0);

    /* Word motion. */
    rl_replace_line("one two three", 0);
    rl_point = 0;
    CHECK(rl_vi_fword(1, 'w') == 0);
    CHECK(rl_point == 4);
    CHECK(rl_vi_bword(1, 'b') == 0);
    CHECK(rl_point == 0);
    CHECK(rl_vi_eword(1, 'e') == 0);
    CHECK(rl_point == 2);
    CHECK(rl_vi_next_word(1, 'w') == 0);
    CHECK(rl_point == 4);
    CHECK(rl_vi_end_word(1, 'e') == 0);
    CHECK(rl_point == 6);
    CHECK(rl_vi_prev_word(1, 'b') == 0);
    CHECK(rl_point == 4);
    rl_replace_line("a.b c", 0);
    rl_point = 0;
    CHECK(rl_vi_fWord(1, 'W') == 0);
    CHECK(rl_point == 4);
    CHECK(rl_vi_bWord(1, 'B') == 0);
    CHECK(rl_point == 0);
    CHECK(rl_vi_eWord(1, 'E') == 0);
    CHECK(rl_point == 2);

    rl_replace_line("   abc", 0);
    rl_point = 0;
    CHECK(rl_vi_first_print(1, '^') == 0);
    CHECK(rl_point == 3);
    CHECK(rl_vi_back_to_indent(1, '^') == 0);
    CHECK(rl_point == 3);
    rl_replace_line("one two", 0);
    rl_point = 0;
    CHECK(rl_vi_column(3, '|') == 0);
    CHECK(rl_point == 2);

    /* Operators and undo. */
    rl_replace_line("one two three", 0);
    rl_point = 0;
    rl_mark = 0;
    set_pending_key('w');
    CHECK(rl_vi_delete_to(1, 'd') == 0);
    CHECK(strcmp(rl_line_buffer, "two three") == 0);
    CHECK(rl_vi_undo(1, 'u') == 0);
    CHECK(strcmp(rl_line_buffer, "one two three") == 0);

    rl_replace_line("abcdef", 0);
    rl_point = 2;
    CHECK(rl_vi_delete(1, 'x') == 0);
    CHECK(strcmp(rl_line_buffer, "abdef") == 0);
    CHECK(rl_vi_undo(1, 'u') == 0);
    CHECK(strcmp(rl_line_buffer, "abcdef") == 0);
    CHECK(rl_point == 3);

    rl_replace_line("abcdef", 0);
    rl_point = 0;
    CHECK(rl_vi_redo(1, '.') == 0);
    CHECK(strcmp(rl_line_buffer, "bcdef") == 0);

    rl_replace_line("one two", 0);
    rl_point = 0;
    set_pending_key('w');
    CHECK(rl_vi_change_to(1, 'c') == 0);
    CHECK(strcmp(rl_line_buffer, " two") == 0);
    CHECK(rl_insert(1, 'O') == 0);
    CHECK(rl_insert(1, 'N') == 0);
    CHECK(rl_insert(1, 'E') == 0);
    CHECK(rl_vi_movement_mode(1, 0) == 0);
    CHECK(strcmp(rl_line_buffer, "ONE two") == 0);
    CHECK(rl_vi_undo(1, 'u') == 0);
    CHECK(strcmp(rl_line_buffer, "one two") == 0);

    rl_replace_line("abc", 0);
    rl_point = 2;
    CHECK(rl_vi_rubout(1, 8) == 0);
    CHECK(strcmp(rl_line_buffer, "ac") == 0);
    rl_replace_line("foo bar", 0);
    rl_point = 7;
    CHECK(rl_vi_unix_word_rubout(1, 23) == 0);
    CHECK(strcmp(rl_line_buffer, "foo ") == 0);

    /* Put / yank / kill ring. */
    rl_vi_movement_mode(1, 0);
    rl_replace_line("abc", 0);
    rl_point = 1;
    rl_kill_text(1, 2);
    rl_vi_movement_mode(1, 0);
    rl_replace_line("zz", 0);
    rl_point = 1;
    rl_kill_text(1, 2);
    rl_vi_movement_mode(1, 0);
    rl_replace_line("q", 0);
    rl_point = 1;
    CHECK(rl_vi_put(1, 'p') == 0);
    CHECK(strcmp(rl_line_buffer, "qz") == 0);
    CHECK(rl_vi_yank_pop(1, 'p') == 0);
    CHECK(strcmp(rl_line_buffer, "qb") == 0);

    rl_replace_line("abcdef", 0);
    rl_point = 0;
    set_pending_key('e');
    CHECK(rl_vi_yank_to(1, 'y') == 0);
    CHECK(strcmp(rl_line_buffer, "abcdef") == 0);

    /* Character search and marks. */
    rl_replace_line("a,b,c", 0);
    rl_point = 0;
    set_pending_key(',');
    CHECK(rl_vi_char_search(1, 'f') == 0);
    CHECK(rl_point == 1);
    CHECK(rl_vi_char_search(1, ';') == 0);
    CHECK(rl_point == 3);
    CHECK(rl_vi_char_search(1, ',') == 0);
    CHECK(rl_point == 1);
    set_pending_key('a');
    CHECK(rl_vi_set_mark(1, 'm') == 0);
    rl_point = 3;
    set_pending_key('a');
    CHECK(rl_vi_goto_mark(1, '`') == 0);
    CHECK(rl_point == 1);

    rl_replace_line("(abc)", 0);
    rl_point = 0;
    CHECK(rl_vi_bracktype('(') == 1);
    CHECK(rl_vi_bracktype(']') == -2);
    CHECK(rl_vi_match(1, '%') == 0);
    CHECK(rl_point == 4);

    /* Character and case edits. */
    rl_replace_line("abcdef", 0);
    rl_point = 1;
    set_pending_key('X');
    CHECK(rl_vi_change_char(2, 'r') == 0);
    CHECK(strcmp(rl_line_buffer, "aXXdef") == 0);
    rl_replace_line("aBc def", 0);
    rl_point = 0;
    CHECK(rl_vi_change_case(2, '~') == 0);
    CHECK(strcmp(rl_line_buffer, "Abc def") == 0);

    rl_replace_line("one two", 0);
    rl_point = 0;
    CHECK(rl_vi_subst(1, 's') == 0);
    CHECK(strcmp(rl_line_buffer, "ne two") == 0);
    rl_vi_movement_mode(1, 0);
    rl_replace_line("one two", 0);
    rl_point = 4;
    CHECK(rl_vi_subst(1, 'S') == 0);
    CHECK(strcmp(rl_line_buffer, "") == 0);
    rl_vi_movement_mode(1, 0);

    /* Insert-entry commands. */
    rl_replace_line("ab", 0);
    rl_point = 0;
    CHECK(rl_vi_append_mode(1, 'a') == 0);
    CHECK(rl_point == 1);
    rl_vi_movement_mode(1, 0);
    rl_replace_line("ab", 0);
    rl_point = 0;
    CHECK(rl_vi_append_eol(1, 'A') == 0);
    CHECK(rl_point == 2);
    rl_vi_movement_mode(1, 0);
    rl_replace_line("ab", 0);
    rl_point = 1;
    CHECK(rl_vi_insert_beg(1, 'I') == 0);
    CHECK(rl_point == 0);
    rl_vi_movement_mode(1, 0);

    /* Overstrike and replace mode. */
    rl_replace_line("abcde", 0);
    rl_point = 1;
    CHECK(rl_vi_overstrike(1, 'X') == 0);
    CHECK(rl_vi_overstrike(1, 'Y') == 0);
    CHECK(strcmp(rl_line_buffer, "aXYde") == 0);
    CHECK(rl_vi_overstrike_delete(1, 127) == 0);
    CHECK(strcmp(rl_line_buffer, "aXcde") == 0);
    rl_vi_movement_mode(1, 0);

    rl_replace_line("abc", 0);
    rl_point = 1;
    CHECK(rl_vi_replace(1, 'R') == 0);
    CHECK(rl_insert_mode == 1);
    CHECK(rl_get_keymap_name(rl_get_keymap()) == NULL);
    CHECK(rl_insert(1, 'X') == 0);
    CHECK(strcmp(rl_line_buffer, "aXc") == 0);
    rl_vi_movement_mode(1, 0);

    /* History commands. */
    clear_history();
    add_history("cmd one two");
    add_history("echo alpha beta");
    using_history();
    rl_replace_line("", 0);
    rl_explicit_arg = 1;
    rl_numeric_arg = 1;
    CHECK(rl_vi_fetch_history(1, 'G') == 0);
    CHECK(strcmp(rl_line_buffer, "cmd one two") == 0);
    CHECK(rl_point == 0);
    CHECK(rl_mark == rl_end);
    rl_explicit_arg = 0;
    rl_numeric_arg = 1;
    /* fetch-history moved the history position to the old entry; reset to
     * the newest before testing the last-argument yank. */
    using_history();

    rl_replace_line("x", 0);
    rl_point = 0;
    CHECK(rl_vi_yank_arg(1, ':') == 0);
    CHECK(strcmp(rl_line_buffer, "x beta") == 0);
    rl_vi_movement_mode(1, 0);

    rl_pending_input = 0;
    CHECK(rl_vi_search(1, '/') == 0);
    CHECK(rl_vi_search_again(1, 'n') == 1);
    CHECK(rl_vi_domove(0, &ignored) == -1);
    CHECK(ignored == -1);

    /* Numeric argument and EOF. */
    rl_vi_arg_digit(1, '3');
    CHECK(rl_numeric_arg == 3);
    rl_discard_argument();
    CHECK(rl_vi_tilde_expand(1, '&') == 0);
    rl_vi_movement_mode(1, 0);
    rl_done = 0;
    CHECK(rl_vi_eof_maybe(1, 4) == 0);
    CHECK(rl_done == 1);
    rl_done = 0;
    /* Leave the global keymap in the emacs default for the probes that
     * follow (callback/terminal assume the standard map). */
    rl_emacs_editing_mode(1, 0);

    return 0;
}

int main(void) {
    if (test_state_and_text() != 0) {
        return 1;
    }
    if (test_display_and_state() != 0) {
        return 1;
    }
    if (test_keymap_and_binding() != 0) {
        return 1;
    }
    if (test_editing_commands() != 0) {
        return 1;
    }
    if (test_vi_commands() != 0) {
        return 1;
    }
    if (test_completion() != 0) {
        return 1;
    }
    if (test_history_abi() != 0) {
        return 1;
    }
    if (test_history() != 0) {
        return 1;
    }
    if (test_history_nav() != 0) {
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
    if (test_undo_macro_and_remaining_text() != 0) {
        return 1;
    }
    puts("readline ABI smoke test: OK");
    return 0;
}
