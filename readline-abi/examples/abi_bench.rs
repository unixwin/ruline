//! ABI-level microbenchmarks for `libreadline`.
//!
//! The harness loads the built shared object with `dlopen`/`dlsym`, so the
//! numbers are for the symbols GNU applications actually call.  There is no
//! criterion dependency; `scripts/bench.sh` builds this example in release mode
//! and writes `BENCH.md` from the markdown table below.
//!
//! Usage:
//!
//! ```text
//! cargo run -p readline-abi --release --example abi_bench -- \
//!     --lib target/release/libreadline.so --samples 21
//! ```
//!
//! Each row reports the time per operation after a single untimed setup.
//! `ops/sample` is the number of operations inside each timed sample; it is
//! there to make it clear that e.g. "history add 10k" amortises one setup and
//! 10,000 `add_history` calls.

use std::ffi::{CStr, CString};
use std::fs;
use std::hint::black_box;
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

type CommandFn = unsafe extern "C" fn(c_int, c_int) -> c_int;
type CompEntryFn = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_char;
type Keymap = *mut c_void;

const HISTORY_1K: usize = 1_000;
const HISTORY_10K: usize = 10_000;
const COMPLETION_CANDIDATES: c_int = 1_000;
const FILENAME_ENTRIES: usize = 256;

// --- Dynamic loader -------------------------------------------------------

struct Api {
    handle: *mut c_void,
}

impl Api {
    unsafe fn open(path: &Path) -> Result<Self, String> {
        let canonical =
            fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let path = CString::new(canonical.to_string_lossy().as_bytes())
            .map_err(|_| "library path contains a NUL byte".to_string())?;
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            let message = libc::dlerror();
            let detail = if message.is_null() {
                "unknown dlopen error".to_string()
            } else {
                CStr::from_ptr(message).to_string_lossy().into_owned()
            };
            return Err(format!(
                "could not load {}: {detail}",
                path.to_string_lossy()
            ));
        }
        Ok(Self { handle })
    }

    unsafe fn symbol<T: Copy>(&self, name: &str) -> T {
        let name_c = CString::new(name).expect("symbol name");
        let pointer = libc::dlsym(self.handle, name_c.as_ptr());
        if pointer.is_null() {
            panic!("missing ABI symbol: {name}");
        }
        // Function pointers have the same representation as `*mut c_void` on
        // every platform this crate targets.
        std::mem::transmute_copy(&pointer)
    }

    unsafe fn data<T>(&self, name: &str) -> *mut T {
        let name_c = CString::new(name).expect("symbol name");
        let pointer = libc::dlsym(self.handle, name_c.as_ptr());
        if pointer.is_null() {
            panic!("missing ABI data symbol: {name}");
        }
        pointer.cast::<T>()
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.handle) };
    }
}

// --- Statistics -----------------------------------------------------------

struct Stats {
    median: f64,
    min: f64,
    max: f64,
    ops_per_sample: usize,
}

fn summarize(mut values: Vec<f64>, ops_per_sample: usize) -> Stats {
    values.sort_by(|left, right| left.total_cmp(right));
    let min = values.first().copied().unwrap_or(0.0);
    let max = values.last().copied().unwrap_or(0.0);
    let median = if values.len() % 2 == 1 {
        values[values.len() / 2]
    } else {
        let middle = values.len() / 2;
        (values[middle - 1] + values[middle]) / 2.0
    };
    Stats {
        median,
        min,
        max,
        ops_per_sample,
    }
}

/// Run `setup` once, then time `op`, which must perform `ops_per_sample`
/// operations.  The result is nanoseconds per operation.
fn bench(
    mut setup: impl FnMut(),
    mut op: impl FnMut(),
    samples: usize,
    ops_per_sample: usize,
) -> Stats {
    // Warm caches, allocator arenas, and lazily initialised keymaps.
    setup();
    op();

    let mut per_sample = Vec::with_capacity(samples);
    for _ in 0..samples {
        setup();
        let start = Instant::now();
        op();
        per_sample.push(start.elapsed().as_nanos() as f64 / ops_per_sample as f64);
    }
    summarize(per_sample, ops_per_sample)
}

/// Paired insert/delete sampling: each sample times an insert-only batch and
/// an insert+delete batch, then derives the delete cost from the same pair.
/// This keeps min/median/max coherent instead of subtracting independent runs.
fn bench_line_pair(
    mut insert_batch: impl FnMut(),
    mut combined_batch: impl FnMut(),
    samples: usize,
    ops_per_sample: usize,
) -> (Stats, Stats) {
    insert_batch();
    combined_batch();

    let mut insert_values = Vec::with_capacity(samples);
    let mut delete_values = Vec::with_capacity(samples);
    for sample in 0..samples {
        // Alternate the order so the second batch does not systematically get
        // a warmer cache and produce a negative derived delete cost.
        let (insert_ns, combined_ns) = if sample % 2 == 0 {
            let start = Instant::now();
            insert_batch();
            let insert_ns = start.elapsed().as_nanos() as f64 / ops_per_sample as f64;

            let start = Instant::now();
            combined_batch();
            let combined_ns = start.elapsed().as_nanos() as f64 / ops_per_sample as f64;
            (insert_ns, combined_ns)
        } else {
            let start = Instant::now();
            combined_batch();
            let combined_ns = start.elapsed().as_nanos() as f64 / ops_per_sample as f64;

            let start = Instant::now();
            insert_batch();
            let insert_ns = start.elapsed().as_nanos() as f64 / ops_per_sample as f64;
            (insert_ns, combined_ns)
        };

        insert_values.push(insert_ns);
        delete_values.push(combined_ns - insert_ns);
    }
    // A negative derived delete cost is physically impossible; it means the
    // paired timed batches were interrupted.  Keep the valid positive samples
    // so a single noisy pair cannot turn the reported minimum into 0 ns.
    let mut delete_values: Vec<f64> = delete_values
        .into_iter()
        .filter(|value| *value > 0.0)
        .collect();
    if delete_values.is_empty() {
        delete_values.push(0.0);
    }
    (
        summarize(insert_values, ops_per_sample),
        summarize(delete_values, ops_per_sample),
    )
}

fn format_ns(value: f64) -> String {
    if value < 1_000.0 {
        format!("{value:.0} ns")
    } else if value < 1_000_000.0 {
        format!("{:.2} µs", value / 1_000.0)
    } else {
        format!("{:.2} ms", value / 1_000_000.0)
    }
}

fn print_row(name: &str, stats: &Stats) {
    println!(
        "| {name} | {} | {} | {} | {} |",
        stats.ops_per_sample,
        format_ns(stats.median),
        format_ns(stats.min),
        format_ns(stats.max)
    );
}

// --- Completion generator -------------------------------------------------

unsafe extern "C" fn candidate_generator(_text: *const c_char, state: c_int) -> *mut c_char {
    if !(0..COMPLETION_CANDIDATES).contains(&state) {
        return ptr::null_mut();
    }
    // Allocate with libc: readline frees generator results with free(3).
    let name = format!("candidate-{state:04}");
    let allocation = libc::malloc(name.len() + 1) as *mut c_char;
    if allocation.is_null() {
        return ptr::null_mut();
    }
    ptr::copy_nonoverlapping(
        name.as_bytes().as_ptr(),
        allocation.cast::<u8>(),
        name.len(),
    );
    *allocation.add(name.len()) = 0;
    allocation
}

unsafe fn free_match_array(matches: *mut *mut c_char) {
    if matches.is_null() {
        return;
    }
    let mut index = 0usize;
    loop {
        let item = *matches.add(index);
        if item.is_null() {
            break;
        }
        libc::free(item as *mut c_void);
        index += 1;
    }
    libc::free(matches as *mut c_void);
}

// --- Fixtures -------------------------------------------------------------

fn temp_directory(override_path: Option<&Path>) -> PathBuf {
    let root = match override_path {
        Some(path) => path.to_path_buf(),
        None => {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            std::env::temp_dir().join(format!("readline-abi-bench-{}-{nanos}", std::process::id()))
        }
    };
    fs::create_dir_all(&root).expect("create benchmark temp directory");
    for index in 0..FILENAME_ENTRIES {
        let path = root.join(format!("file-{index:04}.txt"));
        fs::write(path, b"").expect("create benchmark completion file");
    }
    root
}

fn default_library() -> PathBuf {
    if let Some(path) = std::env::var_os("READLINE_ABI_LIB") {
        return PathBuf::from(path);
    }
    for candidate in [
        "target/release/libreadline.so",
        "../target/release/libreadline.so",
        "target/release/libreadline.dylib",
        "../target/release/libreadline.dylib",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("target/release/libreadline.so")
}

// --- Main -----------------------------------------------------------------

fn main() {
    let mut samples = std::env::var("BENCH_SAMPLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(21);
    let mut library = default_library();
    let mut tmp_override: Option<PathBuf> = None;

    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--samples" => {
                samples = arguments
                    .next()
                    .expect("--samples needs a value")
                    .parse()
                    .expect("--samples must be an integer");
            }
            "--lib" => {
                library = PathBuf::from(arguments.next().expect("--lib needs a value"));
            }
            "--tmp" => {
                tmp_override = Some(PathBuf::from(
                    arguments.next().expect("--tmp needs a value"),
                ));
            }
            "--help" | "-h" => {
                println!(
                    "usage: abi_bench [--lib PATH] [--samples N] [--tmp DIR]\n\
                     env: READLINE_ABI_LIB, BENCH_SAMPLES"
                );
                return;
            }
            other => panic!("unknown argument: {other}"),
        }
    }
    if samples == 0 {
        samples = 1;
    }

    let library = library;
    let api = unsafe { Api::open(&library).unwrap_or_else(|error| panic!("{error}")) };

    unsafe {
        let rl_initialize: unsafe extern "C" fn() -> c_int = api.symbol("rl_initialize");
        rl_initialize();

        let bench_command: CommandFn = api.symbol("rl_complete");
        let rl_make_keymap: unsafe extern "C" fn() -> Keymap = api.symbol("rl_make_keymap");
        let rl_free_keymap: unsafe extern "C" fn(Keymap) = api.symbol("rl_free_keymap");
        let rl_bind_key_in_map: unsafe extern "C" fn(c_int, Option<CommandFn>, Keymap) -> c_int =
            api.symbol("rl_bind_key_in_map");
        let rl_bind_keyseq_in_map: unsafe extern "C" fn(
            *const c_char,
            Option<CommandFn>,
            Keymap,
        ) -> c_int = api.symbol("rl_bind_keyseq_in_map");
        let rl_function_of_keyseq: unsafe extern "C" fn(
            *const c_char,
            Keymap,
            *mut c_int,
        ) -> *mut c_void = api.symbol("rl_function_of_keyseq");
        let rl_translate_keyseq: unsafe extern "C" fn(
            *const c_char,
            *mut c_char,
            *mut c_int,
        ) -> c_int = api.symbol("rl_translate_keyseq");
        let rl_completion_matches: unsafe extern "C" fn(
            *const c_char,
            Option<CompEntryFn>,
        ) -> *mut *mut c_char = api.symbol("rl_completion_matches");
        let rl_filename_completion_function: unsafe extern "C" fn(
            *const c_char,
            c_int,
        ) -> *mut c_char = api.symbol("rl_filename_completion_function");
        let rl_replace_line: unsafe extern "C" fn(*const c_char, c_int) =
            api.symbol("rl_replace_line");
        let rl_insert_text: unsafe extern "C" fn(*const c_char) -> c_int =
            api.symbol("rl_insert_text");
        let rl_delete_text: unsafe extern "C" fn(c_int, c_int) -> c_int =
            api.symbol("rl_delete_text");
        let add_history: unsafe extern "C" fn(*const c_char) = api.symbol("add_history");
        let clear_history: unsafe extern "C" fn() = api.symbol("clear_history");
        let history_get: unsafe extern "C" fn(c_int) -> *mut c_void = api.symbol("history_get");
        let history_base = api.data::<c_int>("history_base");

        println!("Loaded: {}", library.display());
        println!("Samples per benchmark: {samples}");
        println!();
        println!("| Benchmark | ops/sample | median | min | max |");
        println!("| --- | ---: | ---: | ---: | ---: |");

        // 1. inputrc key-sequence translation.
        let translated = CString::new("\\C-x\\M-\\e[1;5A").expect("translation input");
        let mut translated_buffer = [0u8; 256];
        let mut translated_length: c_int = 0;
        let status = rl_translate_keyseq(
            translated.as_ptr(),
            translated_buffer.as_mut_ptr().cast::<c_char>(),
            &mut translated_length,
        );
        assert_eq!(status, 0, "keyseq translation failed");
        assert!(
            translated_length > 0,
            "keyseq translation produced no bytes"
        );
        let stats = bench(
            || {},
            || {
                for _ in 0..10_000 {
                    let status = rl_translate_keyseq(
                        translated.as_ptr(),
                        translated_buffer.as_mut_ptr().cast::<c_char>(),
                        &mut translated_length,
                    );
                    black_box(status);
                }
            },
            samples,
            10_000,
        );
        print_row("keyseq translate (`\\C-x\\M-\\e[1;5A`)", &stats);

        // 2-3. Single-key bind and lookup in a fresh keymap.
        let keymap = rl_make_keymap();
        let single_key = b"a\0";
        let stats = bench(
            || {},
            || {
                for _ in 0..10_000 {
                    black_box(rl_bind_key_in_map(
                        b'a' as c_int,
                        Some(bench_command),
                        keymap,
                    ));
                }
            },
            samples,
            10_000,
        );
        print_row("keymap bind single key", &stats);
        assert!(
            !rl_function_of_keyseq(
                single_key.as_ptr().cast::<c_char>(),
                keymap,
                ptr::null_mut()
            )
            .is_null(),
            "single-key lookup did not find the bound command"
        );

        let stats = bench(
            || {},
            || {
                for _ in 0..100_000 {
                    black_box(rl_function_of_keyseq(
                        single_key.as_ptr().cast::<c_char>(),
                        keymap,
                        ptr::null_mut(),
                    ));
                }
            },
            samples,
            100_000,
        );
        print_row("keymap lookup single key", &stats);

        // 4-6. Multi-key binding and function-of-keyseq lookup.
        let multi_key = b"\x1b[1;5A\0";
        let stats = bench(
            || {},
            || {
                for _ in 0..10_000 {
                    black_box(rl_bind_keyseq_in_map(
                        multi_key.as_ptr().cast::<c_char>(),
                        Some(bench_command),
                        keymap,
                    ));
                }
            },
            samples,
            10_000,
        );
        print_row("keymap bind keyseq (`ESC [ 1 ; 5 A`)", &stats);
        assert!(
            !rl_function_of_keyseq(multi_key.as_ptr().cast::<c_char>(), keymap, ptr::null_mut())
                .is_null(),
            "multi-key lookup did not find the bound command"
        );

        let stats = bench(
            || {},
            || {
                for _ in 0..100_000 {
                    black_box(rl_function_of_keyseq(
                        multi_key.as_ptr().cast::<c_char>(),
                        keymap,
                        ptr::null_mut(),
                    ));
                }
            },
            samples,
            100_000,
        );
        print_row("function_of_keyseq hit (bound multi-key)", &stats);

        let unbound = b"\x1b[9;9Z\0";
        let stats = bench(
            || {},
            || {
                for _ in 0..10_000 {
                    black_box(rl_function_of_keyseq(
                        unbound.as_ptr().cast::<c_char>(),
                        keymap,
                        ptr::null_mut(),
                    ));
                }
            },
            samples,
            10_000,
        );
        print_row("function_of_keyseq miss", &stats);

        // 7. Completion generator with 1,000 candidates, including sorting,
        // LCD construction, and the free of the returned match array.
        let completion_text = b"cand\0";
        let stats = bench(
            || {},
            || {
                let matches = rl_completion_matches(
                    completion_text.as_ptr().cast::<c_char>(),
                    Some(candidate_generator),
                );
                assert!(!matches.is_null(), "completion generator returned no array");
                free_match_array(matches);
            },
            samples,
            1,
        );
        print_row("completion_matches, 1000 candidates", &stats);

        // 8. Filename completion over a temp directory with 256 entries.
        let temp = temp_directory(tmp_override.as_deref());
        let filename_prefix =
            CString::new(format!("{}/file", temp.to_string_lossy())).expect("filename prefix");
        let stats = bench(
            || {},
            || {
                let mut state = 0;
                loop {
                    let match_pointer =
                        rl_filename_completion_function(filename_prefix.as_ptr(), state);
                    if match_pointer.is_null() {
                        break;
                    }
                    libc::free(match_pointer as *mut c_void);
                    state += 1;
                    assert!(
                        state <= FILENAME_ENTRIES as c_int + 8,
                        "filename completion did not terminate"
                    );
                }
                assert_eq!(
                    state as usize, FILENAME_ENTRIES,
                    "filename completion did not return every fixture"
                );
            },
            samples,
            1,
        );
        print_row("filename completion, 256 entries", &stats);

        // 9-12. Line insert/delete at 1 KiB and 64 KiB.  Each sample batches
        // many operations so the clock granularity does not hide the work.
        // The delete benchmark deletes SIZE bytes from the front of a 2*SIZE
        // line, so it has a tail to move and is not dominated by the timer.
        // Its cost is derived by subtracting the insert-only batch from the
        // insert+delete batch, which keeps setup out of the result.
        const LINE_OPS: usize = 500;
        let empty = b"\0";
        for size in [1024usize, 64 * 1024] {
            let payload = CString::new(vec![b'x'; size]).expect("line payload");
            rl_replace_line(empty.as_ptr().cast::<c_char>(), 0);
            assert_eq!(
                rl_insert_text(payload.as_ptr()),
                size as c_int,
                "line insert failed"
            );
            assert_eq!(
                rl_delete_text(0, size as c_int),
                size as c_int,
                "line delete failed"
            );
            let stats = bench(
                || {},
                || {
                    for _ in 0..LINE_OPS {
                        rl_replace_line(empty.as_ptr().cast::<c_char>(), 0);
                        black_box(rl_insert_text(payload.as_ptr()));
                    }
                },
                samples,
                LINE_OPS,
            );
            print_row(&format!("line insert {}", human_bytes(size)), &stats);

            let double_payload = CString::new(vec![b'x'; size * 2]).expect("double line payload");
            rl_replace_line(empty.as_ptr().cast::<c_char>(), 0);
            assert_eq!(
                rl_insert_text(double_payload.as_ptr()),
                (size * 2) as c_int,
                "double line insert failed"
            );
            assert_eq!(
                rl_delete_text(0, size as c_int),
                size as c_int,
                "interior line delete failed"
            );
            let (_, delete) = bench_line_pair(
                || {
                    for _ in 0..LINE_OPS {
                        rl_replace_line(empty.as_ptr().cast::<c_char>(), 0);
                        black_box(rl_insert_text(double_payload.as_ptr()));
                    }
                },
                || {
                    for _ in 0..LINE_OPS {
                        rl_replace_line(empty.as_ptr().cast::<c_char>(), 0);
                        rl_insert_text(double_payload.as_ptr());
                        black_box(rl_delete_text(0, size as c_int));
                    }
                },
                samples,
                LINE_OPS,
            );
            print_row(
                &format!("line delete {} (front of 2x)", human_bytes(size)),
                &delete,
            );
        }

        // 13-16. History add/get over 1k and 10k entries.  `clear_history` and
        // the fill for `history_get` live in the untimed setup.
        let history_line = CString::new("benchmark history entry").expect("history line");
        for count in [HISTORY_1K, HISTORY_10K] {
            let stats = bench(
                || {
                    clear_history();
                },
                || {
                    for _ in 0..count {
                        add_history(history_line.as_ptr());
                    }
                },
                samples,
                count,
            );
            print_row(
                &format!("history add {} entries", human_count(count)),
                &stats,
            );

            let stats = bench(
                || {
                    clear_history();
                    for _ in 0..count {
                        add_history(history_line.as_ptr());
                    }
                },
                || {
                    let base = ptr::addr_of!(*history_base).read();
                    assert!(
                        !history_get(base).is_null(),
                        "history_get missed the first entry"
                    );
                    for index in 0..count {
                        black_box(history_get(base + index as c_int));
                    }
                },
                samples,
                count,
            );
            print_row(
                &format!("history get {} entries", human_count(count)),
                &stats,
            );
        }

        // Leave the process in a clean state for repeated runs.
        clear_history();
        rl_free_keymap(keymap);
        let _ = fs::remove_dir_all(&temp);
    }
}

fn human_bytes(value: usize) -> String {
    if value >= 1024 && value.is_multiple_of(1024) {
        format!("{} KiB", value / 1024)
    } else {
        format!("{value} B")
    }
}

fn human_count(value: usize) -> String {
    if value >= 1000 && value.is_multiple_of(1000) {
        format!("{}k", value / 1000)
    } else {
        value.to_string()
    }
}
