#!/bin/sh
# Stage an installable layout with the GNU ABI filenames.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
repo=$(CDPATH= cd -- "$here/.." && pwd)
profile=${1:-release}

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
out="$here/dist/$profile"

mkdir -p "$out/lib/pkgconfig" "$out/include/readline"
cp "$target/libreadline.so" "$out/lib/libreadline.so.8"
ln -sf libreadline.so.8 "$out/lib/libreadline.so"
cp "$target/libreadline.a" "$out/lib/libreadline.a"
cp "$here/include/readline/readline.h" "$out/include/readline/readline.h"
cp "$here/include/readline/history.h" "$out/include/readline/history.h"
cp "$here/readline.pc" "$out/lib/pkgconfig/readline.pc"

echo "staged ABI layout under $out"
echo "  lib/libreadline.so.8 (SONAME)"
echo "  lib/libreadline.a"
echo "  include/readline/*.h"
echo "  lib/pkgconfig/readline.pc"
