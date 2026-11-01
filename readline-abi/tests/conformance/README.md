# GNU readline differential conformance

This directory holds the oracle tests for the C ABI. The same `conformance.c`
is compiled twice:

- **GNU**: against the system GNU readline 8.3 (`pkg-config --libs readline`)
- **ruline**: against `readline-abi` (`-I include -L target/debug -lreadline`)

Both binaries run with a controlled `HOME`, `TMPDIR`, `TERM` and `INPUTRC`, and
print deterministic `SECTION|key|value` lines. `run.sh` diffs the two outputs.
Any difference is a compatibility bug; the line prefix names the area.

The interactive `readline_driver.c` is a separate PTY oracle: it calls
`readline("> ")` and prints the accepted line. `run_pty.py` feeds the same key
sequence to the GNU and ruline binaries and compares the accepted lines. That
suite is expected to be the last one to go green: it exercises the full editor
loop, not just the ABI functions.

## Run

```sh
readline-abi/tests/conformance/run.sh
```

Requires a system `libreadline-dev`/`libhistory` (GNU oracle), `cc`, `python3`
and a built `readline-abi` cdylib. The script builds the crate itself.

Exit codes: `0` all differential checks match; `1` at least one mismatch
(the diff is printed); `2` the harness could not build/run (missing oracle).
