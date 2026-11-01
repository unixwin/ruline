/*
 * GNU readline differential conformance probe.
 *
 * Compiled twice by run.sh: once against the system GNU readline 8.3 and once
 * against ruline's readline-abi. Every line is deterministic and names its
 * section; run.sh diffs the two outputs. A difference is a compatibility bug.
 */
#include <stdio.h>

#include <readline/history.h>
#include <readline/readline.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/* GNU hides this legacy alias behind `#if 0` in readline.h but compat.c still
 * exports it.  Declare it locally so the same source builds against both
 * libraries. */
extern int alphabetic(int);

/* GNU's compat.c exports these legacy spellings but its header hides the
 * declarations behind `#if 0`.  Declare them locally so one source builds
 * against both libraries. */
extern void free_undo_list(void);
extern int maybe_save_line(void);
extern int maybe_unsave_line(void);
extern int maybe_replace_line(void);

static void section(const char *name) {
    printf("== %s ==\n", name);
}

static void value(const char *key, const char *text) {
    printf("%s|%s\n", key, text == NULL ? "<null>" : text);
}

static void integer(const char *key, int number) {
    printf("%s|%d\n", key, number);
}

static int cmp_str(const void *left, const void *right) {
    const char *const *a = left;
    const char *const *b = right;
    return strcmp(*a, *b);
}

static void print_sorted(char **items, int count, const char *key) {
    if (items == NULL || count == 0) {
        printf("%s|<empty>\n", key);
        return;
    }
    qsort(items, (size_t)count, sizeof(*items), cmp_str);
    for (int i = 0; i < count; i++) {
        printf("%s|%s\n", key, items[i]);
    }
}

static int free_sorted(char **items) {
    int count = 0;
    if (items != NULL) {
        for (; items[count] != NULL; count++) {
            free(items[count]);
        }
        free(items);
    }
    return count;
}

static void print_keyseq(const char *key, const char *spelling) {
    char buffer[128];
    int length = -1;
    int rc = rl_translate_keyseq(spelling, buffer, &length);
    printf("%s|rc=%d|len=%d|", key, rc, length);
    for (int i = 0; i < length && i < (int)sizeof(buffer); i++) {
        printf("%02X", (unsigned char)buffer[i]);
    }
    putchar('\n');
}

/* GNU rl_function_of_keyseq takes RAW key bytes, not the bind spelling.
 * Translate first so the oracle and ruline are asked the same question. */
static rl_command_func_t *function_of_spelling(const char *spelling) {
    char raw[128];
    int length = -1;
    if (rl_translate_keyseq(spelling, raw, &length) != 0 || length < 0) {
        return NULL;
    }
    raw[length] = '\0';
    return rl_function_of_keyseq(raw, NULL, NULL);
}

static int test_command(int count, int key) {
    (void)count;
    (void)key;
    return 0;
}

/* Keep redisplay out of the captured output for commands that repaint. */
static void noop_redisplay(void) {}

static char *generator(const char *text, int state) {
    static int index;
    static const char *const values[] = {"alpha", "alpine", "altitude", NULL};
    if (state == 0) {
        index = 0;
    }
    const char *candidate = values[index++];
    if (candidate == NULL) {
        return NULL;
    }
    if (strncmp(candidate, text, strlen(text)) != 0) {
        return generator(text, state);
    }
    return strdup(candidate);
}

static void write_file(const char *path, const char *text) {
    FILE *file = fopen(path, "w");
    if (file != NULL) {
        fputs(text, file);
        fclose(file);
    }
}

static char *root_dir;

static const char *make_fixture_dir(void) {
    char path[512];
    snprintf(path, sizeof(path), "%s/conformance", root_dir);
    mkdir(path, 0700);
    return strdup(path);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    root_dir = getenv("TMPDIR");
    if (root_dir == NULL || *root_dir == '\0') {
        root_dir = "/tmp";
    }

    section("version");
    integer("rl_readline_version", rl_readline_version);
    value("rl_library_version", rl_library_version);

    section("state");
    rl_initialize();
    integer("point.initial", rl_point);
    integer("end.initial", rl_end);
    integer("mark.initial", rl_mark);
    value("line.initial", rl_line_buffer);

    section("text");
    rl_set_prompt("> ");
    rl_replace_line("hello world", 0);
    value("replace.line", rl_line_buffer);
    integer("replace.point", rl_point);
    integer("replace.end", rl_end);
    rl_point = 5;
    rl_insert_text(" there");
    value("insert.line", rl_line_buffer);
    integer("insert.point", rl_point);
    integer("insert.end", rl_end);
    rl_delete_text(5, 11);
    value("delete.line", rl_line_buffer);
    integer("delete.point", rl_point);
    integer("delete.end", rl_end);
    integer("delete.mark", rl_mark);
    {
        char *copy = rl_copy_text(0, 5);
        value("copy.text", copy);
        free(copy);
    }
    rl_replace_line("abcdef", 0);
    rl_point = 2;
    rl_mark = 4;
    rl_kill_text(2, 4);
    value("kill.line", rl_line_buffer);
    integer("kill.point", rl_point);
    integer("kill.end", rl_end);
    integer("kill.mark", rl_mark);

    section("editing");
    rl_replace_line("one two three", 0);
    rl_point = 0;
    integer("editing.forward_word.rc", rl_forward_word(1, 0));
    integer("editing.forward_word.point", rl_point);
    integer("editing.forward_word2.rc", rl_forward_word(1, 0));
    integer("editing.forward_word2.point", rl_point);
    integer("editing.backward_word.rc", rl_backward_word(1, 0));
    integer("editing.backward_word.point", rl_point);
    integer("editing.forward.point", (rl_forward(2, 0), rl_point));
    integer("editing.backward.point", (rl_backward(1, 0), rl_point));
    integer("editing.beg.rc", rl_beg_of_line(1, 0));
    integer("editing.beg.point", rl_point);
    integer("editing.end.rc", rl_end_of_line(1, 0));
    integer("editing.end.point", rl_point);
    integer("editing.forward_byte.rc", rl_forward_byte(1, 0));
    integer("editing.forward_byte.point", rl_point);
    integer("editing.backward_byte.rc", rl_backward_byte(1, 0));
    integer("editing.backward_byte.point", rl_point);

    rl_replace_line("ac", 0);
    rl_point = 1;
    integer("editing.insert.rc", rl_insert(1, 'b'));
    value("editing.insert.line", rl_line_buffer);
    integer("editing.insert.point", rl_point);
    rl_replace_line("", 0);
    integer("editing.tab_insert.rc", rl_tab_insert(1, 0));
    value("editing.tab_insert.line", rl_line_buffer);

    rl_replace_line("abc", 0);
    rl_point = 1;
    integer("editing.delete.rc", rl_delete(1, 0));
    value("editing.delete.line", rl_line_buffer);
    integer("editing.delete.point", rl_point);
    integer("editing.rubout.rc", rl_rubout(1, 0));
    value("editing.rubout.line", rl_line_buffer);
    integer("editing.rubout.point", rl_point);
    rl_replace_line("ab", 0);
    rl_point = 2;
    integer("editing.rubout_or_delete.rc", rl_rubout_or_delete(1, 0));
    value("editing.rubout_or_delete.line", rl_line_buffer);

    rl_replace_line("a  \t b", 0);
    rl_point = 4;
    integer("editing.delete_horizontal_space.rc",
            rl_delete_horizontal_space(1, 0));
    value("editing.delete_horizontal_space.line", rl_line_buffer);
    integer("editing.delete_horizontal_space.point", rl_point);

    rl_replace_line("ab", 0);
    rl_point = 2;
    integer("editing.transpose_chars.rc", rl_transpose_chars(1, 0));
    value("editing.transpose_chars.line", rl_line_buffer);
    integer("editing.transpose_chars.point", rl_point);

    rl_replace_line("one two", 0);
    rl_point = 3;
    integer("editing.transpose_words.rc", rl_transpose_words(1, 0));
    value("editing.transpose_words.line", rl_line_buffer);
    integer("editing.transpose_words.point", rl_point);

    rl_replace_line("hello world", 0);
    rl_point = 0;
    integer("editing.upcase.rc", rl_upcase_word(1, 0));
    value("editing.upcase.line", rl_line_buffer);
    integer("editing.upcase.point", rl_point);
    integer("editing.downcase.rc", rl_downcase_word(1, 0));
    value("editing.downcase.line", rl_line_buffer);
    rl_replace_line("hello wORLD", 0);
    rl_point = 0;
    integer("editing.capitalize.rc", rl_capitalize_word(2, 0));
    value("editing.capitalize.line", rl_line_buffer);
    integer("editing.capitalize.point", rl_point);

    rl_replace_line("hello", 0);
    rl_point = 2;
    integer("editing.set_mark.rc", rl_set_mark(1, 0));
    integer("editing.set_mark.mark", rl_mark);
    integer("editing.exchange.rc", rl_exchange_point_and_mark(1, 0));
    integer("editing.exchange.point", rl_point);
    integer("editing.exchange.mark", rl_mark);
    integer("editing.mark_active", rl_mark_active_p());
    rl_deactivate_mark();
    integer("editing.mark_inactive", rl_mark_active_p());
    rl_activate_mark();
    integer("editing.mark_reactivated", rl_mark_active_p());

    rl_replace_line("one two", 0);
    rl_point = 0;
    rl_mark = 3;
    integer("editing.kill_region.rc", rl_kill_region(1, 0));
    value("editing.kill_region.line", rl_line_buffer);
    integer("editing.kill_region.point", rl_point);
    integer("editing.kill_region.mark", rl_mark);

    rl_replace_line("alpha beta", 0);
    rl_point = 10;
    integer("editing.unix_word_rubout.rc", rl_unix_word_rubout(1, 0));
    value("editing.unix_word_rubout.line", rl_line_buffer);
    integer("editing.unix_word_rubout.point", rl_point);
    rl_replace_line("src/foo.c", 0);
    rl_point = 9;
    integer("editing.unix_filename_rubout.rc", rl_unix_filename_rubout(1, 0));
    value("editing.unix_filename_rubout.line", rl_line_buffer);
    integer("editing.unix_filename_rubout.point", rl_point);

    rl_replace_line("abc", 0);
    rl_point = 2;
    integer("editing.backward_kill_line.rc", rl_backward_kill_line(1, 0));
    value("editing.backward_kill_line.line", rl_line_buffer);
    integer("editing.backward_kill_line.point", rl_point);
    rl_replace_line("abc", 0);
    rl_point = 1;
    integer("editing.kill_line.rc", rl_kill_line(1, 0));
    value("editing.kill_line.line", rl_line_buffer);
    integer("editing.kill_line.point", rl_point);
    rl_replace_line("abc", 0);
    rl_point = 1;
    integer("editing.kill_full_line.rc", rl_kill_full_line(1, 0));
    value("editing.kill_full_line.line", rl_line_buffer);
    integer("editing.kill_full_line.point", rl_point);
    integer("editing.kill_full_line.mark", rl_mark);

    rl_replace_line("one two", 0);
    rl_point = 0;
    integer("editing.copy_forward_word.rc", rl_copy_forward_word(1, 0));
    integer("editing.copy_forward_word.point", rl_point);
    integer("editing.copy_forward_word.mark", rl_mark);

    integer("editing.alphabetic.a", rl_alphabetic('a'));
    integer("editing.alphabetic.Z", rl_alphabetic('Z'));
    integer("editing.alphabetic.0", rl_alphabetic('0'));
    integer("editing.alphabetic.space", rl_alphabetic(' '));
    integer("editing.alphabetic.legacy", alphabetic('q'));
    integer("editing.lowercase_version", rl_do_lowercase_version(1, 'A'));

    integer("editing.insert_mode.before", rl_insert_mode);
    integer("editing.overwrite_mode.rc", rl_overwrite_mode(1, 0));
    integer("editing.insert_mode.after", rl_insert_mode);
    integer("editing.overwrite_state",
            (rl_readline_state & RL_STATE_OVERWRITE) != 0);
    rl_overwrite_mode(1, 0);
    integer("editing.insert_mode.restored", rl_insert_mode);

    section("undo");
    rl_free_undo_list();
    rl_replace_line("abcdef", 0);
    rl_point = 2;
    integer("undo.insert.rc", rl_insert_text("XY"));
    value("undo.insert.line", rl_line_buffer);
    integer("undo.insert.point", rl_point);
    integer("undo.insert.what", rl_undo_list != NULL ? rl_undo_list->what : -1);
    integer("undo.insert.undo", rl_do_undo());
    value("undo.insert.after", rl_line_buffer);
    integer("undo.insert.after.point", rl_point);
    integer("undo.empty", rl_do_undo());

    rl_point = 3;
    integer("undo.delete.rc", rl_delete_text(1, 3));
    value("undo.delete.line", rl_line_buffer);
    integer("undo.delete.undo", rl_do_undo());
    value("undo.delete.after", rl_line_buffer);
    integer("undo.delete.after.point", rl_point);

    rl_free_undo_list();
    rl_replace_line("abc", 0);
    rl_point = 3;
    integer("undo.group.begin", rl_begin_undo_group());
    integer("undo.group.insert1", rl_insert_text("1"));
    integer("undo.group.insert2", rl_insert_text("2"));
    integer("undo.group.end", rl_end_undo_group());
    value("undo.group.line", rl_line_buffer);
    integer("undo.group.undo", rl_do_undo());
    value("undo.group.after", rl_line_buffer);

    rl_free_undo_list();
    rl_replace_line("abc", 0);
    integer("undo.modifying.rc", rl_modifying(0, 2));
    integer("undo.modifying.undo", rl_do_undo());
    value("undo.modifying.line", rl_line_buffer);
    integer("undo.modifying.point", rl_point);

    rl_free_undo_list();
    rl_replace_line("abc", 0);
    rl_point = 3;
    integer("undo.revert.insert", rl_insert_text("d"));
    integer("undo.revert.rc", rl_revert_line(1, 0));
    value("undo.revert.line", rl_line_buffer);
    integer("undo.command.empty", rl_undo_command(1, 0));
    integer("undo.command.negative", rl_undo_command(-1, 0));

    rl_free_undo_list();
    rl_replace_line("original", 0);
    integer("undo.save.rc", rl_maybe_save_line());
    rl_replace_line("changed", 0);
    integer("undo.unsave.rc", rl_maybe_unsave_line());
    value("undo.unsave.line", rl_line_buffer);
    integer("undo.unsave.point", rl_point);
    integer("undo.unsave.again", rl_maybe_unsave_line());

    clear_history();
    add_history("hist");
    using_history();
    history_set_pos(0);
    rl_replace_line("edited", 0);
    rl_add_undo(UNDO_BEGIN, 0, 0, NULL);
    integer("undo.replace.rc", rl_maybe_replace_line());
    value("undo.replace.history",
          history_list() != NULL && history_list()[0] != NULL
              ? history_list()[0]->line
              : NULL);
    free_undo_list();
    integer("undo.free.legacy", rl_undo_list == NULL);
    clear_history();

    section("char-search");
    rl_free_undo_list();
    rl_replace_line("a-b-c-d", 0);
    rl_point = 0;
    rl_pending_input = '-';
    integer("char-search.forward.rc", rl_char_search(2, 0));
    integer("char-search.forward.point", rl_point);
    rl_pending_input = '-';
    integer("char-search.backward.rc", rl_backward_char_search(1, 0));
    integer("char-search.backward.point", rl_point);
    rl_point = 6;
    rl_pending_input = '-';
    integer("char-search.negative.rc", rl_char_search(-1, 0));
    integer("char-search.negative.point", rl_point);
    rl_point = 0;
    rl_pending_input = 'z';
    integer("char-search.miss.rc", rl_char_search(1, 0));
    integer("char-search.miss.point", rl_point);

    section("remaining-text");
    {
        rl_voidfunc_t *saved_redisplay = rl_redisplay_function;
        rl_redisplay_function = noop_redisplay;

        rl_free_undo_list();
        rl_replace_line("echo ~", 0);
        rl_point = 6;
        integer("tilde.rc", rl_tilde_expand(1, 0));
        value("tilde.line", rl_line_buffer);
        rl_replace_line("echo ~/src", 0);
        rl_point = 10;
        integer("tilde.path.rc", rl_tilde_expand(1, 0));
        value("tilde.path.line", rl_line_buffer);
        rl_replace_line("echo plain", 0);
        rl_point = 10;
        integer("tilde.plain.rc", rl_tilde_expand(1, 0));
        value("tilde.plain.line", rl_line_buffer);

        rl_free_undo_list();
        rl_replace_line("echo hi", 0);
        rl_point = 0;
        rl_explicit_arg = 0;
        integer("comment.rc", rl_insert_comment(1, '#'));
        value("comment.line", rl_line_buffer);
        integer("comment.point", rl_point);
        rl_done = 0;
        rl_readline_state &= ~RL_STATE_DONE;
        rl_replace_line("#echo hi", 0);
        rl_point = 0;
        rl_explicit_arg = 1;
        integer("comment.remove.rc", rl_insert_comment(1, '#'));
        value("comment.remove.line", rl_line_buffer);
        integer("comment.remove.point", rl_point);
        rl_done = 0;
        rl_readline_state &= ~RL_STATE_DONE;
        rl_explicit_arg = 0;

        rl_replace_line("", 0);
        rl_point = 0;
        integer("insert-close.rc", rl_insert_close(1, ')'));
        value("insert-close.line", rl_line_buffer);
        integer("insert-close.point", rl_point);

        integer("tty-status.rc", rl_tty_status(1, 0));

        rl_redisplay_function = saved_redisplay;
    }

    section("yank-args");
    clear_history();
    add_history("cmd four five six");
    add_history("cmd one two three");
    using_history();
    rl_free_undo_list();
    rl_replace_line("", 0);
    rl_point = 0;
    rl_explicit_arg = 0;
    rl_numeric_arg = 1;
    integer("yank-nth.rc", rl_yank_nth_arg(1, 0));
    value("yank-nth.line", rl_line_buffer);
    integer("yank-nth.point", rl_point);
    integer("yank-nth.mark", rl_mark);
    rl_replace_line("", 0);
    rl_point = 0;
    integer("yank-nth.last.rc", rl_yank_nth_arg('$', 0));
    value("yank-nth.last.line", rl_line_buffer);
    rl_replace_line("", 0);
    rl_point = 0;
    rl_free_undo_list();
    rl_last_func = NULL;
    rl_explicit_arg = 0;
    rl_numeric_arg = 1;
    integer("yank-last.rc", rl_yank_last_arg(1, 0));
    value("yank-last.line", rl_line_buffer);
    rl_last_func = rl_yank_last_arg;
    rl_explicit_arg = 0;
    rl_numeric_arg = 1;
    integer("yank-last.repeat.rc", rl_yank_last_arg(1, 0));
    value("yank-last.repeat.line", rl_line_buffer);
    rl_replace_line("", 0);
    rl_point = 0;
    integer("yank-nth.miss.rc", rl_yank_nth_arg(9, 0));
    value("yank-nth.miss.line", rl_line_buffer);
    rl_free_undo_list();
    clear_history();

    section("macro");
    integer("macro.print.none", rl_print_last_kbd_macro(1, 0));
    rl_push_macro_input(strdup("abc"));
    value("macro.executing", rl_executing_macro);
    integer("macro.state", (rl_readline_state & RL_STATE_MACROINPUT) != 0);

    section("execute-named");
    {
        rl_voidfunc_t *saved_redisplay = rl_redisplay_function;
        rl_redisplay_function = noop_redisplay;
        rl_replace_line("one two", 0);
        rl_point = 0;
        rl_push_macro_input(strdup("forward-word\n"));
        integer("execute-named.rc", rl_execute_named_command(1, 0));
        value("execute-named.line", rl_line_buffer);
        integer("execute-named.point", rl_point);
        rl_push_macro_input(strdup("no-such-command\n"));
        integer("execute-named.miss.rc", rl_execute_named_command(1, 0));
        rl_redisplay_function = saved_redisplay;
    }

    section("vi");
    rl_vi_editing_mode(1, 0);
    value("vi.insert-keymap", rl_get_keymap_name(rl_get_keymap()));
    integer("vi.insert-mode", rl_insert_mode);
    rl_vi_movement_mode(1, 0);
    value("vi.movement-keymap", rl_get_keymap_name(rl_get_keymap()));

    rl_replace_line("abc", 0);
    rl_point = 3;
    integer("vi.check.rc", rl_vi_check());
    integer("vi.check.point", rl_point);

    rl_replace_line("one two three", 0);
    rl_point = 0;
    integer("vi.fword.rc", rl_vi_fword(1, 'w'));
    integer("vi.fword.point", rl_point);
    integer("vi.bword.rc", rl_vi_bword(1, 'b'));
    integer("vi.bword.point", rl_point);
    integer("vi.eword.rc", rl_vi_eword(1, 'e'));
    integer("vi.eword.point", rl_point);
    integer("vi.next.rc", rl_vi_next_word(1, 'w'));
    integer("vi.next.point", rl_point);
    integer("vi.end.rc", rl_vi_end_word(1, 'e'));
    integer("vi.end.point", rl_point);
    integer("vi.prev.rc", rl_vi_prev_word(1, 'b'));
    integer("vi.prev.point", rl_point);

    rl_replace_line("a.b c", 0);
    rl_point = 0;
    integer("vi.fWord.rc", rl_vi_fWord(1, 'W'));
    integer("vi.fWord.point", rl_point);
    integer("vi.bWord.rc", rl_vi_bWord(1, 'B'));
    integer("vi.bWord.point", rl_point);
    integer("vi.eWord.rc", rl_vi_eWord(1, 'E'));
    integer("vi.eWord.point", rl_point);

    rl_replace_line("   abc", 0);
    rl_point = 0;
    integer("vi.first_print.rc", rl_vi_first_print(1, '^'));
    integer("vi.first_print.point", rl_point);
    integer("vi.back_to_indent.rc", rl_vi_back_to_indent(1, '^'));
    integer("vi.back_to_indent.point", rl_point);
    rl_replace_line("one two", 0);
    rl_point = 0;
    integer("vi.column.rc", rl_vi_column(3, '|'));
    integer("vi.column.point", rl_point);

    rl_replace_line("one two three", 0);
    rl_point = 0;
    rl_mark = 0;
    rl_pending_input = 'w';
    integer("vi.dw.rc", rl_vi_delete_to(1, 'd'));
    value("vi.dw.line", rl_line_buffer);
    integer("vi.dw.point", rl_point);
    integer("vi.dw.end", rl_end);
    integer("vi.dw.mark", rl_mark);
    integer("vi.dw.undo.rc", rl_vi_undo(1, 'u'));
    value("vi.dw.undo.line", rl_line_buffer);
    integer("vi.dw.undo.point", rl_point);

    rl_replace_line("abcdef", 0);
    rl_point = 2;
    integer("vi.delete.rc", rl_vi_delete(1, 'x'));
    value("vi.delete.line", rl_line_buffer);
    integer("vi.delete.point", rl_point);
    integer("vi.delete.undo.rc", rl_vi_undo(1, 'u'));
    value("vi.delete.undo.line", rl_line_buffer);
    integer("vi.delete.undo.point", rl_point);

    rl_replace_line("one two", 0);
    rl_point = 0;
    rl_pending_input = 'w';
    integer("vi.change.rc", rl_vi_change_to(1, 'c'));
    value("vi.change.line", rl_line_buffer);
    integer("vi.change.point", rl_point);
    value("vi.change.keymap", rl_get_keymap_name(rl_get_keymap()));
    integer("vi.change.insert.rc", rl_insert(1, 'X'));
    value("vi.change.insert.line", rl_line_buffer);
    rl_vi_movement_mode(1, 0);
    integer("vi.change.undo.rc", rl_vi_undo(1, 'u'));
    value("vi.change.undo.line", rl_line_buffer);

    rl_replace_line("abcdef", 0);
    rl_point = 1;
    rl_pending_input = 'X';
    integer("vi.change_char.rc", rl_vi_change_char(2, 'r'));
    value("vi.change_char.line", rl_line_buffer);
    integer("vi.change_char.point", rl_point);
    rl_replace_line("aBc def", 0);
    rl_point = 0;
    integer("vi.change_case.rc", rl_vi_change_case(2, '~'));
    value("vi.change_case.line", rl_line_buffer);
    integer("vi.change_case.point", rl_point);

    rl_replace_line("abcde", 0);
    rl_point = 1;
    integer("vi.overstrike.rc", rl_vi_overstrike(1, 'X'));
    value("vi.overstrike.line", rl_line_buffer);
    integer("vi.overstrike.point", rl_point);
    integer("vi.overstrike_delete.rc", rl_vi_overstrike_delete(1, 127));
    value("vi.overstrike_delete.line", rl_line_buffer);
    rl_vi_movement_mode(1, 0);

    rl_replace_line("abc", 0);
    rl_point = 1;
    integer("vi.replace.rc", rl_vi_replace(1, 'R'));
    integer("vi.replace.insert_mode", rl_insert_mode);
    value("vi.replace.keymap", rl_get_keymap_name(rl_get_keymap()));
    rl_vi_movement_mode(1, 0);

    rl_replace_line("abc", 0);
    rl_point = 2;
    integer("vi.rubout.rc", rl_vi_rubout(1, 8));
    value("vi.rubout.line", rl_line_buffer);
    rl_replace_line("foo bar", 0);
    rl_point = 7;
    integer("vi.unix_word_rubout.rc", rl_vi_unix_word_rubout(1, 23));
    value("vi.unix_word_rubout.line", rl_line_buffer);

    rl_replace_line("a,b,c", 0);
    rl_point = 0;
    rl_pending_input = ',';
    integer("vi.char_search.rc", rl_vi_char_search(1, 'f'));
    integer("vi.char_search.point", rl_point);
    integer("vi.char_search_again.rc", rl_vi_char_search(1, ';'));
    integer("vi.char_search_again.point", rl_point);
    integer("vi.char_search_reverse.rc", rl_vi_char_search(1, ','));
    integer("vi.char_search_reverse.point", rl_point);
    rl_pending_input = 'a';
    integer("vi.set_mark.rc", rl_vi_set_mark(1, 'm'));
    rl_point = 4;
    rl_pending_input = 'a';
    integer("vi.goto_mark.rc", rl_vi_goto_mark(1, '`'));
    integer("vi.goto_mark.point", rl_point);

    rl_replace_line("(abc)", 0);
    rl_point = 0;
    integer("vi.bracktype.open", rl_vi_bracktype('('));
    integer("vi.bracktype.close", rl_vi_bracktype(']'));
    integer("vi.match.rc", rl_vi_match(1, '%'));
    integer("vi.match.point", rl_point);

    rl_replace_line("abc", 0);
    rl_point = 1;
    rl_kill_text(1, 2);
    rl_vi_movement_mode(1, 0);
    rl_replace_line("q", 0);
    rl_point = 1;
    integer("vi.put.rc", rl_vi_put(1, 'p'));
    value("vi.put.line", rl_line_buffer);
    integer("vi.put.point", rl_point);

    clear_history();
    add_history("cmd one two");
    add_history("echo alpha beta");
    using_history();
    rl_replace_line("", 0);
    rl_explicit_arg = 1;
    rl_numeric_arg = 1;
    integer("vi.fetch.rc", rl_vi_fetch_history(1, 'G'));
    value("vi.fetch.line", rl_line_buffer);
    integer("vi.fetch.point", rl_point);
    integer("vi.fetch.mark", rl_mark);
    rl_explicit_arg = 0;
    rl_numeric_arg = 1;

    using_history();
    rl_replace_line("x", 0);
    rl_point = 0;
    integer("vi.yank_arg.rc", rl_vi_yank_arg(1, ':'));
    value("vi.yank_arg.line", rl_line_buffer);
    integer("vi.yank_arg.point", rl_point);
    rl_vi_movement_mode(1, 0);
    clear_history();
    rl_emacs_editing_mode(1, 0);

    section("keyseq");
    print_keyseq("translate.control", "\\C-x");
    print_keyseq("translate.meta", "\\M-x");
    print_keyseq("translate.escape", "\\e[1;5C");
    print_keyseq("translate.rubout", "^?");
    print_keyseq("translate.quoted", "\"\\C-x\\C-r\"");

    section("keymap");
    value("keymap.default", rl_get_keymap_name(rl_get_keymap()));
    value("keymap.emacs", rl_get_keymap_name(rl_get_keymap_by_name("emacs")));
    value("keymap.meta", rl_get_keymap_name(rl_get_keymap_by_name("emacs-meta")));
    value("keymap.vi", rl_get_keymap_name(rl_get_keymap_by_name("vi")));
    value("keymap.vi-insert", rl_get_keymap_name(rl_get_keymap_by_name("vi-insert")));
    value("named.beginning-of-line",
          rl_named_function("beginning-of-line") != NULL ? "found" : "missing");
    value("named.forward-word",
          rl_named_function("forward-word") != NULL ? "found" : "missing");
    value("named.unknown", rl_named_function("no-such-function") != NULL ? "found" : "missing");
    {
        char **seqs = rl_invoking_keyseqs(rl_named_function("beginning-of-line"));
        int count = free_sorted(seqs);
        (void)count;
        seqs = rl_invoking_keyseqs(rl_named_function("beginning-of-line"));
        // print_sorted frees nothing, so count first and print a copy.
        int n = 0;
        if (seqs != NULL) {
            while (seqs[n] != NULL) {
                n++;
            }
        }
        print_sorted(seqs, n, "invoking.beginning-of-line");
        // print_sorted sorted in place; free items.
        if (seqs != NULL) {
            for (int i = 0; i < n; i++) {
                free(seqs[i]);
            }
            free(seqs);
        }
    }

    section("bind");
    rl_add_defun("conformance-command", test_command, -1);
    value("defun", rl_named_function("conformance-command") == test_command ? "found" : "missing");
    integer("bind.custom.rc", rl_bind_keyseq("\\C-xy", test_command));
    value("bind.custom.function",
          function_of_spelling("\\C-xy") == test_command ? "found" : "missing");
    {
        char bind_line[] = "\"\\C-xz\": conformance-command";
        integer("bind.initrc.rc", rl_parse_and_bind(bind_line));
    }
    value("bind.initrc.function",
          function_of_spelling("\\C-xz") == test_command ? "found" : "missing");
    {
        char **seqs = rl_invoking_keyseqs(test_command);
        int n = 0;
        if (seqs != NULL) {
            while (seqs[n] != NULL) {
                n++;
            }
        }
        print_sorted(seqs, n, "invoking.conformance-command");
        if (seqs != NULL) {
            for (int i = 0; i < n; i++) {
                free(seqs[i]);
            }
            free(seqs);
        }
    }

    section("variables");
    value("variable.editing-mode", rl_variable_value("editing-mode"));
    value("variable.convert-meta", rl_variable_value("convert-meta"));
    value("variable.keyseq-timeout", rl_variable_value("keyseq-timeout"));
    integer("variable.bind.convert-meta", rl_variable_bind("convert-meta", "off"));
    value("variable.convert-meta.after", rl_variable_value("convert-meta"));
    integer("variable.bind.convert-meta.back", rl_variable_bind("convert-meta", "on"));
    value("variable.unknown", rl_variable_value("no-such-variable"));

    section("inputrc");
    {
        char path[512];
        snprintf(path, sizeof(path), "%s/conformance-inputrc", root_dir);
        write_file(path,
                   "set editing-mode emacs\n"
                   "set keyseq-timeout 123\n"
                   "\"\\C-xt\": transpose-chars\n"
                   "$if mode=emacs\n"
                   "\"\\C-xe\": forward-word\n"
                   "$else\n"
                   "\"\\C-xv\": backward-word\n"
                   "$endif\n");
        integer("inputrc.rc", rl_read_init_file(path));
        value("inputrc.keyseq-timeout", rl_variable_value("keyseq-timeout"));
        value("inputrc.transpose",
              function_of_spelling("\\C-xt") == rl_named_function("transpose-chars")
                  ? "found"
                  : "missing");
        value("inputrc.forward",
              function_of_spelling("\\C-xe") == rl_named_function("forward-word")
                  ? "found"
                  : "missing");
        unlink(path);
    }

    section("history");
    using_history();
    clear_history();
    add_history("first");
    add_history("second");
    add_history("third");
    integer("history.length", history_length);
    integer("history.base", history_base);
    integer("history.where", where_history());
    value("history.current", current_history() ? current_history()->line : NULL);
    value("history.get0", history_get(history_base) ? history_get(history_base)->line : NULL);
    value("history.get1", history_get(history_base + 1)
                              ? history_get(history_base + 1)->line
                              : NULL);
    integer("history.setpos0", history_set_pos(0));
    value("history.previous", previous_history() ? previous_history()->line : NULL);
    value("history.next", next_history() ? next_history()->line : NULL);
    integer("history.total", history_total_bytes());
    history_comment_char = 0;
    {
        char path[512];
        snprintf(path, sizeof(path), "%s/conformance-history", root_dir);
        unlink(path);
        integer("history.write", write_history(path));
        clear_history();
        integer("history.read", read_history(path));
        integer("history.read.length", history_length);
        value("history.read.first", history_get(history_base)
                                        ? history_get(history_base)->line
                                        : NULL);
        unlink(path);
    }
    add_history("one");
    add_history("two");
    stifle_history(1);
    integer("history.stifled", history_is_stifled());
    integer("history.stifled.length", history_length);
    unstifle_history();

    section("history-abi");
    clear_history();
    add_history("one");
    add_history("two");
    add_history("three");
    using_history();
    {
        HIST_ENTRY **list = history_list();
        integer("list.length", history_length);
        value("list.0", list != NULL ? list[0]->line : NULL);
        value("list.1", list != NULL ? list[1]->line : NULL);
        value("list.2", list != NULL ? list[2]->line : NULL);
        integer("list.null_terminated", list != NULL && list[3] == NULL);

        HISTORY_STATE *state = history_get_history_state();
        integer("state.offset", state->offset);
        integer("state.length", state->length);
        integer("state.flags", state->flags);
        integer("state.entries_match", state->entries == list);
        state->offset = 1;
        history_set_history_state(state);
        integer("state.set.offset", where_history());
        free(state);

        HIST_ENTRY *removed = remove_history(1);
        value("remove.line", removed != NULL ? removed->line : NULL);
        integer("remove.length", history_length);
        value("remove.list0", history_list() != NULL ? history_list()[0]->line : NULL);
        value("remove.list1", history_list() != NULL ? history_list()[1]->line : NULL);
        free_history_entry(removed);

        HIST_ENTRY *old = replace_history_entry(1, "THREE", (histdata_t)0);
        value("replace.old", old != NULL ? old->line : NULL);
        value("replace.new", history_list() != NULL ? history_list()[1]->line : NULL);
        integer("replace.length", history_length);
        free_history_entry(old);
    }
    {
        HIST_ENTRY *allocated = alloc_history_entry("allocated", strdup("#123"));
        HIST_ENTRY *copy = copy_history_entry(allocated);
        value("alloc.line", allocated != NULL ? allocated->line : NULL);
        value("alloc.timestamp", allocated != NULL ? allocated->timestamp : NULL);
        integer("alloc.distinct_line", allocated != NULL && copy != NULL && allocated->line != copy->line);
        history_comment_char = '#';
        integer("alloc.time", allocated != NULL ? (int)history_get_time(allocated) : -1);
        history_comment_char = 0;
        free_history_entry(copy);
        free_history_entry(allocated);
    }
    {
        HIST_ENTRY **range = remove_history_range(0, 0);
        value("range.0", range != NULL ? range[0]->line : NULL);
        integer("range.null_terminated", range != NULL && range[1] == NULL);
        integer("range.length", history_length);
        value("range.remaining", history_list() != NULL ? history_list()[0]->line : NULL);
        if (range != NULL) {
            free_history_entry(range[0]);
            free(range);
        }
    }

    clear_history();
    add_history("alpha one");
    add_history("beta two");
    add_history("alphabet");
    add_history("gamma alpha");
    using_history();
    integer("search.back", history_search("alpha", -1));
    integer("search.back.where", where_history());
    history_set_pos(2);
    integer("search.back.earlier", history_search("alpha", -1));
    integer("search.back.earlier.where", where_history());
    history_set_pos(0);
    integer("search.forward", history_search("alpha", 1));
    integer("search.forward.where", where_history());
    history_set_pos(4);
    integer("search.prefix.back", history_search_prefix("alpha", -1));
    integer("search.prefix.where", where_history());
    integer("search.prefix.miss", history_search_prefix("zzz", -1));
    integer("search.pos", history_search_pos("alpha", -1, 2));
    integer("search.pos.restored", where_history());

    clear_history();
    add_history("first");
    add_history("second arg");
    add_history("third thing here");
    using_history();
    {
        static const char *const cases[] = {
            "echo !!",
            "echo !2",
            "echo !-1",
            "echo !sec",
            "echo !nosuch",
            "echo !!:$",
            "echo !!:*",
            "echo !!:q",
            "echo !!:p",
            "echo !?second?:s/arg/ARG/",
            "echo '!!' \"!!\"",
            "echo \\!!",
        };
        for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
            char *output = NULL;
            int rc = history_expand(cases[i], &output);
            printf("expand.%zu|rc=%d|%s\n", i, rc, output == NULL ? "<null>" : output);
            free(output);
        }
    }
    {
        char *arguments = history_arg_extract(1, 2, "cmd one two three");
        value("arg.1-2", arguments);
        free(arguments);
        arguments = history_arg_extract(0, 0, "cmd one two three");
        value("arg.0", arguments);
        free(arguments);
        arguments = history_arg_extract((int)'$', (int)'$', "cmd one two three");
        value("arg.last", arguments);
        free(arguments);
    }
    {
        static const char *const cases[] = {
            "echo 'a b' \"c d\"",
            "a\\ b c",
            "foo|bar;baz",
            "cmd $(echo hi) end",
            "x 2>/dev/null",
            "a&&b||c",
        };
        for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
            char **words = history_tokenize(cases[i]);
            printf("tokenize.%zu|", i);
            if (words == NULL) {
                printf("<null>\n");
                continue;
            }
            for (int word = 0; words[word] != NULL; word++) {
                printf("[%s]", words[word]);
                free(words[word]);
            }
            printf("\n");
            free(words);
        }
    }
    {
        char path[512];
        snprintf(path, sizeof(path), "%s/conformance-truncate", root_dir);
        write_file(path, "one\ntwo\nthree\nfour\n");
        integer("truncate.rc", history_truncate_file(path, 2));
        FILE *file = fopen(path, "r");
        if (file != NULL) {
            char line[128];
            while (fgets(line, sizeof(line), file) != NULL) {
                printf("truncate.line|%s", line);
            }
            fclose(file);
        }
        unlink(path);
    }

    section("completion");
    {
        char **matches = rl_completion_matches("al", generator);
        int n = 0;
        if (matches != NULL) {
            while (matches[n] != NULL) {
                n++;
            }
        }
        print_sorted(matches, n, "completion.generator");
        if (matches != NULL) {
            for (int i = 0; i < n; i++) {
                free(matches[i]);
            }
            free(matches);
        }
    }
    {
        char *dir = (char *)make_fixture_dir();
        char path[512];
        snprintf(path, sizeof(path), "%s/alpha.txt", dir);
        write_file(path, "a");
        snprintf(path, sizeof(path), "%s/alpine.txt", dir);
        write_file(path, "b");
        snprintf(path, sizeof(path), "%s/beta.txt", dir);
        write_file(path, "c");
        {
            char *saved = getcwd(NULL, 0);
            chdir(dir);
            char **matches = NULL;
            int n = 0;
            for (int state = 0;; state++) {
                char *match = rl_filename_completion_function("al", state);
                if (match == NULL) {
                    break;
                }
                matches = realloc(matches, sizeof(*matches) * (size_t)(n + 2));
                matches[n++] = match;
                matches[n] = NULL;
            }
            if (matches != NULL) {
                matches[n] = NULL;
            }
            print_sorted(matches, n, "completion.filename");
            if (matches != NULL) {
                for (int i = 0; i < n; i++) {
                    free(matches[i]);
                }
                free(matches);
            }
            if (saved != NULL) {
                chdir(saved);
                free(saved);
            }
        }
        free(dir);
    }
    {
        char **matches = NULL;
        int n = 0;
        for (int state = 0;; state++) {
            char *match = rl_username_completion_function("ro", state);
            if (match == NULL) {
                break;
            }
            matches = realloc(matches, sizeof(*matches) * (size_t)(n + 2));
            matches[n++] = match;
            matches[n] = NULL;
        }
        if (matches != NULL) {
            matches[n] = NULL;
        }
        print_sorted(matches, n, "completion.username");
        if (matches != NULL) {
            for (int i = 0; i < n; i++) {
                free(matches[i]);
            }
            free(matches);
        }
    }

    section("display-state");
    integer("gnu_readline_p", rl_gnu_readline_p);
    integer("editing_mode", rl_editing_mode);
    integer("eof_found", rl_eof_found);
    integer("dispatching", rl_dispatching);
    integer("prefer_env_winsize", rl_prefer_env_winsize);
    integer("erase_empty_line", rl_erase_empty_line);
    integer("already_prompted", rl_already_prompted);
    integer("num_chars_to_read", rl_num_chars_to_read);
    integer("executing_macro_null", rl_executing_macro == NULL);
    integer("instream_stdin", rl_instream == stdin);
    integer("outstream_stdout", rl_outstream == stdout);
    value("terminal_name", rl_terminal_name);
    integer("character_len.printable", rl_character_len('a', 0));
    integer("character_len.control", rl_character_len(0x01, 0));
    integer("character_len.rubout", rl_character_len(0x7f, 0));
    integer("character_len.meta", rl_character_len(0x80, 0));
    integer("character_len.tab.0", rl_character_len('\t', 0));
    integer("character_len.tab.3", rl_character_len('\t', 3));
    integer("struct.readline_state.size", (int)sizeof(struct readline_state));
    integer("struct.ul.offset", (int)offsetof(struct readline_state, ul));
    integer("struct.kmap.offset", (int)offsetof(struct readline_state, kmap));
    integer("struct.macro.offset", (int)offsetof(struct readline_state, macro));
    integer("struct.wordbreakchars.offset",
            (int)offsetof(struct readline_state, wordbreakchars));
    integer("struct.reserved.offset",
            (int)offsetof(struct readline_state, reserved));
    integer("struct.reserved.size",
            (int)sizeof(((struct readline_state *)0)->reserved));

    section("saved-state");
    {
        struct readline_state saved;
        char *original;
        char *replacement;
        rl_replace_line("snapshot", 0);
        rl_point = 3;
        rl_mark = 2;
        rl_done = 0;
        integer("save.rc", rl_save_state(&saved));
        original = rl_line_buffer;
        replacement = strdup("changed");
        rl_line_buffer = replacement;
        rl_end = 7;
        rl_point = 1;
        rl_mark = 0;
        rl_done = 1;
        integer("restore.rc", rl_restore_state(&saved));
        integer("restore.buffer_same", rl_line_buffer == original);
        value("restore.line", rl_line_buffer);
        integer("restore.point", rl_point);
        integer("restore.mark", rl_mark);
        integer("restore.done", rl_done);
        free(replacement);
    }

    return 0;
}
