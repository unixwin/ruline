#!/bin/sh
# Build the cdylib, run the C ABI smoke test, and audit exported symbols.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
repo=$(CDPATH= cd -- "$here/.." && pwd)
profile=${1:-debug}

case "$profile" in
    debug)
        cargo build --manifest-path "$repo/Cargo.toml" -p readline-abi
        ;;
    release)
        cargo build --manifest-path "$repo/Cargo.toml" -p readline-abi --release
        ;;
    *)
        echo "usage: $0 [debug|release]" >&2
        exit 2
        ;;
esac

target="$repo/target/$profile"
library="$target/libreadline.so"

if [ ! -f "$library" ]; then
    echo "error: expected shared library at $library" >&2
    exit 1
fi

# The ELF SONAME is libreadline.so.8; make the runtime name resolvable next to
# Cargo's unversioned output.
ln -sf libreadline.so "$target/libreadline.so.8"

cc -std=c11 -Wall -Wextra \
    -I"$here/include" \
    "$here/tests/readline_smoke.c" \
    -L"$target" -lreadline -Wl,-rpath,"$target" \
    -o "$target/readline-smoke"

LD_LIBRARY_PATH="$target${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$target/readline-smoke"
python3 "$here/scripts/symbol-audit.py" "$library"
