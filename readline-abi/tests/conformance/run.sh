#!/bin/sh
# Differential GNU readline 8.3 vs ruline readline-abi conformance run.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH= cd -- "$here/../../.." && pwd)
abi="$root/readline-abi"
target="${CARGO_TARGET_DIR:-$root/target}"
build="$target/debug"
cc=${CC:-cc}

tmp=$(mktemp -d "${TMPDIR:-/tmp}/ruline-conformance.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

mkdir -p "$tmp/home"
export HOME="$tmp/home"
export TMPDIR="$tmp"
export LC_ALL=C
export TERM=xterm
unset INPUTRC || true

if ! pkg-config --exists readline 2>/dev/null; then
    echo "conformance: SKIP (no system GNU readline pkg-config)" >&2
    exit 2
fi

gnu_cflags=$(pkg-config --cflags readline)
gnu_libs=$(pkg-config --libs readline)
# GNU keeps history in libhistory; link it explicitly.
case "$gnu_libs" in
    *-lhistory*) ;;
    *) gnu_libs="$gnu_libs -lhistory" ;;
esac

# Build the ruline cdylib. The runtime soname is libreadline.so.8; the
# loader needs that name next to the unversioned Cargo artifact.
(cd "$root" && cargo build -p readline-abi >/dev/null)
ln -sf libreadline.so "$build/libreadline.so.8"

echo "== build GNU oracle =="
$cc $gnu_cflags "$here/conformance.c" -o "$tmp/conformance-gnu" $gnu_libs
echo "== build ruline =="
$cc -I"$abi/include" "$here/conformance.c" -o "$tmp/conformance-ruline" \
    -L"$build" -lreadline -Wl,-rpath,"$build"
ln -sf libreadline.so "$tmp/libreadline.so.8"

echo "== run GNU oracle =="
"$tmp/conformance-gnu" >"$tmp/gnu.out" 2>"$tmp/gnu.err" || {
    status=$?
    echo "conformance: GNU oracle failed (exit $status)" >&2
    cat "$tmp/gnu.err" >&2
    exit 2
}
echo "== run ruline =="
LD_LIBRARY_PATH="$build" "$tmp/conformance-ruline" >"$tmp/ruline.out" 2>"$tmp/ruline.err" || {
    status=$?
    echo "conformance: ruline failed (exit $status)" >&2
    cat "$tmp/ruline.err" >&2
    exit 1
}

if diff -u "$tmp/gnu.out" "$tmp/ruline.out" >"$tmp/diff"; then
    echo "conformance: PASS ($(grep -c '^' "$tmp/gnu.out") oracle lines match GNU readline)"
else
    echo "conformance: FAIL"
    cat "$tmp/diff"
    echo
    echo "GNU stderr:" >&2
    cat "$tmp/gnu.err" >&2
    echo "ruline stderr:" >&2
    cat "$tmp/ruline.err" >&2
    exit 1
fi
