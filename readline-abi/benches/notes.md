## Complexity and allocation notes

The table above is a baseline, not a target. No optimisation changes were made
in this measurement slice.

| Operation | Cost | Allocation / locking notes |
| --- | --- | --- |
| `rl_translate_keyseq` | O(n) in the input sequence | Allocates one `Vec` with capacity `n + 1` per call. Takes the keymap mutex to read `convert-meta` / `force-meta-prefix`; each of those is a linear scan of the string-variable table. |
| `rl_bind_key_in_map` (ASCII key) | O(1) | Keymap mutex. No allocation on the `ISFUNC` path; replacing a macro frees its old string. Meta-prefixed keys and `ISKMAP` targets take the key-sequence path below. |
| `rl_bind_keyseq_in_map` | O(k) for k bytes | Allocates a translation `Vec`, walks/creates child keymaps, and takes the keymap mutex. First bind of a prefix allocates a 257-entry keymap; later binds reuse it. |
| `rl_function_of_keyseq` hit | O(k) | Keymap mutex, no allocation on the raw-hit path. The call still evaluates `meta_prefix()`, which linearly scans the variable table twice (~two string comparisons over the option list). |
| `rl_function_of_keyseq` miss | O(k) plus translation | The raw miss falls through to `translate_impl`, which allocates a `Vec`. This makes the miss path measurably slower than a hit even for a short unbound sequence. |
| `rl_completion_matches` | O(n log n) for n candidates | Calls the generator n + 1 times, copies every result into a `Vec<Vec<u8>>`, frees the generator strings, then builds a fresh `malloc` array, sorts it, and computes the lowest common denominator. That is two copies per candidate plus the sort/LCD pass. |
| `rl_filename_completion_function` | O(n) for n directory entries | State 0 does a full `read_dir` snapshot, filters by prefix, and takes the completion mutex. Every later state clones one match and `malloc`s it. Directory entries are not re-read, so the per-match cost dominates after state 0. |
| `rl_insert_text` | O(payload + tail) | Copies the payload and shifts the tail. The line buffer grows by doubling through `realloc`; after capacity is reached there is no per-call allocation. |
| `rl_delete_text` (whole line) | O(remaining tail) | Deleting the whole buffer has a zero-length tail, so it is O(1): one NUL write and the bookkeeping. The benchmark deletes the front half of a 2x line to include a real tail move; an interior delete is O(remaining). |
| `add_history` | O(1) amortised | One `Box<HIST_ENTRY>` plus two `libc::malloc`s (line and timestamp) per entry, a `Vec` push, and the history mutex. When stifled, dropping the oldest entry is `Vec::remove(0)`, which is O(n); a ring buffer or `VecDeque` would make that O(1). |
| `history_get` | O(1) | Bounds check and history mutex; returns a pointer into the vector. |

### Obvious hot spots

1. **`meta_prefix()` is on every key lookup and translation.** `rl_function_of_keyseq`
   costs more than an order of magnitude more than a plain key bind on this
   baseline even though both take the same mutex. The linear scan of the
   string-variable table is the difference. Caching the two derived booleans
   when variables are set would remove it.
2. **Misses allocate for translation.** `rl_function_of_keyseq` pays a `Vec`
   allocation on every unbound sequence. It only needs translation when the
   input actually contains inputrc escapes.
3. **Completion copies candidates twice.** The generator already returns a
   `malloc`ed string, which readline then copies into a `Vec` and later copies
   again into the result array. At 1,000 candidates the whole call is dominated
   by these copies plus the duplicate/LCD sorting pass.
4. **Filename completion clones every returned match.** The state-0 snapshot is
   already an owned `Vec<Vec<u8>>`; each returned match is then cloned and
   `malloc`ed. That is required by the C contract (the caller frees each match),
   but it is the main cost after `readdir`.
5. **History entries are three allocations.** One box plus two strings. Stifled
   history also shifts the whole vector on eviction.

These are observations from the current implementation. The measurement slice
deliberately makes no speculative optimisation changes.
