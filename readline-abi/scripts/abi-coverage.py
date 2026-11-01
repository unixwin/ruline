#!/usr/bin/env python3
"""Measure GNU readline ABI coverage for readline-abi.

The GNU 8.3 public headers are the specification.  A symbol counts as
implemented when the built libreadline exports it *and* the matching public
header declares it.  Names are grouped by functional area so the report can
drive the next implementation slices.

The vendor headers are parsed directly (or via `--vendor` /
`READLINE_ABI_VENDOR_HEADERS`).  Only names, kinds, and grouping are stored;
no GNU header text is copied into this repository.

Examples:

    # human-readable report against the release library
    readline-abi/scripts/abi-coverage.py

    # machine-readable report
    readline-abi/scripts/abi-coverage.py --json

    # refresh the checked-in snapshot
    readline-abi/scripts/abi-coverage.py --snapshot

    # CI: fail when abi/coverage.md is stale
    readline-abi/scripts/abi-coverage.py --check
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
CRATE = HERE.parent
REPO = CRATE.parent
DEFAULT_SNAPSHOT = CRATE / "abi" / "coverage.md"
DEFAULT_JSON = False
OUR_HEADERS = [
    CRATE / "include" / "readline" / "readline.h",
    CRATE / "include" / "readline" / "history.h",
]
VENDOR_HEADERS = ("readline.h", "keymaps.h", "history.h")

# `rl_echoing_p` is a compatibility spelling for GNU's internal `_rl_echoing_p`.
# GNU does not publish it in readline.h, but our ABI promise includes it.
PROJECT_EXTENSIONS = {"rl_echoing_p"}

# These names only describe C syntax, never the ABI surface.
C_KEYWORDS = {
    "extern",
    "const",
    "volatile",
    "int",
    "char",
    "short",
    "long",
    "void",
    "unsigned",
    "signed",
    "float",
    "double",
    "struct",
    "union",
    "enum",
    "size_t",
    "ssize_t",
    "FILE",
    "time_t",
    "histdata_t",
    "Keymap",
}

AREA_ORDER = [
    "state",
    "text",
    "keymap",
    "completion",
    "hooks",
    "history",
    "callbacks",
    "terminal",
    "display",
    "misc",
]

VENDOR_CANDIDATES = [
    CRATE.parents[2] / "projects" / "rubash" / "third_party" / "bash" / "lib" / "readline",
    REPO / "third_party" / "bash" / "lib" / "readline",
    Path("/data/data/com.termux/files/home/projects/rubash/third_party/bash/lib/readline"),
]


# --- Header parsing -------------------------------------------------------


@dataclass(frozen=True)
class Symbol:
    name: str
    kind: str  # "function" or "global"


def strip_comments(text: str) -> str:
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def split_declarators(body: str) -> list[str]:
    """Split a declaration body on top-level commas.

    Commas inside parameter lists, array subscripts, and `__attribute__`
    argument lists stay attached to their declarator.
    """
    parts: list[str] = []
    depth = 0
    start = 0
    for index, character in enumerate(body):
        if character in "([":
            depth += 1
        elif character in ")]":
            depth = max(0, depth - 1)
        elif character == "," and depth == 0:
            parts.append(body[start:index])
            start = index + 1
    parts.append(body[start:])
    return parts


def is_symbol_name(name: str) -> bool:
    return (
        not name.startswith("__")
        and not name.endswith("_t")
        and name not in C_KEYWORDS
    )


def declaration_symbols(text: str) -> list[Symbol]:
    """Return function/global declarations from `extern` statements.

    Multi-line declarations and multi-declarator globals such as
    `extern KEYMAP_ENTRY_ARRAY a, b, c;` are handled.  Legacy aliases inside
    `#if 0` blocks are deliberately included: GNU's compat.c still exports
    them even though the header disables the declarations.
    """
    symbols: list[Symbol] = []
    seen: set[tuple[str, str]] = set()
    for match in re.finditer(r'\bextern\s+(?!["])([^;]*);', strip_comments(text), flags=re.S):
        body = match.group(1)
        for raw_part in split_declarators(body):
            # Attributes carry their own parenthesised argument lists and
            # cannot contribute a declarator name.
            part = re.sub(r"__attribute__\s*\(\(.*?\)\)", "", raw_part, flags=re.S).strip()
            if not part:
                continue

            # Function-pointer variable: `extern rl_hook_func_t *rl_hook;`
            pointer = re.search(r"\(\s*\*\s*([A-Za-z_]\w*)\s*\)", part)
            if pointer:
                name = pointer.group(1)
                if is_symbol_name(name) and (name, "global") not in seen:
                    seen.add((name, "global"))
                    symbols.append(Symbol(name, "global"))
                continue

            # Function: first identifier immediately before a top-level `(`.
            function_name = None
            depth = 0
            for index, character in enumerate(part):
                if character == "(":
                    if depth == 0:
                        prefix = part[:index]
                        candidate = re.search(r"([A-Za-z_]\w*)\s*$", prefix)
                        if candidate:
                            function_name = candidate.group(1)
                        break
                    depth += 1
                elif character == ")":
                    depth = max(0, depth - 1)

            if function_name and is_symbol_name(function_name):
                if (function_name, "function") not in seen:
                    seen.add((function_name, "function"))
                    symbols.append(Symbol(function_name, "function"))
                continue

            # Global: the last identifier of the declarator.
            identifiers = re.findall(r"[A-Za-z_]\w*", part)
            if identifiers:
                name = identifiers[-1]
                if is_symbol_name(name) and (name, "global") not in seen:
                    seen.add((name, "global"))
                    symbols.append(Symbol(name, "global"))
    return symbols


def parse_headers(paths: list[Path]) -> dict[str, Symbol]:
    result: dict[str, Symbol] = {}
    for path in paths:
        for symbol in declaration_symbols(path.read_text(encoding="utf-8", errors="replace")):
            result.setdefault(symbol.name, symbol)
    return result


# --- Vendor discovery and library inspection ------------------------------


def vendor_directory(override: str | None) -> Path | None:
    candidates: list[Path] = []
    if override:
        candidates.append(Path(override))
    from_env = os.environ.get("READLINE_ABI_VENDOR_HEADERS")
    if from_env:
        candidates.append(Path(from_env))
    candidates.extend(VENDOR_CANDIDATES)
    for candidate in candidates:
        if candidate.is_dir() and all((candidate / name).is_file() for name in VENDOR_HEADERS):
            return candidate
    return None


def exported_symbols(library: Path) -> set[str]:
    commands = [
        ["nm", "-D", "--defined-only", str(library)],
        ["nm", "-gU", str(library)],
        ["readelf", "--wide", "--dyn-syms", str(library)],
    ]
    last_error = ""
    for command in commands:
        try:
            result = subprocess.run(command, capture_output=True, text=True, check=False)
        except FileNotFoundError as error:
            last_error = str(error)
            continue
        if result.returncode != 0:
            last_error = result.stderr.strip()
            continue

        names: set[str] = set()
        for line in result.stdout.splitlines():
            fields = line.split()
            if not fields:
                continue
            name = fields[-1]
            name = name.split("@", 1)[0]
            # readelf prints `Name` as the last field, nm prints the same.
            # Skip headers and undefined/version entries.
            if name in {"Name", "Type"} or name.startswith("__"):
                continue
            # Mach-O nm prefixes C symbols with an underscore; Rust/ELF symbols
            # do not.  Strip one prefix and let the spec/declaration filter
            # decide whether the result is an ABI name.
            if name.startswith("_"):
                name = name[1:]
            names.add(name)
        return names
    raise RuntimeError(f"could not inspect {library}: {last_error}")


def find_library(explicit: str | None) -> Path:
    candidates: list[Path] = []
    if explicit:
        candidates.append(Path(explicit))
    from_env = os.environ.get("READLINE_ABI_LIB")
    if from_env:
        candidates.append(Path(from_env))
    for directory in ("release", "debug"):
        for name in ("libreadline.so", "libreadline.dylib", "readline.dll"):
            candidates.append(REPO / "target" / directory / name)
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    raise FileNotFoundError(
        "libreadline not found; build it with "
        "`cargo build -p readline-abi --release` or pass --library PATH"
    )


def build_release() -> None:
    subprocess.run(
        ["cargo", "build", "-p", "readline-abi", "--release"],
        cwd=REPO,
        check=True,
    )


# --- Functional areas -----------------------------------------------------


HISTORY_NAMES = {
    "add_history",
    "add_history_time",
    "alloc_history_entry",
    "append_history",
    "clear_history",
    "copy_history_entry",
    "current_history",
    "free_history_entry",
    "get_history_event",
    "history_arg_extract",
    "history_expand",
    "history_get_history_state",
    "history_inhibit_expansion_function",
    "history_search",
    "history_search_pos",
    "history_search_prefix",
    "history_set_history_state",
    "history_tokenize",
    "max_input_history",
    "rl_beginning_of_history",
    "rl_clear_history",
    "rl_end_of_history",
    "rl_fetch_history",
    "rl_forward_search_history",
    "rl_get_next_history",
    "rl_get_previous_history",
    "rl_history_search_backward",
    "rl_history_search_forward",
    "rl_history_substr_search_backward",
    "rl_history_substr_search_forward",
    "rl_noninc_forward_search",
    "rl_noninc_forward_search_again",
    "rl_noninc_reverse_search",
    "rl_noninc_reverse_search_again",
    "rl_operate_and_get_next",
    "rl_reverse_search_history",
    "rl_vi_fetch_history",
    "rl_vi_search",
    "rl_vi_search_again",
    "next_history",
    "previous_history",
    "read_history",
    "read_history_range",
    "remove_history",
    "remove_history_range",
    "replace_history_entry",
    "stifle_history",
    "unstifle_history",
    "using_history",
    "where_history",
    "write_history",
}

CALLBACK_NAMES = {
    "rl_callback_handler_install",
    "rl_callback_read_char",
    "rl_callback_handler_remove",
    "rl_callback_sigcleanup",
}

HOOK_NAMES = {
    "rl_startup_hook",
    "rl_pre_input_hook",
    "rl_event_hook",
    "rl_signal_event_hook",
    "rl_timeout_event_hook",
    "rl_input_available_hook",
    "rl_getc_function",
    "rl_redisplay_function",
    "rl_prep_term_function",
    "rl_deprep_term_function",
    "rl_macro_display_hook",
}

KEYMAP_NAMES = {
    "funmap",
    "emacs_standard_keymap",
    "emacs_meta_keymap",
    "emacs_ctlx_keymap",
    "vi_insertion_keymap",
    "vi_movement_keymap",
    "rl_executing_keymap",
    "rl_binding_keymap",
    "rl_executing_key",
    "rl_executing_keyseq",
    "rl_key_sequence_length",
    "rl_last_func",
    "rl_set_key",
}

KEYMAP_PREFIXES = (
    "rl_bind",
    "rl_unbind",
    "rl_add_defun",
    "rl_add_funmap_entry",
    "rl_funmap_names",
    "rl_initialize_funmap",
    "rl_list_funmap_names",
    "rl_named_function",
    "rl_function_of_keyseq",
    "rl_invoking_keyseqs",
    "rl_get_keymap",
    "rl_set_keymap",
    "rl_make_bare_keymap",
    "rl_make_keymap",
    "rl_copy_keymap",
    "rl_discard_keymap",
    "rl_free_keymap",
    "rl_empty_keymap",
    "rl_translate_keyseq",
    "rl_untranslate_keyseq",
    "rl_generic_bind",
    "rl_macro_bind",
    "rl_trim_arg_from_keyseq",
    "rl_print_keybinding",
    "rl_function_dumper",
    "rl_macro_dumper",
    "rl_variable_dumper",
    "rl_dump_functions",
    "rl_dump_macros",
    "rl_dump_variables",
    "rl_read_init_file",
    "rl_parse_and_bind",
    "rl_re_read_init_file",
    "rl_variable_value",
    "rl_variable_bind",
)

TERMINAL_NAMES = {
    "rl_prep_terminal",
    "rl_deprep_terminal",
    "rl_tty_set_default_bindings",
    "rl_tty_unset_default_bindings",
    "rl_tty_set_echoing",
    "rl_reset_terminal",
    "rl_resize_terminal",
    "rl_set_screen_size",
    "rl_get_screen_size",
    "rl_reset_screen_size",
    "rl_get_termcap",
    "rl_reparse_colors",
    "rl_stuff_char",
    "rl_execute_next",
    "rl_clear_pending_input",
    "rl_read_key",
    "rl_getc",
    "rl_set_keyboard_input_timeout",
    "rl_set_timeout",
    "rl_timeout_remaining",
    "rl_set_paren_blink_timeout",
    "rl_echo_signal_char",
}

DISPLAY_NAMES = {
    "rl_redisplay",
    "rl_on_new_line",
    "rl_on_new_line_with_prompt",
    "rl_forced_update_display",
    "rl_clear_visible_line",
    "rl_clear_message",
    "rl_reset_line_state",
    "rl_crlf",
    "crlf",
    "rl_message",
    "rl_show_char",
    "rl_character_len",
    "rl_redraw_prompt_last_line",
    "rl_save_prompt",
    "rl_restore_prompt",
    "rl_expand_prompt",
    "rl_already_prompted",
    "rl_erase_empty_line",
    "rl_num_chars_to_read",
    "rl_refresh_line",
    "rl_clear_screen",
    "rl_clear_display",
    "rl_skip_csi_sequence",
    "rl_arrow_keys",
    "rl_previous_screen_line",
    "rl_next_screen_line",
}

STATE_NAMES = {
    "readline",
    "rl_initialize",
    "rl_set_prompt",
    "rl_save_state",
    "rl_restore_state",
    "rl_discard_argument",
    "rl_line_buffer",
    "rl_point",
    "rl_end",
    "rl_mark",
    "rl_readline_state",
    "rl_done",
    "rl_pending_input",
    "rl_prompt",
    "rl_display_prompt",
    "rl_readline_name",
    "rl_echoing_p",
    "rl_insert_mode",
    "rl_library_version",
    "rl_readline_version",
    "rl_gnu_readline_p",
    "rl_editing_mode",
    "rl_eof_found",
    "rl_dispatching",
    "rl_explicit_arg",
    "rl_numeric_arg",
    "rl_terminal_name",
    "rl_instream",
    "rl_outstream",
    "rl_prefer_env_winsize",
    "rl_undo_list",
    "rl_executing_macro",
}

MISC_NAMES = {
    "rl_set_signals",
    "rl_clear_signals",
    "rl_cleanup_after_signal",
    "rl_reset_after_signal",
    "rl_free_line_state",
    "rl_pending_signal",
    "rl_check_signals",
    "rl_catch_signals",
    "rl_catch_sigwinch",
    "rl_change_environment",
    "rl_persistent_signal_handlers",
}


def classify(symbol: Symbol) -> str:
    """Map a symbol to one of AREA_ORDER, first match wins.

    Completion is checked before hooks so completion callback storage lands in
    `completion`; history is checked first so `history_inhibit_expansion_function`
    lands in `history`.
    """
    name = symbol.name

    if name in HISTORY_NAMES or name.startswith("history_"):
        return "history"

    if name in CALLBACK_NAMES or name.startswith("rl_callback"):
        return "callbacks"

    if (
        "complet" in name
        or name in {"completion_matches", "filename_completion_function", "username_completion_function"}
        or name.startswith(("rl_filename_", "rl_directory_", "rl_completer_"))
        or name
        in {
            "rl_basic_word_break_characters",
            "rl_basic_quote_characters",
            "rl_special_prefixes",
            "rl_char_is_quoted_p",
            "rl_full_quoting_desired",
            "rl_display_match_list",
        }
    ):
        return "completion"

    if name in HOOK_NAMES or name.endswith("_hook"):
        return "hooks"

    if name in KEYMAP_NAMES or name.startswith(KEYMAP_PREFIXES):
        return "keymap"

    if name in TERMINAL_NAMES:
        return "terminal"

    if name in DISPLAY_NAMES:
        return "display"

    if name in STATE_NAMES:
        return "state"

    if name in MISC_NAMES:
        return "misc"

    # Editing commands and their support helpers are the large remaining
    # function group.  Unmatched globals stay in misc rather than being
    # mistaken for text APIs.
    if symbol.kind == "function":
        return "text"
    return "misc"


# --- Measurement ----------------------------------------------------------


@dataclass
class AreaStats:
    name: str
    implemented_functions: int = 0
    total_functions: int = 0
    implemented_globals: int = 0
    total_globals: int = 0

    @property
    def implemented(self) -> int:
        return self.implemented_functions + self.implemented_globals

    @property
    def total(self) -> int:
        return self.total_functions + self.total_globals

    @property
    def missing(self) -> int:
        return self.total - self.implemented

    @property
    def coverage(self) -> float:
        return percent(self.implemented, self.total)


@dataclass
class Coverage:
    spec_source: str
    spec: dict[str, Symbol]
    declared: dict[str, Symbol]
    exported: set[str]
    implemented: dict[str, Symbol] = field(default_factory=dict)
    missing: dict[str, Symbol] = field(default_factory=dict)
    areas: dict[str, AreaStats] = field(default_factory=dict)
    diagnostics: dict[str, list[str]] = field(default_factory=dict)
    library_path: str = ""

    def compute(self) -> None:
        self.implemented = {
            name: symbol
            for name, symbol in self.spec.items()
            if name in self.exported and name in self.declared
        }
        self.missing = {
            name: symbol for name, symbol in self.spec.items() if name not in self.implemented
        }

        self.areas = {area: AreaStats(area) for area in AREA_ORDER}
        for symbol in self.spec.values():
            area = self.areas[classify(symbol)]
            if symbol.kind == "function":
                area.total_functions += 1
            else:
                area.total_globals += 1
        for symbol in self.implemented.values():
            area = self.areas[classify(symbol)]
            if symbol.kind == "function":
                area.implemented_functions += 1
            else:
                area.implemented_globals += 1

        self.diagnostics = {
            "declared_not_exported": sorted(
                name for name in self.declared if name not in self.exported
            ),
            "exported_not_declared": sorted(
                name
                for name in self.exported
                if name not in self.declared and (name in self.spec or name.startswith(("rl_", "history_")))
            ),
            "declared_not_in_spec": sorted(
                name for name in self.declared if name not in self.spec and name not in PROJECT_EXTENSIONS
            ),
            "kind_mismatches": sorted(
                name
                for name, symbol in self.spec.items()
                if name in self.declared and self.declared[name].kind != symbol.kind
            ),
        }

    @property
    def total_functions(self) -> int:
        return sum(1 for symbol in self.spec.values() if symbol.kind == "function")

    @property
    def total_globals(self) -> int:
        return sum(1 for symbol in self.spec.values() if symbol.kind == "global")

    @property
    def implemented_functions(self) -> int:
        return sum(1 for symbol in self.implemented.values() if symbol.kind == "function")

    @property
    def implemented_globals(self) -> int:
        return sum(1 for symbol in self.implemented.values() if symbol.kind == "global")

    @property
    def missing_functions(self) -> int:
        return self.total_functions - self.implemented_functions

    @property
    def missing_globals(self) -> int:
        return self.total_globals - self.implemented_globals

    @property
    def coverage(self) -> float:
        return percent(len(self.implemented), len(self.spec))

    def grouped_missing(self) -> dict[str, list[Symbol]]:
        grouped: dict[str, list[Symbol]] = {area: [] for area in AREA_ORDER}
        for symbol in sorted(self.missing.values(), key=lambda item: item.name):
            grouped[classify(symbol)].append(symbol)
        return grouped

    def top_missing_groups(self, limit: int = 5) -> list[AreaStats]:
        return sorted(
            (area for area in self.areas.values() if area.missing > 0),
            key=lambda area: (-area.missing, AREA_ORDER.index(area.name)),
        )[:limit]

    def to_json(self) -> dict:
        grouped = {
            area: [
                {"name": symbol.name, "kind": symbol.kind}
                for symbol in symbols
            ]
            for area, symbols in self.grouped_missing().items()
            if symbols
        }
        return {
            "schema": 1,
            "specification": {
                "source": self.spec_source,
                "headers": list(VENDOR_HEADERS),
                "functions": self.total_functions,
                "globals": self.total_globals,
                "total": len(self.spec),
            },
            "headers": {
                "declared_functions": sum(
                    1 for symbol in self.declared.values() if symbol.kind == "function"
                ),
                "declared_globals": sum(
                    1 for symbol in self.declared.values() if symbol.kind == "global"
                ),
                "total": len(self.declared),
            },
            "library": {
                "path": self.library_path,
                "exported_functions": sum(
                    1
                    for name in self.exported
                    if name in self.declared and self.declared[name].kind == "function"
                ),
                "exported_globals": sum(
                    1
                    for name in self.exported
                    if name in self.declared and self.declared[name].kind == "global"
                ),
                "abi_exports": len(
                    [
                        name
                        for name in self.exported
                        if name in self.declared or name in self.spec
                    ]
                ),
            },
            "implemented": {
                "functions": self.implemented_functions,
                "globals": self.implemented_globals,
                "total": len(self.implemented),
                "coverage": {
                    "functions": percent(self.implemented_functions, self.total_functions),
                    "globals": percent(self.implemented_globals, self.total_globals),
                    "total": self.coverage,
                },
            },
            "missing": {
                "functions": self.missing_functions,
                "globals": self.missing_globals,
                "total": len(self.missing),
            },
            "areas": [
                {
                    "area": area.name,
                    "implemented_functions": area.implemented_functions,
                    "total_functions": area.total_functions,
                    "implemented_globals": area.implemented_globals,
                    "total_globals": area.total_globals,
                    "implemented": area.implemented,
                    "total": area.total,
                    "missing": area.missing,
                    "coverage": area.coverage,
                }
                for area in self.areas.values()
            ],
            "top_missing_groups": [
                {"area": area.name, "missing": area.missing}
                for area in self.top_missing_groups()
            ],
            "missing_symbols": grouped,
            "project_extensions": sorted(PROJECT_EXTENSIONS),
            "diagnostics": self.diagnostics,
        }


def percent(part: int, total: int) -> float:
    if total == 0:
        return 0.0
    return round(100.0 * part / total, 1)


# --- Rendering ------------------------------------------------------------


def format_count(value: int) -> str:
    return f"{value:,}"


def human_report(coverage: Coverage) -> str:
    lines: list[str] = []
    lines.append("GNU readline ABI coverage")
    lines.append(f"specification: {coverage.spec_source}")
    lines.append(
        "               "
        f"{format_count(len(coverage.spec))} symbols "
        f"({coverage.total_functions} functions, {coverage.total_globals} globals)"
    )
    lines.append(
        "headers:       "
        f"{len(coverage.declared)} declared "
        f"({sum(1 for s in coverage.declared.values() if s.kind == 'function')} functions, "
        f"{sum(1 for s in coverage.declared.values() if s.kind == 'global')} globals)"
    )
    lines.append(
        "library:       "
        f"{coverage.library_path} "
        f"({len([n for n in coverage.exported if n in coverage.declared])} ABI exports)"
    )
    lines.append("")
    lines.append(
        "implemented:   "
        f"{len(coverage.implemented)}/{len(coverage.spec)} "
        f"({coverage.coverage:.1f}%)  "
        f"[{coverage.implemented_functions}/{coverage.total_functions} functions "
        f"({percent(coverage.implemented_functions, coverage.total_functions):.1f}%), "
        f"{coverage.implemented_globals}/{coverage.total_globals} globals "
        f"({percent(coverage.implemented_globals, coverage.total_globals):.1f}%)]"
    )
    lines.append(
        "missing:       "
        f"{len(coverage.missing)} "
        f"({coverage.missing_functions} functions, {coverage.missing_globals} globals)"
    )
    lines.append("")
    lines.append("by area:")
    lines.append(
        "  "
        f"{'area':<12} {'functions':>12} {'globals':>12} {'total':>12} "
        f"{'coverage':>9} {'missing':>8}"
    )
    for area in coverage.areas.values():
        lines.append(
            "  "
            f"{area.name:<12} "
            f"{area.implemented_functions:>5}/{area.total_functions:<6} "
            f"{area.implemented_globals:>5}/{area.total_globals:<6} "
            f"{area.implemented:>5}/{area.total:<6} "
            f"{area.coverage:>8.1f}% {area.missing:>8}"
        )
    lines.append("")
    lines.append("top missing groups:")
    for index, area in enumerate(coverage.top_missing_groups(), start=1):
        lines.append(f"  {index}. {area.name:<12} {area.missing:>4} missing  ({area.implemented}/{area.total})")
    lines.append("")
    lines.append("missing symbols by area:")
    for area, symbols in coverage.grouped_missing().items():
        if not symbols:
            continue
        lines.append(f"  {area} ({len(symbols)}):")
        names = [symbol.name for symbol in symbols]
        for offset in range(0, len(names), 6):
            lines.append("    " + " ".join(names[offset : offset + 6]))
    lines.append("")
    lines.append("project extensions not in the GNU headers:")
    for name in sorted(PROJECT_EXTENSIONS):
        lines.append(f"  {name}")
    if any(coverage.diagnostics.values()):
        lines.append("")
        lines.append("diagnostics:")
        for key, values in coverage.diagnostics.items():
            if values:
                lines.append(f"  {key}: {', '.join(values)}")
    return "\n".join(lines)


def snapshot_markdown(coverage: Coverage) -> str:
    grouped = coverage.grouped_missing()
    lines: list[str] = []
    lines.append("# GNU readline ABI coverage")
    lines.append("")
    lines.append("<!-- Generated by readline-abi/scripts/abi-coverage.py --snapshot. -->")
    lines.append("<!-- Do not edit by hand; rerun the script after adding or removing ABI symbols. -->")
    lines.append("")
    lines.append(
        "Specification: GNU readline 8.3 public headers "
        f"(`{'`, `'.join(VENDOR_HEADERS)}`)"
    )
    lines.append("")
    lines.append(
        "Implemented: symbols declared by our clean-room headers **and** exported "
        "by the built `libreadline`."
    )
    lines.append("")
    lines.append("Regenerate with:")
    lines.append("")
    lines.append("```sh")
    lines.append("readline-abi/scripts/abi-coverage.py --snapshot")
    lines.append("```")
    lines.append("")
    lines.append("## Summary")
    lines.append("")
    lines.append("| Surface | Implemented | Total | Coverage | Missing |")
    lines.append("| --- | ---: | ---: | ---: | ---: |")
    lines.append(
        f"| Functions | {coverage.implemented_functions} | {coverage.total_functions} | "
        f"{percent(coverage.implemented_functions, coverage.total_functions):.1f}% | "
        f"{coverage.missing_functions} |"
    )
    lines.append(
        f"| Globals | {coverage.implemented_globals} | {coverage.total_globals} | "
        f"{percent(coverage.implemented_globals, coverage.total_globals):.1f}% | "
        f"{coverage.missing_globals} |"
    )
    lines.append(
        f"| **All** | **{len(coverage.implemented)}** | **{len(coverage.spec)}** | "
        f"**{coverage.coverage:.1f}%** | **{len(coverage.missing)}** |"
    )
    lines.append("")
    lines.append("## By area")
    lines.append("")
    lines.append("| Area | Functions | Globals | Implemented | Total | Coverage | Missing |")
    lines.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: |")
    for area in coverage.areas.values():
        lines.append(
            f"| {area.name} | {area.implemented_functions}/{area.total_functions} | "
            f"{area.implemented_globals}/{area.total_globals} | {area.implemented} | "
            f"{area.total} | {area.coverage:.1f}% | {area.missing} |"
        )
    lines.append("")
    lines.append("## Top missing groups")
    lines.append("")
    for index, area in enumerate(coverage.top_missing_groups(), start=1):
        lines.append(f"{index}. **{area.name}** — {area.missing} missing ({area.implemented}/{area.total} implemented)")
    lines.append("")
    lines.append("## Missing symbols")
    for area in AREA_ORDER:
        symbols = grouped.get(area, [])
        if not symbols:
            continue
        lines.append("")
        lines.append(f"### {area} ({len(symbols)})")
        lines.append("")
        names = [f"`{symbol.name}`" for symbol in symbols]
        for offset in range(0, len(names), 4):
            lines.append(" ".join(names[offset : offset + 4]))
    lines.append("")
    lines.append("## Project extensions not in the GNU headers")
    lines.append("")
    for name in sorted(PROJECT_EXTENSIONS):
        lines.append(f"- `{name}`: compatibility spelling for GNU's internal `_rl_echoing_p`.")
    lines.append("")
    lines.append("## Audit notes")
    lines.append("")
    lines.append(
        f"- Library: `{coverage.library_path}` ({len([n for n in coverage.exported if n in coverage.declared])} ABI exports)."
    )
    lines.append(
        f"- Headers: {len(coverage.declared)} declarations; "
        f"exported-but-undeclared: {len(coverage.diagnostics['exported_not_declared'])}; "
        f"declared-but-not-exported: {len(coverage.diagnostics['declared_not_exported'])}."
    )
    lines.append(
        "- The ten legacy compatibility aliases wrapped in `#if 0` in the GNU "
        "header are included in the denominator: GNU's `compat.c` still exports "
        "them in `libreadline.so.8`."
    )
    if any(coverage.diagnostics.values()):
        lines.append("")
        lines.append("Drift detected:")
        for key, values in coverage.diagnostics.items():
            if values:
                lines.append(f"- `{key}`: {', '.join(f'`{value}`' for value in values)}")
    lines.append("")
    return "\n".join(lines)


# --- CLI ------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--library", help="path to the built libreadline shared object")
    parser.add_argument("--vendor", help="path to the vendored GNU readline headers")
    parser.add_argument("--json", action="store_true", help="print the report as JSON")
    parser.add_argument(
        "--snapshot",
        nargs="?",
        const=str(DEFAULT_SNAPSHOT),
        metavar="PATH",
        help=f"write the markdown snapshot (default: {DEFAULT_SNAPSHOT})",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="with --snapshot: fail if the checked-in snapshot differs",
    )
    parser.add_argument(
        "--build",
        action="store_true",
        help="run `cargo build -p readline-abi --release` when the library is missing",
    )
    parser.add_argument(
        "--require-full",
        action="store_true",
        help="fail unless every GNU-spec symbol is implemented",
    )
    parser.add_argument(
        "--min-coverage",
        type=float,
        metavar="PERCENT",
        help="fail when overall coverage is below PERCENT",
    )
    args = parser.parse_args()

    vendor = vendor_directory(args.vendor)
    if vendor is None:
        print(
            "error: vendored GNU readline headers not found; "
            "pass --vendor or set READLINE_ABI_VENDOR_HEADERS",
            file=sys.stderr,
        )
        return 1

    try:
        library = find_library(args.library)
    except FileNotFoundError as error:
        if not args.build:
            print(f"error: {error}", file=sys.stderr)
            return 1
        build_release()
        library = find_library(args.library)

    spec = parse_headers([vendor / name for name in VENDOR_HEADERS])
    declared = parse_headers(OUR_HEADERS)
    exported = exported_symbols(library)

    coverage = Coverage(
        spec_source=f"vendored GNU readline 8.3 headers ({vendor})",
        spec=spec,
        declared=declared,
        exported=exported,
    )
    coverage.library_path = str(library.relative_to(REPO)) if library.is_relative_to(REPO) else str(library)
    coverage.compute()

    if args.json:
        print(json.dumps(coverage.to_json(), indent=2, sort_keys=False))
    else:
        print(human_report(coverage))

    if args.snapshot:
        path = Path(args.snapshot)
        rendered = snapshot_markdown(coverage)
        if args.check:
            if not path.is_file():
                print(f"error: snapshot not found: {path}", file=sys.stderr)
                return 1
            current = path.read_text(encoding="utf-8")
            if current != rendered:
                print(f"error: snapshot is stale: {path}", file=sys.stderr)
                return 1
            print(f"snapshot is up to date: {path}", file=sys.stderr)
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(rendered, encoding="utf-8")
            print(f"wrote {path}", file=sys.stderr)

    if args.require_full and coverage.missing:
        print(
            f"error: ABI is not complete: {len(coverage.missing)} GNU-spec symbols missing",
            file=sys.stderr,
        )
        return 1
    if args.min_coverage is not None:
        total = len(coverage.spec)
        implemented = total - len(coverage.missing)
        percent = (implemented * 100.0 / total) if total else 0.0
        if percent + 1e-9 < args.min_coverage:
            print(
                f"error: ABI coverage {percent:.1f}% is below the {args.min_coverage:.1f}% gate",
                file=sys.stderr,
            )
            return 1

    has_drift = any(coverage.diagnostics.values())
    return 1 if has_drift else 0


if __name__ == "__main__":
    sys.exit(main())
