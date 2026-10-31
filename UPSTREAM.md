# Provenance

`ruline` is derived from [nushell/reedline](https://github.com/nushell/reedline)
at fork point (base: reedline 0.52.x). MIT License — the original
`LICENSE` (Copyright (c) 2021 Nushell Project) is retained in full.

## This is a hard fork

The product contract differs from upstream: ruline targets GNU
readline 8.x parity for niubash (see `unixwin/rubash`'s vendored
`third_party/bash/lib/readline` as the spec), not nushell's editor
surface. Changes are not intended for upstream merge; fixes worth
sharing may be ported back manually.

## Delta vs upstream (at fork)

Removed (subtraction):
- `sqlite`/`sqlite-dynlib` features + `rusqlite` (bundled C dep) —
  `FileBackedHistory` covers the host's needs
- `ide_menu`, `description_menu` — nushell-specific completion UIs
- `cwd_aware_hinter` example (sqlite-only)

Added (GNU parity, caomengxuan666/reedline history):
`ReedlineEvent::{KeySequence, QuotedInsert, Suspend, OverwriteMode,
CharSearch, YankLastArg, YankNthArg, OperateAndGetNext, HistorySearch,
NonIncrementalSearch(+Again), StartKbdMacro, EndKbdMacro,
CallLastKbdMacro, PrintLastKbdMacro, DigitArgument, UniversalArgument,
WithArg}`; `EditCommand::{YankPop, RevertLine, CutFilenameLeft,
DeleteHorizontalSpace, SetMark, ExchangePointAndMark, KillRegion,
CopyRegionAsKill}`; kill ring; kbd-macro recording; numeric args;
marks/regions; quoted-insert; suspend; non-incremental search;
quote-aware history tokenization.
