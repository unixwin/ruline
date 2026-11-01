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

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
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
    if (test_history() != 0) {
        return 1;
    }
    puts("readline ABI smoke test: OK");
    return 0;
}
