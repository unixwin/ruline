#!/usr/bin/env python3
"""Audit the readline-abi symbol surface.

The implemented surface is defined by our clean-room headers.  Every symbol
declared there must:

* exist in the vendored GNU 8.3 public headers (the ABI specification), and
* be exported with C linkage by the built libreadline.

Conversely, every exported function/variable in the public readline/history
surface must be declared by our headers, so a hidden stub cannot slip through.
Symbol *names* are facts about the ABI; no GNU header text is copied into this
repository.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
CRATE = HERE.parent
REPO = CRATE.parent
DEFAULT_INVENTORY = CRATE / "abi" / "readline-symbols.txt"
OUR_HEADERS = [CRATE / "include" / "readline" / "readline.h", CRATE / "include" / "readline" / "history.h"]

# `rl_echoing_p` is a compatibility spelling for GNU's internal `_rl_echoing_p`.
# GNU does not publish it in readline.h, but our ABI promise includes it.
VENDOR_EXCEPTIONS = {"rl_echoing_p"}

VENDOR_CANDIDATES = [
    CRATE.parents[2] / "projects" / "rubash" / "third_party" / "bash" / "lib" / "readline",
    REPO / "third_party" / "bash" / "lib" / "readline",
    Path("/data/data/com.termux/files/home/projects/rubash/third_party/bash/lib/readline"),
]


def strip_comments(text: str) -> str:
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


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


def is_symbol_name(name: str) -> bool:
    return (
        not name.startswith("__")
        and not name.endswith("_t")
        and name not in C_KEYWORDS
    )


def declared_symbols(text: str) -> set[str]:
    """Return function/variable names from `extern` declarations."""
    symbols: set[str] = set()
    for declaration in re.findall(r"\bextern\b[^;]*;", strip_comments(text), flags=re.S):
        for name in re.findall(r"\b([A-Za-z_]\w*)\s*(?=\(|\[|;)", declaration):
            if is_symbol_name(name):
                symbols.add(name)
    return symbols


def vendor_directory(override: str | None) -> Path | None:
    if override:
        candidate = Path(override)
        return candidate if candidate.is_dir() else None
    from_env = os.environ.get("READLINE_ABI_VENDOR_HEADERS")
    if from_env and Path(from_env).is_dir():
        return Path(from_env)
    for candidate in VENDOR_CANDIDATES:
        if candidate.is_dir():
            return candidate
    return None


def vendor_symbols(directory: Path) -> set[str]:
    names: set[str] = set()
    for filename in ("readline.h", "history.h"):
        path = directory / filename
        if path.is_file():
            names |= declared_symbols(path.read_text(encoding="utf-8", errors="replace"))
    return names


def load_inventory(path: Path) -> set[str]:
    names: set[str] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            names.add(line)
    return names


def generate_inventory(directory: Path, output: Path) -> int:
    names = sorted(vendor_symbols(directory))
    if not names:
        print(f"error: no symbols found under {directory}", file=sys.stderr)
        return 1
    output.parent.mkdir(parents=True, exist_ok=True)
    body = [
        "# Symbol inventory derived from the vendored GNU readline 8.3 public",
        "# headers (readline.h, history.h).  Names only; no header text is copied.",
        "# Regenerate with: scripts/symbol-audit.py --generate-inventory",
        "",
        *names,
        "",
    ]
    output.write_text("\n".join(body), encoding="utf-8")
    print(f"wrote {len(names)} symbols to {output}")
    return 0


def exported_symbols(library: Path) -> set[str]:
    commands = [
        ["nm", "-D", "--defined-only", str(library)],
        ["nm", "-gU", str(library)],
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
            names.add(fields[-1])
        return names
    raise RuntimeError(f"could not inspect {library}: {last_error}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("library", nargs="?", help="path to libreadline.so")
    parser.add_argument("--vendor", help="path to the vendored GNU readline headers")
    parser.add_argument("--inventory", default=str(DEFAULT_INVENTORY))
    parser.add_argument("--generate-inventory", action="store_true")
    args = parser.parse_args()

    vendor = vendor_directory(args.vendor)
    if args.generate_inventory:
        if vendor is None:
            print("error: vendored readline headers not found; pass --vendor", file=sys.stderr)
            return 1
        return generate_inventory(vendor, Path(args.inventory))

    if args.library is None:
        parser.error("library path is required (or use --generate-inventory)")
    library = Path(args.library)
    if not library.is_file():
        print(f"error: library not found: {library}", file=sys.stderr)
        return 1

    implemented = set()
    for header in OUR_HEADERS:
        implemented |= declared_symbols(header.read_text(encoding="utf-8"))

    specification = vendor_symbols(vendor) if vendor is not None else load_inventory(Path(args.inventory))
    specification_source = str(vendor) if vendor is not None else str(Path(args.inventory))
    exported_all = exported_symbols(library)
    exported = {
        name
        for name in exported_all
        if name.startswith(("rl_", "history_"))
        or name in specification
        or name in implemented
    }

    missing_from_spec = sorted(implemented - specification - VENDOR_EXCEPTIONS)
    missing_from_library = sorted(implemented - exported)
    undeclared_exports = sorted(exported - implemented)

    print(f"specification: {specification_source} ({len(specification)} names)")
    print(f"headers:       {len(implemented)} declared symbols")
    print(f"library:       {len(exported)} exported ABI symbols")

    failed = False
    if missing_from_spec:
        failed = True
        print("error: declared symbols missing from the vendored ABI specification:", file=sys.stderr)
        for name in missing_from_spec:
            print(f"  {name}", file=sys.stderr)

    if missing_from_library:
        failed = True
        print("error: declared symbols missing from the library:", file=sys.stderr)
        for name in missing_from_library:
            print(f"  {name}", file=sys.stderr)

    if undeclared_exports:
        failed = True
        print("error: exported symbols not declared by our headers:", file=sys.stderr)
        for name in undeclared_exports:
            print(f"  {name}", file=sys.stderr)

    if failed:
        return 1

    print("symbol audit: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
