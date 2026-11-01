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

    return 0;
}
