# GNU readline compatibility — definition of done

The target is GNU readline 8.3 as vendored in `unixwin/rubash`
(`third_party/bash/lib/readline`). "Compatible" is not a claim; it is the
following gates, all green on the same commit.

## Hard gates

1. **ABI coverage 100%**
   ```sh
   readline-abi/scripts/abi-coverage.py --require-full
   ```
   Every function and global in the GNU headers is declared by our clean-room
   headers **and** exported by the built library. No missing symbols, no
   declared/exported drift, no undocumented stubs.

2. **GNU differential conformance zero diff**
   ```sh
   readline-abi/tests/conformance/run.sh
   ```
   The same C probe is linked against system GNU readline 8.3 and our
   `libreadline`; every deterministic `SECTION|key|value` line must match.
   The PTY `readline()` driver is the last-mile extension of this gate.

3. **Plugin matrix has no unexpected SKIP**
   ```sh
   scripts/readline-plugin-probes/run.sh
   ```
   `enable -f` loadable builtins, fzf `bind -x`/`READLINE_*`, bash-completion,
   `$INPUTRC` startup and `C-x C-r` reload. A SKIP is allowed only for a
   platform/tool genuinely absent, with the reason recorded.

4. **Full test suite**
   - `cargo test` (ruline + `readline-abi`)
   - `cargo fmt --all -- --check`
   - `cargo clippy --all-targets --all -- -D warnings`
   - `readline-abi/scripts/smoke.sh`
   - the C symbol audit
   - the workspace tests of niubash that consume the editor

5. **Performance gate**
   `readline-abi/scripts/bench.sh` must stay within the recorded baseline in
   `readline-abi/BENCH.md` (or the change must explain the trade). Keymap
   lookup/bind, completion, line edits and history are measured.

6. **Behavioral parity**
   Where ruline and GNU disagree, GNU wins. The only allowed divergences are
   platform/technical (for example, no POSIX signal dispositions on Windows),
   each documented at the call site with a platform-equivalence test.

## Status snapshot

| Gate | Current |
| --- | --- |
| ABI coverage | **273 / 439 (62.2%)**, 166 missing — `--require-full` fails |
| Differential conformance | **239 oracle lines, zero diff** |
| Plugin matrix | **10 PASS / 1 FAIL / 0 SKIP** — `enable -f` works (rubash#2440); the remaining FAIL is the in-process full-surface export (niubash#775) |
| Full tests | green except the documented Linux/Android environment set in niubash |
| Performance | baseline recorded; no threshold regression yet |

The largest missing groups are text (80), history (20), display (16),
terminal (14), state (12), vi commands (47 inside text). They are tracked in
`readline-abi/abi/coverage.md` and split into implementation slices in
`readline-abi/abi/issue-8-comment.md`.

## How this is enforced

- `abi-coverage.py --require-full` is the release/merge gate; it is expected
  to fail until the missing groups are implemented, and that failure is the
  point.
- The differential conformance job runs in CI with `libreadline-dev`.
- Each implementation slice adds conformance lines and raises the coverage
  snapshot; the snapshot is checked in (`--snapshot --check`).
- The project does not describe itself as "100% compatible" until every hard
  gate above is green on the same commit.

## Attribution

GNU readline is the behavioural oracle. No GPL code or header text is copied:
the C declarations are clean-room, and the symbol inventory contains names
only. See `ACKNOWLEDGMENTS.md`.
