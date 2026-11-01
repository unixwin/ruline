# Provenance and independence

ruline is an independent project. It is **not a fork** and does not track an
upstream. The initial code base was taken from
[nushell/reedline](https://github.com/nushell/reedline) 0.52.1 under the MIT
license; the original copyright notice is retained in full in `LICENSE`.

Everything after that starting point is ruline's own code and policy:

- ruline targets GNU readline 8.x behaviour for niubash and for C readline
  consumers (the `rl_*` / `history_*` ABI, hooks, keymaps, funmap, inputrc).
  The vendored bash 5.3.20 tree in `unixwin/rubash`
  (`third_party/bash/lib/readline`) is the behavioural oracle.
- We do not merge nushell/reedline, do not follow its release cadence, and do
  not treat its API or feature set as a compatibility constraint. Fixes worth
  sharing may be offered upstream, but that is a courtesy, not a dependency.
- The crate keeps the package name `reedline` during the ABI transition so
  existing source and the niubash patch entry keep compiling. The product name
  is **ruline**; the crate rename is tracked separately.

Thank you to the Nushell Project and every reedline contributor: their
MIT-licensed work is the foundation of ruline. See
[`ACKNOWLEDGMENTS.md`](./ACKNOWLEDGMENTS.md) for the full attribution,
including the GNU readline team whose interface we target.

## Starting point

- Base: reedline 0.52.1 (MIT), fork point retained in the git history.
- Removed in the subtraction pass: `sqlite`/`sqlite-dynlib` and `rusqlite`
  (history is `FileBackedHistory` only), the nushell-specific `ide_menu` and
  `description_menu`, and the sqlite-only `cwd_aware_hinter` example.
- Added in the GNU parity work: the readline event/command surface
  (`KeySequence`, `QuotedInsert`, `Suspend`, `OverwriteMode`, `CharSearch`,
  `Yank*`, `OperateAndGetNext`, history search, non-incremental search, kbd
  macros, digit/universal arguments, `WithArg`, kill ring, marks/regions,
  revert-line, filename rubout, horizontal-space delete), multi-key sequence
  resolution, completion list/insert events, and the GNU keyseq parser.

## License

MIT. The original reedline copyright notice and the ruline contributors'
notice are both kept in `LICENSE`. GNU readline headers are **not** copied:
the C ABI work uses clean-room declarations; the GNU source is a behavioural
specification only.
