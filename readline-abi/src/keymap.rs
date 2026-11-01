//! Keymaps, funmaps, key bindings, variables, and inputrc parsing.
//!
//! This is the standalone ABI store: it owns the GNU-layout keymap arrays, the
//! name -> function funmap, macro bindings, readline variables, and the
//! `bind`/inputrc parser.  It deliberately does *not* route keys into the live
//! editor; that integration belongs to the niubash work item.  Every public
//! entry point below is a real, self-contained implementation.
//!
//! Default keymap slots and default command names are generated from the GNU
//! readline 8.3 oracle as name/number facts (`keymap_defaults.rs`).  Commands
//! that can edit the ABI line buffer standalone resolve to the real bodies in
//! `editing.rs`; the remainder stay inert until the live dispatcher is wired
//! in (niubash#775).

use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io::Write;
use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::{Mutex, MutexGuard, Once};

use crate::keymap_defaults::{
    default_command, BindTarget, DEFAULT_FUNMAP, EMACS_CTLX_KEYMAP_BINDINGS,
    EMACS_META_KEYMAP_BINDINGS, EMACS_STANDARD_KEYMAP_BINDINGS, TERMINAL_SEQUENCE_BINDINGS,
    VI_INSERTION_KEYMAP_BINDINGS, VI_MOVEMENT_KEYMAP_BINDINGS,
};
use crate::state::{self, RlCommandFunc};

// --- ABI constants and layout --------------------------------------------

/// One entry for every byte value plus the `ANYOTHERKEY` shadow slot.
pub const KEYMAP_SIZE: usize = 257;
/// Index of the shadowed binding used when a longer sequence does not match.
pub const ANYOTHERKEY: usize = KEYMAP_SIZE - 1;

/// The entry holds a function.
pub const ISFUNC: c_uchar = 0;
/// The entry holds a child keymap.
pub const ISKMAP: c_uchar = 1;
/// The entry holds a NUL-terminated macro string.
pub const ISMACR: c_uchar = 2;

const ESC: usize = 0x1b;
const RUBOUT: u8 = 0x7f;

/// GNU `KEYMAP_ENTRY`: a type byte followed by an 8-byte pointer union.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KeymapEntry {
    pub r#type: c_uchar,
    pub function: *mut c_void,
}

/// C `Keymap` is a pointer to an entry array.
pub type Keymap = *mut KeymapEntry;

const NULL_ENTRY: KeymapEntry = KeymapEntry {
    r#type: ISFUNC,
    function: ptr::null_mut(),
};

// --- Exported keymap arrays ----------------------------------------------

#[no_mangle]
pub static mut emacs_standard_keymap: [KeymapEntry; KEYMAP_SIZE] = [NULL_ENTRY; KEYMAP_SIZE];
#[no_mangle]
pub static mut emacs_meta_keymap: [KeymapEntry; KEYMAP_SIZE] = [NULL_ENTRY; KEYMAP_SIZE];
#[no_mangle]
pub static mut emacs_ctlx_keymap: [KeymapEntry; KEYMAP_SIZE] = [NULL_ENTRY; KEYMAP_SIZE];
#[no_mangle]
pub static mut vi_insertion_keymap: [KeymapEntry; KEYMAP_SIZE] = [NULL_ENTRY; KEYMAP_SIZE];
#[no_mangle]
pub static mut vi_movement_keymap: [KeymapEntry; KEYMAP_SIZE] = [NULL_ENTRY; KEYMAP_SIZE];

static EMACS_KEYMAP_NAME: [u8; 6] = *b"emacs\0";
static EMACS_META_KEYMAP_NAME: [u8; 11] = *b"emacs-meta\0";
static EMACS_CTLX_KEYMAP_NAME: [u8; 11] = *b"emacs-ctlx\0";
static VI_KEYMAP_NAME: [u8; 3] = *b"vi\0";
static VI_INSERT_KEYMAP_NAME: [u8; 10] = *b"vi-insert\0";
static EMACS_MODE_NAME: [u8; 6] = *b"emacs\0";
static VI_MODE_NAME: [u8; 3] = *b"vi\0";
static ON_VALUE: [u8; 3] = *b"on\0";
static OFF_VALUE: [u8; 4] = *b"off\0";

fn standard_keymap_ptr() -> Keymap {
    ptr::addr_of_mut!(emacs_standard_keymap).cast::<KeymapEntry>()
}

fn meta_keymap_ptr() -> Keymap {
    ptr::addr_of_mut!(emacs_meta_keymap).cast::<KeymapEntry>()
}

fn ctlx_keymap_ptr() -> Keymap {
    ptr::addr_of_mut!(emacs_ctlx_keymap).cast::<KeymapEntry>()
}

fn vi_insertion_keymap_ptr() -> Keymap {
    ptr::addr_of_mut!(vi_insertion_keymap).cast::<KeymapEntry>()
}

fn vi_movement_keymap_ptr() -> Keymap {
    ptr::addr_of_mut!(vi_movement_keymap).cast::<KeymapEntry>()
}

fn is_builtin_keymap(map: Keymap) -> bool {
    !map.is_null()
        && (map == standard_keymap_ptr()
            || map == meta_keymap_ptr()
            || map == ctlx_keymap_ptr()
            || map == vi_insertion_keymap_ptr()
            || map == vi_movement_keymap_ptr())
}

fn builtin_keymap_by_name(name: &str) -> Option<Keymap> {
    if name.eq_ignore_ascii_case("emacs") || name.eq_ignore_ascii_case("emacs-standard") {
        Some(standard_keymap_ptr())
    } else if name.eq_ignore_ascii_case("emacs-meta") {
        Some(meta_keymap_ptr())
    } else if name.eq_ignore_ascii_case("emacs-ctlx") {
        Some(ctlx_keymap_ptr())
    } else if name.eq_ignore_ascii_case("vi")
        || name.eq_ignore_ascii_case("vi-move")
        || name.eq_ignore_ascii_case("vi-command")
    {
        Some(vi_movement_keymap_ptr())
    } else if name.eq_ignore_ascii_case("vi-insert") {
        Some(vi_insertion_keymap_ptr())
    } else {
        None
    }
}

fn builtin_keymap_name(map: Keymap) -> Option<&'static str> {
    if map == standard_keymap_ptr() {
        Some("emacs")
    } else if map == meta_keymap_ptr() {
        Some("emacs-meta")
    } else if map == ctlx_keymap_ptr() {
        Some("emacs-ctlx")
    } else if map == vi_movement_keymap_ptr() {
        Some("vi")
    } else if map == vi_insertion_keymap_ptr() {
        Some("vi-insert")
    } else {
        None
    }
}

// --- Internal store -------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditMode {
    Emacs,
    Vi,
}

struct NamedKeymap {
    name: CString,
    map: Keymap,
}

struct FunmapEntry {
    name: CString,
    function: RlCommandFunc,
}

struct Store {
    current: Keymap,
    named: Vec<NamedKeymap>,
    funmap: Vec<FunmapEntry>,
    booleans: Vec<bool>,
    strings: Vec<Option<CString>>,
    last_value: Option<CString>,
    last_init_file: Option<PathBuf>,
    if_stack: Vec<bool>,
    conditionalized_out: bool,
    editing_mode: EditMode,
}

// SAFETY: the store is only reachable through `STORE`'s mutex.  The raw
// pointers are keymap arrays owned either by static storage or by libc.
unsafe impl Send for Store {}

static STORE: Mutex<Store> = Mutex::new(Store {
    current: ptr::null_mut(),
    named: Vec::new(),
    funmap: Vec::new(),
    booleans: Vec::new(),
    strings: Vec::new(),
    last_value: None,
    last_init_file: None,
    if_stack: Vec::new(),
    conditionalized_out: false,
    editing_mode: EditMode::Emacs,
});

const BOOLEAN_DEFAULTS: &[(&str, bool)] = &[
    ("bind-tty-special-chars", true),
    ("blink-matching-paren", false),
    ("byte-oriented", false),
    ("colored-completion-prefix", false),
    ("colored-stats", false),
    ("completion-ignore-case", false),
    ("completion-map-case", false),
    ("convert-meta", true),
    ("disable-completion", false),
    ("echo-control-characters", true),
    ("enable-active-region", true),
    ("enable-bracketed-paste", true),
    ("enable-keypad", false),
    ("enable-meta-key", true),
    ("expand-tilde", false),
    ("force-meta-prefix", false),
    ("history-preserve-point", false),
    ("horizontal-scroll-mode", false),
    ("input-meta", false),
    ("mark-directories", true),
    ("mark-modified-lines", false),
    ("mark-symlinked-directories", false),
    ("match-hidden-files", true),
    ("menu-complete-display-prefix", false),
    ("meta-flag", false),
    ("output-meta", false),
    ("page-completions", true),
    ("prefer-visible-bell", false),
    ("print-completions-horizontally", false),
    ("revert-all-at-newline", false),
    ("search-ignore-case", false),
    ("show-all-if-ambiguous", false),
    ("show-all-if-unmodified", false),
    ("show-mode-in-prompt", false),
    ("skip-completed-text", false),
    ("visible-stats", false),
];

const STRING_DEFAULTS: &[(&str, Option<&str>)] = &[
    ("active-region-end-color", None),
    ("active-region-start-color", None),
    ("bell-style", Some("audible")),
    ("comment-begin", Some("#")),
    ("completion-display-width", Some("-1")),
    ("completion-prefix-display-length", Some("0")),
    ("completion-query-items", Some("100")),
    ("editing-mode", Some("emacs")),
    ("emacs-mode-string", Some("@")),
    ("history-size", Some("-1")),
    ("isearch-terminators", None),
    ("keymap", Some("emacs")),
    ("keyseq-timeout", Some("500")),
    ("vi-cmd-mode-string", Some("(cmd)")),
    ("vi-ins-mode-string", Some("(ins)")),
];

static INIT: Once = Once::new();

fn store() -> MutexGuard<'static, Store> {
    STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn initialize() {
    ensure_initialized();
}

/// Whether the active editing mode is emacs.  Editing commands use this for
/// GNU's emacs-only mark updates.
pub(crate) fn emacs_mode() -> bool {
    ensure_initialized();
    matches!(store().editing_mode, EditMode::Emacs)
}

/// Switch the active editing mode to Emacs and select its standard keymap.
pub(crate) fn set_emacs_mode() {
    ensure_initialized();
    let mut state = store();
    state.editing_mode = EditMode::Emacs;
    state.current = standard_keymap_ptr();
    self::state::set_editing_mode(1);
}

fn ensure_initialized() {
    INIT.call_once(|| {
        fill_keymap(standard_keymap_ptr(), EMACS_STANDARD_KEYMAP_BINDINGS);
        fill_keymap(meta_keymap_ptr(), EMACS_META_KEYMAP_BINDINGS);
        fill_keymap(ctlx_keymap_ptr(), EMACS_CTLX_KEYMAP_BINDINGS);
        fill_keymap(vi_insertion_keymap_ptr(), VI_INSERTION_KEYMAP_BINDINGS);
        fill_keymap(vi_movement_keymap_ptr(), VI_MOVEMENT_KEYMAP_BINDINGS);

        let mut state = store();
        state.current = standard_keymap_ptr();
        state.editing_mode = EditMode::Emacs;
        crate::state::set_editing_mode(1);
        state.funmap = DEFAULT_FUNMAP
            .iter()
            .map(|(name, function)| FunmapEntry {
                name: CString::new(*name).unwrap_or_default(),
                function: *function,
            })
            .collect();
        state.booleans = BOOLEAN_DEFAULTS.iter().map(|(_, value)| *value).collect();
        state.strings = STRING_DEFAULTS
            .iter()
            .map(|(_, value)| value.and_then(|text| CString::new(text).ok()))
            .collect();

        // GNU binds the terminal's Home/End/arrow/Delete/PageUp/PageDown
        // sequences after the static maps (terminal.c). They are ESC/CSI
        // chains in the same keymap trie, not extra single-byte slots.
        for (sequence, name) in TERMINAL_SEQUENCE_BINDINGS {
            if let Some(function) = named_function_impl(&state, name.as_bytes()) {
                generic_bind_keys_impl(
                    &mut state,
                    ISFUNC as c_int,
                    sequence,
                    function_to_ptr(Some(function)) as *mut c_void,
                    standard_keymap_ptr(),
                );
            }
        }
    });
}

fn fill_keymap(map: Keymap, bindings: &[(usize, BindTarget)]) {
    for (slot, target) in bindings {
        if *slot >= KEYMAP_SIZE || map.is_null() {
            continue;
        }
        // SAFETY: `map` points at a KEYMAP_SIZE entry array and `slot` is in
        // range.  The default tables are filled exactly once by `INIT`.
        unsafe {
            let entry = map.add(*slot);
            match target {
                BindTarget::Cmd(name) => {
                    (*entry).r#type = ISFUNC;
                    (*entry).function = function_to_ptr(default_command(name));
                }
                BindTarget::Map(name) => {
                    let child = if *name == "emacs_meta_keymap" {
                        meta_keymap_ptr()
                    } else if *name == "emacs_ctlx_keymap" {
                        ctlx_keymap_ptr()
                    } else {
                        ptr::null_mut()
                    };
                    (*entry).r#type = ISKMAP;
                    (*entry).function = child as *mut c_void;
                }
            }
        }
    }
}

// --- Small helpers --------------------------------------------------------

fn function_to_ptr(function: RlCommandFunc) -> *mut c_void {
    match function {
        Some(function) => function as *const () as *mut c_void,
        None => ptr::null_mut(),
    }
}

fn ptr_to_function(pointer: *mut c_void) -> RlCommandFunc {
    if pointer.is_null() {
        None
    } else {
        // SAFETY: callers only store function pointers with `ISFUNC` or
        // shadowed entries; the inverse cast restores the same signature.
        Some(unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(c_int, c_int) -> c_int>(pointer)
        })
    }
}

fn c_bytes<'a>(value: *const c_char) -> &'a [u8] {
    if value.is_null() {
        &[]
    } else {
        // SAFETY: ABI callers pass NUL-terminated strings valid for the call.
        unsafe { CStr::from_ptr(value).to_bytes() }
    }
}

fn c_string(value: *const c_char) -> Option<String> {
    if value.is_null() {
        None
    } else {
        Some(String::from_utf8_lossy(c_bytes(value)).into_owned())
    }
}

fn duplicate_bytes(bytes: &[u8]) -> *mut c_char {
    let allocation = unsafe { libc::malloc(bytes.len() + 1) as *mut c_char };
    if allocation.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), allocation.cast::<u8>(), bytes.len());
        *allocation.add(bytes.len()) = 0;
    }
    allocation
}

fn write_stdout(bytes: &[u8]) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(bytes);
    let _ = lock.flush();
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | 0x0c)
}

fn trim_start_whitespace(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && is_whitespace(bytes[start]) {
        start += 1;
    }
    &bytes[start..]
}

fn trim_end_whitespace(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && is_whitespace(bytes[end - 1]) {
        end -= 1;
    }
    &bytes[..end]
}

fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle.iter())
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn keymap_name_impl(state: &Store, map: Keymap) -> Option<&str> {
    if let Some(name) = builtin_keymap_name(map) {
        return Some(name);
    }
    state
        .named
        .iter()
        .find(|entry| entry.map == map)
        .and_then(|entry| entry.name.to_str().ok())
}

fn get_keymap_by_name_impl(state: &Store, name: &str) -> Keymap {
    if let Some(map) = builtin_keymap_by_name(name) {
        return map;
    }
    state
        .named
        .iter()
        .find(|entry| {
            entry
                .name
                .to_str()
                .is_ok_and(|candidate| candidate.eq_ignore_ascii_case(name))
        })
        .map(|entry| entry.map)
        .unwrap_or(ptr::null_mut())
}

fn named_function_impl(state: &Store, name: &[u8]) -> RlCommandFunc {
    let name = String::from_utf8_lossy(name);
    state
        .funmap
        .iter()
        .find(|entry| {
            entry
                .name
                .to_str()
                .is_ok_and(|candidate| candidate.eq_ignore_ascii_case(&name))
        })
        .and_then(|entry| entry.function)
}

fn bool_indices(name: &str) -> Vec<usize> {
    let meta_alias =
        name.eq_ignore_ascii_case("meta-flag") || name.eq_ignore_ascii_case("input-meta");
    BOOLEAN_DEFAULTS
        .iter()
        .enumerate()
        .filter(|(_, (candidate, _))| {
            candidate.eq_ignore_ascii_case(name)
                || (meta_alias
                    && (candidate.eq_ignore_ascii_case("meta-flag")
                        || candidate.eq_ignore_ascii_case("input-meta")))
        })
        .map(|(index, _)| index)
        .collect()
}

fn string_index(name: &str) -> Option<usize> {
    STRING_DEFAULTS
        .iter()
        .position(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
}

fn bool_value(state: &Store, name: &str) -> Option<bool> {
    bool_indices(name)
        .first()
        .and_then(|index| state.booleans.get(*index).copied())
}

fn set_bool_value(state: &mut Store, name: &str, value: bool) {
    for index in bool_indices(name) {
        if let Some(slot) = state.booleans.get_mut(index) {
            *slot = value;
        }
    }
}

fn mode_name(mode: EditMode) -> &'static str {
    match mode {
        EditMode::Emacs => "emacs",
        EditMode::Vi => "vi",
    }
}

fn meta_prefix(state: &Store) -> bool {
    bool_value(state, "force-meta-prefix").unwrap_or(false)
        || bool_value(state, "convert-meta").unwrap_or(true)
}

// --- Key sequence translation --------------------------------------------

fn caret_value(byte: u8) -> u8 {
    match byte {
        b'?' => RUBOUT,
        b'@' => 0,
        b'^' => 0x1e,
        b'_' => 0x1f,
        other => other.to_ascii_uppercase() & 0x1f,
    }
}

/// Translate GNU `bind`/inputrc spelling into the bytes a keymap stores.
///
/// Supported forms include `\C-x`, `\M-x`, `\e`, `\n`, `\t`, `\xHH`,
/// `\ooo`, `^?`, backslash quoting, and a surrounding single or double quote.
fn translate_impl(sequence: &[u8], meta_prefix: bool, binding_syntax: bool) -> Result<Vec<u8>, ()> {
    let mut input = sequence;
    // GNU rl_translate_keyseq itself does NOT strip quotes or translate
    // caret notation; the inputrc/bind parser does that before it calls
    // here.  Keep `binding_syntax` for the parser's older convenience
    // path, but the exported rl_translate_keyseq uses the GNU form.
    if binding_syntax && input.len() >= 2 && (input[0] == b'"' || input[0] == b'\'') {
        if input[input.len() - 1] != input[0] {
            return Err(());
        }
        input = &input[1..input.len() - 1];
    }

    let mut output = Vec::with_capacity(input.len() + 1);
    let mut index = 0usize;
    let mut has_control = false;
    let mut has_meta = false;

    loop {
        if index >= input.len() && !has_control && !has_meta {
            break;
        }

        let mut byte = if index < input.len() { input[index] } else { 0 };
        let mut advance = true;

        if index < input.len() {
            if byte == b'\\' && index + 1 < input.len() {
                index += 1;
                byte = input[index];
                if byte == b'C' && input.get(index + 1) == Some(&b'-') {
                    index += 2;
                    has_control = true;
                    continue;
                }
                if byte == b'M' && input.get(index + 1) == Some(&b'-') {
                    index += 2;
                    has_meta = true;
                    continue;
                }

                match byte {
                    b'a' => byte = 0x07,
                    b'b' => byte = 0x08,
                    b'd' => byte = RUBOUT,
                    b'e' => byte = 0x1b,
                    b'f' => byte = 0x0c,
                    b'n' => byte = b'\n',
                    b'r' => byte = b'\r',
                    b't' => byte = b'\t',
                    b'v' => byte = 0x0b,
                    b'\\' => byte = b'\\',
                    b'0'..=b'7' => {
                        let mut value = byte - b'0';
                        index += 1;
                        let mut consumed = 0;
                        while consumed < 2 && index < input.len() {
                            let digit = input[index];
                            if !(b'0'..=b'7').contains(&digit) {
                                break;
                            }
                            value = value.wrapping_mul(8).wrapping_add(digit - b'0');
                            index += 1;
                            consumed += 1;
                        }
                        byte = value;
                        advance = false;
                    }
                    b'x' => {
                        index += 1;
                        let mut value = 0u8;
                        let mut consumed = 0;
                        while consumed < 2 && index < input.len() {
                            let digit = input[index];
                            let nibble = match digit {
                                b'0'..=b'9' => digit - b'0',
                                b'a'..=b'f' => digit - b'a' + 10,
                                b'A'..=b'F' => digit - b'A' + 10,
                                _ => break,
                            };
                            value = value.wrapping_mul(16).wrapping_add(nibble);
                            index += 1;
                            consumed += 1;
                        }
                        byte = if consumed == 0 { b'x' } else { value };
                        advance = false;
                    }
                    _ => {}
                }
            } else if binding_syntax && byte == b'^' && index + 1 < input.len() {
                index += 1;
                byte = caret_value(input[index]);
            }
        }

        if has_control {
            byte = if byte == b'?' {
                RUBOUT
            } else {
                byte.to_ascii_uppercase() & 0x1f
            };
            has_control = false;
        }

        if has_meta {
            byte |= 0x80;
            if meta_prefix {
                let unmasked = byte & 0x7f;
                if unmasked != 0 {
                    output.push(ESC as u8);
                    output.push(unmasked);
                } else {
                    output.push(byte);
                }
            } else {
                output.push(byte);
            }
            has_meta = false;
        } else {
            output.push(byte);
        }

        if advance && index < input.len() {
            index += 1;
        }
        if index >= input.len() && !has_control && !has_meta {
            break;
        }
    }

    Ok(output)
}

/// Return the GNU/inputrc spelling for a single keymap slot.
fn keyname_bytes(key: usize) -> Vec<u8> {
    if key == ANYOTHERKEY {
        return Vec::new();
    }
    if key == ESC {
        return b"\\e".to_vec();
    }

    let mut output = Vec::with_capacity(8);
    let mut byte = key as u8;

    if byte < 0x20 || byte == RUBOUT {
        output.extend_from_slice(b"\\C-");
        byte = if byte == RUBOUT {
            b'?'
        } else {
            (byte ^ 0x40).to_ascii_lowercase()
        };
    } else if (0x80..=0x9f).contains(&byte) {
        output.extend_from_slice(b"\\2");
        byte -= 0x80;
        output.push(b'0' + (byte / 8));
        byte = b'0' + (byte % 8);
    } else if byte >= 0xa0 {
        output.push(b'\\');
        output.push(b'0' + ((byte >> 6) & 0x07));
        output.push(b'0' + ((byte >> 3) & 0x07));
        byte = b'0' + (byte & 0x07);
    }

    if byte == b'\\' || byte == b'"' {
        output.push(b'\\');
    }
    output.push(byte);
    output
}

fn untranslate_keyseq_bytes(key: u8) -> Vec<u8> {
    let mut output = Vec::with_capacity(8);
    let mut byte = key;
    if byte & 0x80 != 0 {
        output.extend_from_slice(b"\\M-");
        byte &= 0x7f;
    }
    if byte == ESC as u8 {
        output.extend_from_slice(b"\\e");
    } else if byte < 0x20 || byte == RUBOUT {
        output.extend_from_slice(b"\\C-");
        byte = if byte == RUBOUT {
            b'?'
        } else {
            (byte ^ 0x40).to_ascii_lowercase()
        };
    }
    if byte == b'\\' || byte == b'"' {
        output.push(b'\\');
    }
    output.push(byte);
    output
}

fn untranslate_macro_bytes(sequence: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(sequence.len() * 2);
    for raw in sequence {
        let mut byte = *raw;
        if byte & 0x80 != 0 {
            output.extend_from_slice(b"\\M-");
            byte &= 0x7f;
        }

        if byte == ESC as u8 {
            output.extend_from_slice(b"\\e");
        } else if byte < 0x20 {
            output.extend_from_slice(b"\\C-");
            byte = (byte ^ 0x40).to_ascii_lowercase();
        } else if byte == RUBOUT {
            output.extend_from_slice(b"\\C-");
            byte = b'?';
        }

        if byte == b'\\' || byte == b'"' {
            output.push(b'\\');
        }
        output.push(byte);
    }
    output
}

// --- Keymap entry manipulation -------------------------------------------

fn free_macro_entry(entry: &mut KeymapEntry) {
    if entry.r#type == ISMACR && !entry.function.is_null() {
        unsafe { libc::free(entry.function) };
    }
    entry.r#type = ISFUNC;
    entry.function = ptr::null_mut();
}

fn make_bare_keymap() -> Keymap {
    let layout = std::mem::size_of::<KeymapEntry>();
    let map = unsafe { libc::calloc(KEYMAP_SIZE, layout) as Keymap };
    if map.is_null() {
        return ptr::null_mut();
    }
    map
}

fn empty_keymap(map: Keymap) -> bool {
    if map.is_null() {
        return true;
    }
    for index in 0..ANYOTHERKEY {
        // SAFETY: keymaps are KEYMAP_SIZE entry arrays.
        let entry = unsafe { &*map.add(index) };
        if entry.r#type != ISFUNC || !entry.function.is_null() {
            return false;
        }
    }
    true
}

fn discard_keymap(map: Keymap) {
    if map.is_null() || is_builtin_keymap(map) {
        return;
    }
    for index in 0..KEYMAP_SIZE {
        // SAFETY: keymaps are KEYMAP_SIZE entry arrays.
        let entry = unsafe { &mut *map.add(index) };
        match entry.r#type {
            ISKMAP => {
                if !entry.function.is_null() {
                    let child = entry.function.cast::<KeymapEntry>();
                    discard_keymap(child);
                    if !is_builtin_keymap(child) {
                        unsafe { libc::free(entry.function) };
                    }
                    entry.function = ptr::null_mut();
                    entry.r#type = ISFUNC;
                }
            }
            ISMACR => {
                if !entry.function.is_null() {
                    unsafe { libc::free(entry.function) };
                }
                entry.function = ptr::null_mut();
            }
            _ => {}
        }
    }
}

fn copy_keymap(map: Keymap) -> Keymap {
    if map.is_null() {
        return ptr::null_mut();
    }
    let copy = make_bare_keymap();
    if copy.is_null() {
        return copy;
    }
    unsafe { ptr::copy_nonoverlapping(map, copy, KEYMAP_SIZE) };
    copy
}

// --- Generic binding ------------------------------------------------------

fn bind_key_impl(state: &mut Store, map: Keymap, key: c_int, function: RlCommandFunc) -> c_int {
    if map.is_null() || !(0..=255).contains(&key) {
        return key;
    }
    let key = key as usize;

    if key & 0x80 != 0 && meta_prefix(state) {
        let esc_entry = unsafe { &*map.add(ESC) };
        if esc_entry.r#type == ISKMAP && !esc_entry.function.is_null() {
            let child = esc_entry.function.cast::<KeymapEntry>();
            let unmasked = key & 0x7f;
            unsafe {
                let entry = &mut *child.add(unmasked);
                free_macro_entry(entry);
                entry.r#type = ISFUNC;
                entry.function = function_to_ptr(function);
            }
            return 0;
        }
        let sequence = [ESC as u8, (key & 0x7f) as u8];
        return generic_bind_keys_impl(
            state,
            ISFUNC as c_int,
            &sequence,
            function_to_ptr(function),
            map,
        );
    }

    // SAFETY: key is in 0..=255 and map has KEYMAP_SIZE entries.
    let entry = unsafe { &mut *map.add(key) };
    if entry.r#type != ISKMAP {
        free_macro_entry(entry);
        entry.r#type = ISFUNC;
        entry.function = function_to_ptr(function);
        return 0;
    }

    let sequence = match key {
        0 => b"\\0".to_vec(),
        0x5c => b"\\\\".to_vec(),
        _ => vec![key as u8],
    };
    generic_bind_impl(
        state,
        ISFUNC as c_int,
        &sequence,
        function_to_ptr(function),
        map,
    )
}

fn generic_bind_impl(
    state: &mut Store,
    kind: c_int,
    keyseq: &[u8],
    data: *mut c_void,
    map: Keymap,
) -> c_int {
    if keyseq.is_empty() {
        if kind == ISMACR as c_int && !data.is_null() {
            unsafe { libc::free(data) };
        }
        return -1;
    }

    let keys = match translate_impl(keyseq, meta_prefix(state), false) {
        Ok(keys) => keys,
        Err(()) => {
            if kind == ISMACR as c_int && !data.is_null() {
                unsafe { libc::free(data) };
            }
            return -1;
        }
    };
    generic_bind_keys_impl(state, kind, &keys, data, map)
}

fn generic_bind_keys_impl(
    _state: &mut Store,
    kind: c_int,
    keys: &[u8],
    mut data: *mut c_void,
    map: Keymap,
) -> c_int {
    if keys.is_empty() || map.is_null() {
        return -1;
    }

    let mut current = map;
    let mut previous = map;
    let mut previous_key = keys[0] as usize;
    let mut bound_map = map;
    let lowercase = function_to_ptr(named_function_impl(_state, b"do-lowercase-version"));

    for (index, raw) in keys.iter().enumerate() {
        let key = *raw as usize;
        if key >= KEYMAP_SIZE {
            return -1;
        }
        if index > 0 {
            previous_key = *keys.get(index - 1).unwrap_or(&0) as usize;
        }

        if index + 1 < keys.len() {
            // SAFETY: current is a keymap and key is in range.
            let entry = unsafe { &mut *current.add(key) };
            let saved = *entry;
            if entry.r#type != ISKMAP {
                entry.r#type = ISKMAP;
                entry.function = make_bare_keymap() as *mut c_void;
            }
            previous = current;
            current = entry.function.cast::<KeymapEntry>();
            if current.is_null() {
                return -1;
            }
            if !saved.function.is_null()
                && ((saved.r#type == ISFUNC && saved.function != lowercase)
                    || saved.r#type == ISMACR)
            {
                unsafe {
                    let shadow = &mut *current.add(ANYOTHERKEY);
                    *shadow = saved;
                }
            }
        } else {
            // SAFETY: current is a keymap and key is in range.
            let entry = unsafe { &mut *current.add(key) };
            let mut target_key = key;
            if entry.r#type == ISKMAP {
                previous = current;
                current = entry.function.cast::<KeymapEntry>();
                if current.is_null() {
                    return -1;
                }
                target_key = ANYOTHERKEY;
                if kind == ISFUNC as c_int && data.is_null() {
                    data = function_to_ptr(Some(null_command));
                }
            }
            let target = unsafe { &mut *current.add(target_key) };
            free_macro_entry(target);
            target.r#type = kind as c_uchar;
            target.function = data;
        }
        bound_map = current;
    }

    if kind == ISFUNC as c_int && data.is_null() {
        // SAFETY: previous_key was maintained alongside the walk.
        let previous_entry = unsafe { &mut *previous.add(previous_key) };
        if previous_entry.r#type == ISKMAP
            && previous_entry.function.cast::<KeymapEntry>() == bound_map
        {
            let shadow = unsafe { *bound_map.add(ANYOTHERKEY) };
            if shadow.r#type == ISFUNC && empty_keymap(bound_map) {
                previous_entry.r#type = shadow.r#type;
                previous_entry.function = shadow.function;
                discard_keymap(bound_map);
                if !is_builtin_keymap(bound_map) {
                    unsafe { libc::free(bound_map as *mut c_void) };
                }
            }
        }
    }

    0
}

unsafe extern "C" fn null_command(_: c_int, _: c_int) -> c_int {
    0
}

// --- Key sequence lookup and invoking sequences --------------------------

fn function_of_raw(map: Keymap, keys: &[u8], kind: *mut c_int, meta_prefix: bool) -> *mut c_void {
    if map.is_null() {
        return ptr::null_mut();
    }
    let mut current = map;
    let mut index = 0usize;

    while index < keys.len() {
        let mut key = keys[index] as usize;
        if key & 0x80 != 0 && meta_prefix {
            // SAFETY: current is a keymap and ESC is in range.
            let escape = unsafe { &*current.add(ESC) };
            if escape.r#type == ISKMAP && !escape.function.is_null() {
                current = escape.function.cast::<KeymapEntry>();
                key &= 0x7f;
            } else {
                if !kind.is_null() {
                    unsafe { *kind = escape.r#type as c_int };
                }
                return escape.function;
            }
        }

        let entry = unsafe { &*current.add(key) };
        if entry.r#type == ISKMAP {
            if index + 1 == keys.len() {
                if !kind.is_null() {
                    unsafe { *kind = ISKMAP as c_int };
                }
                return entry.function;
            }
            if entry.function.is_null() {
                return ptr::null_mut();
            }
            current = entry.function.cast::<KeymapEntry>();
        } else if index + 1 < keys.len() {
            return ptr::null_mut();
        } else {
            if !kind.is_null() {
                unsafe { *kind = entry.r#type as c_int };
            }
            return entry.function;
        }
        index += 1;
    }
    ptr::null_mut()
}

fn function_of_keyseq(state: &Store, keys: &[u8], map: Keymap, kind: *mut c_int) -> RlCommandFunc {
    let map = if map.is_null() { state.current } else { map };
    let prefix = meta_prefix(state);

    // GNU rl_function_of_keyseq takes RAW key bytes; it does not translate
    // bind spellings. Callers with a spelling pass it through
    // rl_translate_keyseq first.
    ptr_to_function(function_of_raw(map, keys, kind, prefix))
}

fn collect_invoking(
    map: Keymap,
    function: *mut c_void,
    prefix: &[u8],
    output: &mut Vec<Vec<u8>>,
    depth: usize,
) {
    if map.is_null() || depth > 32 {
        return;
    }
    for key in 0..KEYMAP_SIZE {
        // SAFETY: map is a KEYMAP_SIZE entry array.
        let entry = unsafe { &*map.add(key) };
        match entry.r#type {
            ISFUNC | ISMACR => {
                if entry.function == function {
                    let mut sequence = prefix.to_vec();
                    sequence.extend(keyname_bytes(key));
                    output.push(sequence);
                }
            }
            ISKMAP if !entry.function.is_null() => {
                let mut next_prefix = prefix.to_vec();
                next_prefix.extend(keyname_bytes(key));
                collect_invoking(
                    entry.function.cast::<KeymapEntry>(),
                    function,
                    &next_prefix,
                    output,
                    depth + 1,
                );
            }
            _ => {}
        }
    }
}

fn invoking_keyseqs_impl(state: &Store, function: *mut c_void, map: Keymap) -> Vec<Vec<u8>> {
    let map = if map.is_null() { state.current } else { map };
    let mut output = Vec::new();
    collect_invoking(map, function, &[], &mut output, 0);
    output
}

// --- Variable lookup ------------------------------------------------------

fn string_value_impl(state: &Store, index: usize) -> Option<String> {
    let name = STRING_DEFAULTS.get(index)?.0;
    if name.eq_ignore_ascii_case("editing-mode") {
        return Some(mode_name(state.editing_mode).to_string());
    }
    if name.eq_ignore_ascii_case("keymap") {
        return keymap_name_impl(state, state.current)
            .map(str::to_owned)
            .or_else(|| Some(mode_name(state.editing_mode).to_string()));
    }
    state
        .strings
        .get(index)
        .and_then(|value| value.as_ref())
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn cache_value(state: &mut Store, value: String) -> *mut c_char {
    state.last_value = CString::new(value).ok();
    match &state.last_value {
        Some(value) => value.as_ptr() as *mut c_char,
        None => ptr::null_mut(),
    }
}

fn variable_value_impl(state: &mut Store, name: &str) -> *mut c_char {
    if let Some(value) = bool_value(state, name) {
        let bytes: &[u8] = if value { &ON_VALUE } else { &OFF_VALUE };
        return bytes.as_ptr() as *mut c_char;
    }

    let Some(index) = string_index(name) else {
        return ptr::null_mut();
    };
    let var_name = STRING_DEFAULTS[index].0;
    if var_name.eq_ignore_ascii_case("editing-mode") || var_name.eq_ignore_ascii_case("keymap") {
        return match string_value_impl(state, index) {
            Some(value) => cache_value(state, value),
            None => ptr::null_mut(),
        };
    }

    match state.strings.get(index).and_then(|value| value.as_ref()) {
        Some(value) => value.as_ptr() as *mut c_char,
        None => ptr::null_mut(),
    }
}

fn bool_from_value(value: Option<&str>) -> bool {
    match value {
        None | Some("") => true,
        Some(value) => {
            value.eq_ignore_ascii_case("on")
                || value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
        }
    }
}

fn variable_bind_impl(state: &mut Store, name: &str, value: Option<&str>) -> c_int {
    if !bool_indices(name).is_empty() {
        let enabled = bool_from_value(value);
        set_bool_value(state, name, enabled);
        return 0;
    }

    let Some(index) = string_index(name) else {
        return 0;
    };
    let var_name = STRING_DEFAULTS[index].0;
    let text = value.unwrap_or("");

    if var_name.eq_ignore_ascii_case("editing-mode") {
        if text.len() >= 2 && text[..2].eq_ignore_ascii_case("vi") {
            state.editing_mode = EditMode::Vi;
            state.current = vi_insertion_keymap_ptr();
            crate::state::set_editing_mode(0);
            return 0;
        }
        if text.len() >= 5 && text[..5].eq_ignore_ascii_case("emacs") {
            state.editing_mode = EditMode::Emacs;
            state.current = standard_keymap_ptr();
            crate::state::set_editing_mode(1);
            return 0;
        }
        return 1;
    }

    if var_name.eq_ignore_ascii_case("keymap") {
        let map = get_keymap_by_name_impl(state, text);
        if map.is_null() {
            return 1;
        }
        state.current = map;
        return 0;
    }

    if var_name.eq_ignore_ascii_case("bell-style") {
        if text.eq_ignore_ascii_case("none")
            || text.eq_ignore_ascii_case("visible")
            || text.eq_ignore_ascii_case("audible")
        {
            state.strings[index] = CString::new(text).ok();
            return 0;
        }
        return 1;
    }

    if var_name.eq_ignore_ascii_case("comment-begin") {
        if text.is_empty() {
            return 1;
        }
        state.strings[index] = CString::new(text).ok();
        return 0;
    }

    if matches!(
        var_name,
        "completion-display-width"
            | "completion-prefix-display-length"
            | "completion-query-items"
            | "history-size"
            | "keyseq-timeout"
    ) {
        let Ok(number) = text.parse::<i64>() else {
            return 1;
        };
        state.strings[index] = CString::new(number.to_string()).ok();
        return 0;
    }

    if matches!(
        var_name,
        "active-region-end-color"
            | "active-region-start-color"
            | "emacs-mode-string"
            | "isearch-terminators"
            | "vi-cmd-mode-string"
            | "vi-ins-mode-string"
    ) {
        state.strings[index] = CString::new(text).ok();
        return 0;
    }

    state.strings[index] = CString::new(text).ok();
    0
}

// --- inputrc parser -------------------------------------------------------

fn skip_to_delimiter(bytes: &[u8], start: usize, delimiter: u8) -> usize {
    let mut index = start;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if byte == delimiter {
            return index;
        }
        index += 1;
    }
    index
}

fn split_first_word(bytes: &[u8]) -> (&[u8], &[u8]) {
    let bytes = trim_start_whitespace(bytes);
    let mut end = 0;
    while end < bytes.len() && !is_whitespace(bytes[end]) {
        end += 1;
    }
    let rest = trim_start_whitespace(&bytes[end..]);
    (&bytes[..end], rest)
}

fn parse_comparison(rest: &[u8]) -> Option<(u8, &[u8])> {
    let rest = trim_start_whitespace(rest);
    if rest.starts_with(b">=") {
        Some((b'G', trim_start_whitespace(&rest[2..])))
    } else if rest.starts_with(b"<=") {
        Some((b'L', trim_start_whitespace(&rest[2..])))
    } else if rest.starts_with(b"==") {
        Some((b'E', trim_start_whitespace(&rest[2..])))
    } else if rest.starts_with(b"!=") {
        Some((b'N', trim_start_whitespace(&rest[2..])))
    } else if rest.starts_with(b">") {
        Some((b'>', trim_start_whitespace(&rest[1..])))
    } else if rest.starts_with(b"<") {
        Some((b'<', trim_start_whitespace(&rest[1..])))
    } else if rest.starts_with(b"=") {
        Some((b'E', trim_start_whitespace(&rest[1..])))
    } else {
        None
    }
}

fn compare_values(left: &str, operator: u8, right: &str) -> bool {
    match operator {
        b'E' => left.eq_ignore_ascii_case(right),
        b'N' => !left.eq_ignore_ascii_case(right),
        _ => false,
    }
}

fn compare_versions(left: (i32, i32), operator: u8, right: (i32, i32)) -> bool {
    match operator {
        b'E' => left == right,
        b'N' => left != right,
        b'>' => left > right,
        b'G' => left >= right,
        b'<' => left < right,
        b'L' => left <= right,
        _ => false,
    }
}

fn parse_version(text: &[u8]) -> Option<(i32, i32)> {
    let text = String::from_utf8_lossy(text);
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

fn readline_name() -> String {
    let pointer = unsafe { ptr::addr_of!(state::rl_readline_name).read() };
    if pointer.is_null() {
        String::new()
    } else {
        // SAFETY: rl_readline_name is a NUL-terminated application string.
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }
}

fn parser_if(state: &mut Store, arguments: &[u8]) {
    state.if_stack.push(state.conditionalized_out);
    if state.conditionalized_out {
        return;
    }

    let (first, rest) = split_first_word(arguments);
    let first_text = String::from_utf8_lossy(first);
    let lower = first_text.to_ascii_lowercase();

    if let Some(term) = lower.strip_prefix("term=") {
        let terminal = std::env::var("TERM").unwrap_or_default();
        let short = terminal.split('-').next().unwrap_or("");
        state.conditionalized_out =
            !(term.eq_ignore_ascii_case(short) || term.eq_ignore_ascii_case(&terminal));
        return;
    }

    if let Some(mode) = lower.strip_prefix("mode=") {
        state.conditionalized_out = !mode.eq_ignore_ascii_case(mode_name(state.editing_mode));
        return;
    }

    if lower == "version" {
        let Some((operator, operand)) = parse_comparison(rest) else {
            state.conditionalized_out = true;
            return;
        };
        let Some((major, minor)) = parse_version(operand) else {
            state.conditionalized_out = true;
            return;
        };
        state.conditionalized_out = !compare_versions((8, 3), operator, (major, minor));
        return;
    }

    if first_text.eq_ignore_ascii_case(&readline_name()) {
        state.conditionalized_out = false;
        return;
    }

    // Variable comparisons: `$if editing-mode == emacs` or
    // `$if convert-meta != off`.  The first word may itself contain the
    // comparison when there is no whitespace.
    let (variable, remainder) = if lower.contains('=') {
        let split = first
            .iter()
            .position(|byte| *byte == b'=')
            .unwrap_or(first.len());
        (&first_text[..split], &first[split..])
    } else {
        (first_text.as_ref(), rest)
    };

    if bool_indices(variable).is_empty() && string_index(variable).is_none() {
        state.conditionalized_out = true;
        return;
    }

    let Some((operator, operand)) = parse_comparison(remainder) else {
        state.conditionalized_out = true;
        return;
    };
    let operand = String::from_utf8_lossy(trim_end_whitespace(operand));
    let current = if let Some(value) = bool_value(state, variable) {
        if value {
            "on".to_string()
        } else {
            "off".to_string()
        }
    } else {
        let index = string_index(variable).unwrap();
        string_value_impl(state, index).unwrap_or_default()
    };
    state.conditionalized_out = !compare_values(&current, operator, &operand);
}

fn parser_else(state: &mut Store) {
    if state.if_stack.is_empty() {
        return;
    }
    if state.if_stack.iter().any(|value| *value) {
        return;
    }
    state.conditionalized_out = !state.conditionalized_out;
}

fn parser_endif(state: &mut Store) {
    if let Some(value) = state.if_stack.pop() {
        state.conditionalized_out = value;
    }
}

fn expand_tilde(filename: &[u8]) -> PathBuf {
    if filename == b"~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = filename.strip_prefix(b"~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut path = PathBuf::from(home);
            path.push(OsStr::from_bytes(rest));
            return path;
        }
    }
    PathBuf::from(OsStr::from_bytes(filename))
}

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[cfg(not(unix))]
trait OsStrExt {
    fn from_bytes(bytes: &[u8]) -> &OsStr;
}

#[cfg(not(unix))]
impl OsStrExt for OsStr {
    fn from_bytes(bytes: &[u8]) -> &OsStr {
        // The ABI passes bytes; non-Unix targets get a lossy view.
        OsStr::new(std::str::from_utf8(bytes).unwrap_or(""))
    }
}

fn errno_from(error: &std::io::Error) -> c_int {
    error.raw_os_error().unwrap_or(libc::EIO)
}

fn read_init_file_bytes(state: &mut Store, filename: &[u8], level: u32) -> std::io::Result<()> {
    if level > 16 {
        return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
    }
    let path = expand_tilde(filename);
    let data = fs::read(&path)?;

    if level == 0 {
        state.last_init_file = Some(PathBuf::from(OsStr::from_bytes(filename)));
        state.if_stack.clear();
        state.conditionalized_out = false;
    }

    for raw_line in data.split(|byte| *byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        let line = trim_start_whitespace(line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        parse_and_bind_impl(state, line);
    }
    Ok(())
}

fn read_init_file_impl(state: &mut Store, filename: Option<&[u8]>) -> c_int {
    let (chosen, fallback) = if let Some(filename) = filename.filter(|name| !name.is_empty()) {
        (filename.to_vec(), false)
    } else if let Some(last) = &state.last_init_file {
        (last.as_os_str().as_bytes().to_vec(), false)
    } else if let Some(inputrc) = std::env::var_os("INPUTRC") {
        let bytes = inputrc.as_os_str().as_bytes().to_vec();
        if bytes.is_empty() {
            (b"~/.inputrc".to_vec(), true)
        } else {
            (bytes, false)
        }
    } else {
        (b"~/.inputrc".to_vec(), true)
    };

    match read_init_file_bytes(state, &chosen, 0) {
        Ok(()) => 0,
        Err(error) => {
            if fallback {
                match read_init_file_bytes(state, b"/etc/inputrc", 0) {
                    Ok(()) => 0,
                    Err(fallback_error) => errno_from(&fallback_error),
                }
            } else {
                errno_from(&error)
            }
        }
    }
}

fn glean_key_from_name(name: &[u8]) -> u8 {
    const NAMES: &[(&[u8], u8)] = &[
        (b"DEL", 0x7f),
        (b"ESC", 0x1b),
        (b"Escape", 0x1b),
        (b"LFD", b'\n'),
        (b"Newline", b'\n'),
        (b"RET", b'\r'),
        (b"Return", b'\r'),
        (b"Rubout", 0x7f),
        (b"SPC", b' '),
        (b"Space", b' '),
        (b"Tab", b'\t'),
    ];
    for (candidate, value) in NAMES {
        if name.eq_ignore_ascii_case(candidate) {
            return *value;
        }
    }
    name.first().copied().unwrap_or(0)
}

fn trim_quotes(bytes: &[u8]) -> &[u8] {
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        &bytes[1..bytes.len() - 1]
    } else {
        bytes
    }
}

fn macro_bind_impl(state: &mut Store, keyseq: &[u8], macro_text: &[u8], map: Keymap) -> c_int {
    let macro_bytes = match translate_impl(macro_text, meta_prefix(state), false) {
        Ok(bytes) => bytes,
        Err(()) => return -1,
    };
    let allocation = duplicate_bytes(&macro_bytes);
    if allocation.is_null() {
        return -1;
    }
    generic_bind_impl(
        state,
        ISMACR as c_int,
        keyseq,
        allocation as *mut c_void,
        map,
    )
}

fn parse_set(state: &mut Store, text: &[u8]) -> c_int {
    let text = trim_start_whitespace(text);
    let (variable, rest) = split_first_word(text);
    if variable.is_empty() {
        return 1;
    }
    let variable = String::from_utf8_lossy(variable);
    let rest = trim_start_whitespace(rest);

    let value = if rest.first() == Some(&b'"') {
        let end = skip_to_delimiter(rest, 1, b'"');
        if end >= rest.len() {
            return 1;
        }
        String::from_utf8_lossy(&rest[1..end]).into_owned()
    } else if !bool_indices(&variable).is_empty() {
        let (word, _) = split_first_word(rest);
        String::from_utf8_lossy(word).into_owned()
    } else {
        String::from_utf8_lossy(trim_end_whitespace(rest)).into_owned()
    };

    variable_bind_impl(state, &variable, Some(&value))
}

fn handle_directive(state: &mut Store, directive: &[u8]) -> c_int {
    let (name, arguments) = split_first_word(directive);
    if name.eq_ignore_ascii_case(b"if") {
        parser_if(state, arguments);
        0
    } else if name.eq_ignore_ascii_case(b"else") {
        parser_else(state);
        0
    } else if name.eq_ignore_ascii_case(b"endif") {
        parser_endif(state);
        0
    } else if name.eq_ignore_ascii_case(b"include") {
        if state.conditionalized_out {
            return 0;
        }
        let filename = trim_end_whitespace(arguments);
        if filename.is_empty() {
            return 1;
        }
        match read_init_file_bytes(state, filename, 1) {
            Ok(()) => 0,
            Err(error) => errno_from(&error),
        }
    } else {
        1
    }
}

fn parse_and_bind_impl(state: &mut Store, string: &[u8]) -> c_int {
    let string = trim_start_whitespace(string);
    if string.is_empty() || string[0] == b'#' {
        return 0;
    }
    if string[0] == b'$' {
        handle_directive(state, &string[1..]);
        return 0;
    }
    if state.conditionalized_out {
        return 0;
    }

    let mut index = 0usize;
    if string[0] == b'"' {
        index = skip_to_delimiter(string, 1, b'"');
        if index >= string.len() {
            return 1;
        }
        index += 1;
    }

    while index < string.len()
        && string[index] != b':'
        && string[index] != b' '
        && string[index] != b'\t'
    {
        index += 1;
    }
    if index == 0 {
        return 1;
    }

    let equivalency =
        index + 1 < string.len() && string[index] == b':' && string[index + 1] == b'=';
    let found_separator = index < string.len();
    let key_end = index;
    if index < string.len() {
        index += 1;
    }
    if equivalency && index < string.len() {
        index += 1;
    }
    let keyname = &string[..key_end];

    if keyname.eq_ignore_ascii_case(b"set") {
        return parse_set(state, &string[index..]);
    }

    while index < string.len() && is_whitespace(string[index]) {
        index += 1;
    }
    let function_start = index;
    if index < string.len() && (string[index] == b'"' || string[index] == b'\'') {
        let delimiter = string[index];
        let end = skip_to_delimiter(string, index + 1, delimiter);
        if end >= string.len() {
            return 1;
        }
        index = end + 1;
    }
    while index < string.len() && !is_whitespace(string[index]) {
        index += 1;
    }
    let funname = &string[function_start..index];

    if equivalency {
        return 0;
    }
    if !found_separator {
        return 1;
    }

    if keyname.first() == Some(&b'"') {
        let inner = trim_quotes(keyname);
        if funname.first() == Some(&b'"') || funname.first() == Some(&b'\'') {
            macro_bind_impl(state, inner, trim_quotes(funname), state.current)
        } else {
            let function = named_function_impl(state, funname);
            generic_bind_impl(
                state,
                ISFUNC as c_int,
                inner,
                function_to_ptr(function),
                state.current,
            )
        }
    } else {
        let kname = keyname
            .rsplit(|byte| *byte == b'-')
            .next()
            .unwrap_or(keyname);
        let mut key = glean_key_from_name(kname) as c_int;
        let mut found_modifier = false;

        if contains_ignore_ascii_case(keyname, b"Control-")
            || contains_ignore_ascii_case(keyname, b"C-")
            || contains_ignore_ascii_case(keyname, b"CTRL-")
        {
            key = c_int::from((key as u8).to_ascii_uppercase() & 0x1f);
            found_modifier = true;
        }
        if contains_ignore_ascii_case(keyname, b"Meta")
            || contains_ignore_ascii_case(keyname, b"M-")
        {
            key |= 0x80;
            found_modifier = true;
        }
        if !found_modifier && kname.len() != keyname.len() {
            return 1;
        }

        if funname.first() == Some(&b'"') || funname.first() == Some(&b'\'') {
            let sequence = [key as u8];
            macro_bind_impl(state, &sequence, trim_quotes(funname), state.current)
        } else {
            let function = named_function_impl(state, funname);
            bind_key_impl(state, state.current, key, function)
        }
    }
}

// --- Keymap management ABI ------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rl_make_bare_keymap() -> Keymap {
    make_bare_keymap()
}

#[no_mangle]
pub unsafe extern "C" fn rl_empty_keymap(map: Keymap) -> c_int {
    c_int::from(empty_keymap(map))
}

#[no_mangle]
pub unsafe extern "C" fn rl_copy_keymap(map: Keymap) -> Keymap {
    copy_keymap(map)
}

#[no_mangle]
pub unsafe extern "C" fn rl_discard_keymap(map: Keymap) {
    discard_keymap(map);
}

#[no_mangle]
pub unsafe extern "C" fn rl_free_keymap(map: Keymap) {
    if map.is_null() || is_builtin_keymap(map) {
        return;
    }
    discard_keymap(map);
    libc::free(map as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn rl_make_keymap() -> Keymap {
    let map = make_bare_keymap();
    if map.is_null() {
        return map;
    }
    let insert = function_to_ptr(default_command("self-insert"));
    let rubout = function_to_ptr(default_command("backward-delete-char"));
    for index in 0x20..0x7f {
        let entry = &mut *map.add(index);
        entry.function = insert;
    }
    (*map.add(b'\t' as usize)).function = insert;
    (*map.add(RUBOUT as usize)).function = rubout;
    (*map.add(0x08)).function = rubout;
    for index in 0x80..ANYOTHERKEY {
        (*map.add(index)).function = insert;
    }
    map
}

#[no_mangle]
pub unsafe extern "C" fn rl_get_keymap() -> Keymap {
    ensure_initialized();
    store().current
}

#[no_mangle]
pub unsafe extern "C" fn rl_set_keymap(map: Keymap) {
    if map.is_null() {
        return;
    }
    ensure_initialized();
    store().current = map;
}

#[no_mangle]
pub unsafe extern "C" fn rl_get_keymap_by_name(name: *const c_char) -> Keymap {
    let Some(name) = c_string(name) else {
        return ptr::null_mut();
    };
    ensure_initialized();
    get_keymap_by_name_impl(&store(), &name)
}

#[no_mangle]
pub unsafe extern "C" fn rl_get_keymap_name(map: Keymap) -> *mut c_char {
    ensure_initialized();
    let state = store();
    if map == standard_keymap_ptr() {
        return EMACS_KEYMAP_NAME.as_ptr() as *mut c_char;
    }
    if map == meta_keymap_ptr() {
        return EMACS_META_KEYMAP_NAME.as_ptr() as *mut c_char;
    }
    if map == ctlx_keymap_ptr() {
        return EMACS_CTLX_KEYMAP_NAME.as_ptr() as *mut c_char;
    }
    if map == vi_movement_keymap_ptr() {
        return VI_KEYMAP_NAME.as_ptr() as *mut c_char;
    }
    if map == vi_insertion_keymap_ptr() {
        return VI_INSERT_KEYMAP_NAME.as_ptr() as *mut c_char;
    }
    state
        .named
        .iter()
        .find(|entry| entry.map == map)
        .map(|entry| entry.name.as_ptr() as *mut c_char)
        .unwrap_or(ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn rl_set_keymap_name(name: *const c_char, map: Keymap) -> c_int {
    let Some(name) = c_string(name) else {
        return -1;
    };
    if map.is_null() || builtin_keymap_by_name(&name).is_some() || is_builtin_keymap(map) {
        return -1;
    }
    ensure_initialized();
    let mut state = store();

    if let Some(index) = state.named.iter().position(|entry| entry.map == map) {
        if let Ok(name) = CString::new(name) {
            state.named[index].name = name;
        }
        return (index + 8) as c_int;
    }

    if let Some(index) = state.named.iter().position(|entry| {
        entry
            .name
            .to_str()
            .is_ok_and(|candidate| candidate.eq_ignore_ascii_case(&name))
    }) {
        state.named[index].map = map;
        return (index + 8) as c_int;
    }

    if let Ok(name) = CString::new(name) {
        state.named.push(NamedKeymap { name, map });
        return (state.named.len() + 7) as c_int;
    }
    -1
}

#[no_mangle]
pub unsafe extern "C" fn rl_set_keymap_from_edit_mode() {
    ensure_initialized();
    let mut state = store();
    state.current = match state.editing_mode {
        EditMode::Emacs => standard_keymap_ptr(),
        EditMode::Vi => vi_insertion_keymap_ptr(),
    };
}

#[no_mangle]
pub unsafe extern "C" fn rl_get_keymap_name_from_edit_mode() -> *mut c_char {
    ensure_initialized();
    match store().editing_mode {
        EditMode::Emacs => EMACS_MODE_NAME.as_ptr() as *mut c_char,
        EditMode::Vi => VI_MODE_NAME.as_ptr() as *mut c_char,
    }
}

// --- Funmap ABI -----------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rl_initialize_funmap() {
    ensure_initialized();
}

#[no_mangle]
pub unsafe extern "C" fn rl_add_funmap_entry(
    name: *const c_char,
    function: RlCommandFunc,
) -> c_int {
    ensure_initialized();
    let name = String::from_utf8_lossy(c_bytes(name));
    let mut state = store();
    state.funmap.push(FunmapEntry {
        name: CString::new(name.as_bytes()).unwrap_or_default(),
        function,
    });
    state.funmap.len() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn rl_add_defun(
    name: *const c_char,
    function: RlCommandFunc,
    key: c_int,
) -> c_int {
    ensure_initialized();
    let mut state = store();
    if key != -1 {
        let map = state.current;
        bind_key_impl(&mut state, map, key, function);
    }
    let name = String::from_utf8_lossy(c_bytes(name));
    state.funmap.push(FunmapEntry {
        name: CString::new(name.as_bytes()).unwrap_or_default(),
        function,
    });
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_named_function(name: *const c_char) -> RlCommandFunc {
    ensure_initialized();
    named_function_impl(&store(), c_bytes(name))
}

#[no_mangle]
pub unsafe extern "C" fn rl_funmap_names() -> *mut *const c_char {
    ensure_initialized();
    let state = store();
    let mut names: Vec<&CString> = state.funmap.iter().map(|entry| &entry.name).collect();
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    if names.is_empty() {
        return ptr::null_mut();
    }

    let layout = std::mem::size_of::<*const c_char>();
    let array = libc::malloc((names.len() + 1) * layout) as *mut *const c_char;
    if array.is_null() {
        return ptr::null_mut();
    }
    for (index, name) in names.iter().enumerate() {
        *array.add(index) = name.as_ptr() as *const c_char;
    }
    *array.add(names.len()) = ptr::null();
    array
}

#[no_mangle]
pub unsafe extern "C" fn rl_list_funmap_names() {
    let names = rl_funmap_names();
    if names.is_null() {
        return;
    }
    let mut index = 0usize;
    while !(*names.add(index)).is_null() {
        let name = CStr::from_ptr(*names.add(index)).to_bytes();
        write_stdout(name);
        write_stdout(b"\n");
        index += 1;
    }
    libc::free(names as *mut c_void);
}

// --- Binding ABI ----------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rl_bind_key(key: c_int, function: RlCommandFunc) -> c_int {
    ensure_initialized();
    let mut state = store();
    let map = state.current;
    bind_key_impl(&mut state, map, key, function)
}

#[no_mangle]
pub unsafe extern "C" fn rl_bind_key_in_map(
    key: c_int,
    function: RlCommandFunc,
    map: Keymap,
) -> c_int {
    ensure_initialized();
    bind_key_impl(&mut store(), map, key, function)
}

#[no_mangle]
pub unsafe extern "C" fn rl_unbind_key(key: c_int) -> c_int {
    rl_bind_key(key, None)
}

#[no_mangle]
pub unsafe extern "C" fn rl_unbind_key_in_map(key: c_int, map: Keymap) -> c_int {
    rl_bind_key_in_map(key, None, map)
}

#[no_mangle]
pub unsafe extern "C" fn rl_bind_key_if_unbound_in_map(
    key: c_int,
    function: RlCommandFunc,
    map: Keymap,
) -> c_int {
    if !(0..=255).contains(&key) {
        return key;
    }
    let sequence = untranslate_keyseq_bytes(key as u8);
    rl_bind_keyseq_if_unbound_in_map(sequence.as_ptr() as *const c_char, function, map)
}

#[no_mangle]
pub unsafe extern "C" fn rl_bind_key_if_unbound(key: c_int, function: RlCommandFunc) -> c_int {
    ensure_initialized();
    let map = store().current;
    rl_bind_key_if_unbound_in_map(key, function, map)
}

#[no_mangle]
pub unsafe extern "C" fn rl_bind_keyseq(keyseq: *const c_char, function: RlCommandFunc) -> c_int {
    ensure_initialized();
    let mut state = store();
    let map = state.current;
    generic_bind_impl(
        &mut state,
        ISFUNC as c_int,
        c_bytes(keyseq),
        function_to_ptr(function),
        map,
    )
}

#[no_mangle]
pub unsafe extern "C" fn rl_bind_keyseq_in_map(
    keyseq: *const c_char,
    function: RlCommandFunc,
    map: Keymap,
) -> c_int {
    ensure_initialized();
    generic_bind_impl(
        &mut store(),
        ISFUNC as c_int,
        c_bytes(keyseq),
        function_to_ptr(function),
        map,
    )
}

#[no_mangle]
pub unsafe extern "C" fn rl_bind_keyseq_if_unbound_in_map(
    keyseq: *const c_char,
    function: RlCommandFunc,
    map: Keymap,
) -> c_int {
    ensure_initialized();
    let mut state = store();
    let map = if map.is_null() { state.current } else { map };
    let prefix = meta_prefix(&state);
    let keys = match translate_impl(c_bytes(keyseq), prefix, false) {
        Ok(keys) => keys,
        Err(()) => return -1,
    };
    let existing = function_of_keyseq(&state, &keys, map, ptr::null_mut());
    let existing_pointer = function_to_ptr(existing);
    let lowercase = function_to_ptr(named_function_impl(&state, b"do-lowercase-version"));
    let vi_movement = function_to_ptr(named_function_impl(&state, b"vi-movement-mode"));
    if existing_pointer.is_null()
        || existing_pointer == lowercase
        || existing_pointer == vi_movement
    {
        generic_bind_keys_impl(
            &mut state,
            ISFUNC as c_int,
            &keys,
            function_to_ptr(function),
            map,
        )
    } else {
        1
    }
}

#[no_mangle]
pub unsafe extern "C" fn rl_bind_keyseq_if_unbound(
    keyseq: *const c_char,
    function: RlCommandFunc,
) -> c_int {
    ensure_initialized();
    let map = store().current;
    rl_bind_keyseq_if_unbound_in_map(keyseq, function, map)
}

#[no_mangle]
pub unsafe extern "C" fn rl_generic_bind(
    kind: c_int,
    keyseq: *const c_char,
    data: *mut c_char,
    map: Keymap,
) -> c_int {
    ensure_initialized();
    let mut state = store();
    let map = if map.is_null() { state.current } else { map };
    generic_bind_impl(&mut state, kind, c_bytes(keyseq), data as *mut c_void, map)
}

#[no_mangle]
pub unsafe extern "C" fn rl_macro_bind(
    keyseq: *const c_char,
    macro_text: *const c_char,
    map: Keymap,
) -> c_int {
    ensure_initialized();
    let mut state = store();
    let map = if map.is_null() { state.current } else { map };
    macro_bind_impl(&mut state, c_bytes(keyseq), c_bytes(macro_text), map)
}

#[no_mangle]
pub unsafe extern "C" fn rl_unbind_function_in_map(function: RlCommandFunc, map: Keymap) -> c_int {
    fn unbind(map: Keymap, target: *mut c_void) -> bool {
        if map.is_null() {
            return false;
        }
        let mut changed = false;
        for index in 0..KEYMAP_SIZE {
            let entry = unsafe { &mut *map.add(index) };
            if entry.r#type == ISFUNC && entry.function == target {
                entry.function = ptr::null_mut();
                changed = true;
            } else if entry.r#type == ISKMAP && !entry.function.is_null() {
                changed |= unbind(entry.function.cast::<KeymapEntry>(), target);
            }
        }
        changed
    }

    c_int::from(unbind(map, function_to_ptr(function)))
}

#[no_mangle]
pub unsafe extern "C" fn rl_unbind_command_in_map(command: *const c_char, map: Keymap) -> c_int {
    ensure_initialized();
    let state = store();
    let function = named_function_impl(&state, c_bytes(command));
    let Some(function) = function else {
        return 0;
    };
    rl_unbind_function_in_map(Some(function), map)
}

// --- Lookup ABI -----------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rl_function_of_keyseq(
    keyseq: *const c_char,
    map: Keymap,
    kind: *mut c_int,
) -> RlCommandFunc {
    ensure_initialized();
    let state = store();
    function_of_keyseq(&state, c_bytes(keyseq), map, kind)
}

#[no_mangle]
#[allow(clippy::unnecessary_cast)]
pub unsafe extern "C" fn rl_function_of_keyseq_len(
    keyseq: *const c_char,
    length: usize,
    map: Keymap,
    kind: *mut c_int,
) -> RlCommandFunc {
    ensure_initialized();
    if keyseq.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(keyseq as *const u8, length);
    function_of_keyseq(&store(), bytes, map, kind)
}

#[no_mangle]
pub unsafe extern "C" fn rl_invoking_keyseqs_in_map(
    function: RlCommandFunc,
    map: Keymap,
) -> *mut *mut c_char {
    ensure_initialized();
    let state = store();
    let sequences = invoking_keyseqs_impl(&state, function_to_ptr(function), map);
    if sequences.is_empty() {
        return ptr::null_mut();
    }

    let layout = std::mem::size_of::<*mut c_char>();
    let array = libc::malloc((sequences.len() + 1) * layout) as *mut *mut c_char;
    if array.is_null() {
        return ptr::null_mut();
    }
    for (index, sequence) in sequences.iter().enumerate() {
        *array.add(index) = duplicate_bytes(sequence);
    }
    *array.add(sequences.len()) = ptr::null_mut();
    array
}

#[no_mangle]
pub unsafe extern "C" fn rl_invoking_keyseqs(function: RlCommandFunc) -> *mut *mut c_char {
    ensure_initialized();
    let state = store();
    let map = state.current;
    drop(state);
    rl_invoking_keyseqs_in_map(function, map)
}

// --- Translation ABI ------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rl_translate_keyseq(
    sequence: *const c_char,
    array: *mut c_char,
    length: *mut c_int,
) -> c_int {
    ensure_initialized();
    let state = store();
    let translated = match translate_impl(c_bytes(sequence), meta_prefix(&state), false) {
        Ok(translated) => translated,
        Err(()) => return 1,
    };
    drop(state);

    if !array.is_null() {
        ptr::copy_nonoverlapping(translated.as_ptr(), array.cast::<u8>(), translated.len());
        *array.add(translated.len()) = 0;
    }
    if !length.is_null() {
        *length = translated.len() as c_int;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_untranslate_keyseq(key: c_int) -> *mut c_char {
    static mut BUFFER: [u8; 16] = [0; 16];
    let bytes = untranslate_keyseq_bytes(key as u8);
    let buffer = ptr::addr_of_mut!(BUFFER).cast::<u8>();
    let count = bytes.len().min(15);
    ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, count);
    *buffer.add(count) = 0;
    buffer as *mut c_char
}

// --- Dump and display ABI -------------------------------------------------

fn print_keybinding_text(state: &Store, name: &str, map: Keymap, readable: bool) -> Vec<u8> {
    let function = named_function_impl(state, name.as_bytes());
    let sequences = match function {
        Some(function) => invoking_keyseqs_impl(state, function_to_ptr(Some(function)), map),
        None => Vec::new(),
    };

    let mut output = Vec::new();
    if readable {
        if sequences.is_empty() {
            output.extend_from_slice(b"# ");
            output.extend_from_slice(name.as_bytes());
            output.extend_from_slice(b" (not bound)\n");
        } else {
            for sequence in &sequences {
                output.push(b'"');
                output.extend_from_slice(sequence);
                output.extend_from_slice(b"\": ");
                output.extend_from_slice(name.as_bytes());
                output.push(b'\n');
            }
        }
        return output;
    }

    if sequences.is_empty() {
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(b" is not bound to any keys\n");
        return output;
    }

    output.extend_from_slice(name.as_bytes());
    output.extend_from_slice(b" can be found on ");
    for (index, sequence) in sequences.iter().take(5).enumerate() {
        output.push(b'"');
        output.extend_from_slice(sequence);
        output.push(b'"');
        if index + 1 < sequences.len() {
            output.extend_from_slice(b", ");
        } else {
            output.extend_from_slice(b".\n");
        }
    }
    if sequences.len() > 5 {
        output.extend_from_slice(b"...\n");
    }
    output
}

fn function_dump(state: &Store, readable: bool) -> Vec<u8> {
    let mut names: Vec<&str> = state
        .funmap
        .iter()
        .filter_map(|entry| entry.name.to_str().ok())
        .collect();
    names.sort_unstable();
    names.dedup();

    let mut output = b"\n".to_vec();
    for name in names {
        output.extend(print_keybinding_text(state, name, state.current, readable));
    }
    output
}

fn macro_dump(state: &Store, readable: bool) -> Vec<u8> {
    fn walk(map: Keymap, prefix: &[u8], readable: bool, output: &mut Vec<u8>, depth: usize) {
        if map.is_null() || depth > 32 {
            return;
        }
        for key in 0..KEYMAP_SIZE {
            // SAFETY: map is a KEYMAP_SIZE entry array.
            let entry = unsafe { &*map.add(key) };
            match entry.r#type {
                ISMACR => {
                    let mut keyname = prefix.to_vec();
                    keyname.extend(keyname_bytes(key));
                    let macro_bytes = if entry.function.is_null() {
                        Vec::new()
                    } else {
                        unsafe { CStr::from_ptr(entry.function.cast::<c_char>()) }
                            .to_bytes()
                            .to_vec()
                    };
                    let expansion = untranslate_macro_bytes(&macro_bytes);
                    if readable {
                        output.push(b'"');
                        output.extend_from_slice(&keyname);
                        output.extend_from_slice(b"\": \"");
                        output.extend_from_slice(&expansion);
                        output.extend_from_slice(b"\"\n");
                    } else {
                        output.extend_from_slice(&keyname);
                        output.extend_from_slice(b" outputs ");
                        output.extend_from_slice(&expansion);
                        output.push(b'\n');
                    }
                }
                ISKMAP if !entry.function.is_null() => {
                    let mut next_prefix = prefix.to_vec();
                    next_prefix.extend(keyname_bytes(key));
                    walk(
                        entry.function.cast::<KeymapEntry>(),
                        &next_prefix,
                        readable,
                        output,
                        depth + 1,
                    );
                }
                _ => {}
            }
        }
    }

    let mut output = Vec::new();
    walk(state.current, &[], readable, &mut output, 0);
    output
}

fn variable_dump(state: &Store, readable: bool) -> Vec<u8> {
    let mut output = Vec::new();
    for (index, (name, _)) in BOOLEAN_DEFAULTS.iter().enumerate() {
        let value = if state.booleans.get(index).copied().unwrap_or(false) {
            "on"
        } else {
            "off"
        };
        if readable {
            output.extend_from_slice(format!("set {name} {value}\n").as_bytes());
        } else {
            output.extend_from_slice(format!("{name} is set to `{value}'\n").as_bytes());
        }
    }
    for (index, (name, _)) in STRING_DEFAULTS.iter().enumerate() {
        let Some(value) = string_value_impl(state, index) else {
            continue;
        };
        if readable {
            output.extend_from_slice(format!("set {name} {value}\n").as_bytes());
        } else {
            output.extend_from_slice(format!("{name} is set to `{value}'\n").as_bytes());
        }
    }
    output
}

#[no_mangle]
pub unsafe extern "C" fn rl_print_keybinding(name: *const c_char, map: Keymap, readable: c_int) {
    ensure_initialized();
    let state = store();
    let name = String::from_utf8_lossy(c_bytes(name)).into_owned();
    let text = print_keybinding_text(&state, &name, map, readable != 0);
    drop(state);
    write_stdout(&text);
}

#[no_mangle]
pub unsafe extern "C" fn rl_function_dumper(readable: c_int) {
    ensure_initialized();
    let state = store();
    let text = function_dump(&state, readable != 0);
    drop(state);
    write_stdout(&text);
}

#[no_mangle]
pub unsafe extern "C" fn rl_macro_dumper(readable: c_int) {
    ensure_initialized();
    let state = store();
    let text = macro_dump(&state, readable != 0);
    drop(state);
    write_stdout(&text);
}

#[no_mangle]
pub unsafe extern "C" fn rl_variable_dumper(readable: c_int) {
    ensure_initialized();
    let state = store();
    let text = variable_dump(&state, readable != 0);
    drop(state);
    write_stdout(&text);
}

#[no_mangle]
pub unsafe extern "C" fn rl_dump_functions(count: c_int, _key: c_int) -> c_int {
    rl_function_dumper(count);
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_dump_macros(count: c_int, _key: c_int) -> c_int {
    rl_macro_dumper(count);
    0
}

#[no_mangle]
pub unsafe extern "C" fn rl_dump_variables(count: c_int, _key: c_int) -> c_int {
    rl_variable_dumper(count);
    0
}

// --- Variables and init-file ABI -----------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rl_variable_value(name: *const c_char) -> *mut c_char {
    let Some(name) = c_string(name) else {
        return ptr::null_mut();
    };
    ensure_initialized();
    variable_value_impl(&mut store(), &name)
}

#[no_mangle]
pub unsafe extern "C" fn rl_variable_bind(name: *const c_char, value: *const c_char) -> c_int {
    let Some(name) = c_string(name) else {
        return 0;
    };
    let value = c_string(value);
    ensure_initialized();
    variable_bind_impl(&mut store(), &name, value.as_deref())
}

#[no_mangle]
pub unsafe extern "C" fn rl_parse_and_bind(string: *mut c_char) -> c_int {
    if string.is_null() {
        return 0;
    }
    ensure_initialized();
    let bytes = c_bytes(string).to_vec();
    parse_and_bind_impl(&mut store(), &bytes)
}

#[no_mangle]
pub unsafe extern "C" fn rl_read_init_file(filename: *const c_char) -> c_int {
    ensure_initialized();
    let bytes = if filename.is_null() {
        None
    } else {
        Some(c_bytes(filename).to_vec())
    };
    read_init_file_impl(&mut store(), bytes.as_deref())
}

#[no_mangle]
pub unsafe extern "C" fn rl_re_read_init_file(_count: c_int, _key: c_int) -> c_int {
    ensure_initialized();
    let mut state = store();
    let result = read_init_file_impl(&mut state, None);
    state.current = match state.editing_mode {
        EditMode::Emacs => standard_keymap_ptr(),
        EditMode::Vi => vi_insertion_keymap_ptr(),
    };
    result
}

// --- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn test_command(_: c_int, _: c_int) -> c_int {
        0
    }

    /// Translate a bind spelling and look up the raw key bytes, the way a
    /// GNU caller must (rl_function_of_keyseq itself takes raw bytes).
    fn lookup_spelling(spelling: &std::ffi::CStr, kind: *mut c_int) -> RlCommandFunc {
        let mut raw = [0u8; 64];
        let mut length = 0;
        let rc =
            unsafe { rl_translate_keyseq(spelling.as_ptr(), raw.as_mut_ptr().cast(), &mut length) };
        assert_eq!(rc, 0);
        unsafe { rl_function_of_keyseq(raw.as_ptr().cast(), ptr::null_mut(), kind) }
    }

    fn same_function(left: RlCommandFunc, right: RlCommandFunc) -> bool {
        match (left, right) {
            (Some(left), Some(right)) => std::ptr::fn_addr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }

    #[test]
    fn translation_covers_gnu_spellings() {
        assert_eq!(translate_impl(b"\\C-x", true, false).unwrap(), vec![0x18]);
        assert_eq!(translate_impl(b"\\C-?", true, false).unwrap(), vec![0x7f]);
        // GNU rl_translate_keyseq does not implement caret notation; the
        // inputrc/bind parser handles it before calling here.
        assert_eq!(translate_impl(b"^?", true, false).unwrap(), b"^?".to_vec());
        assert_eq!(translate_impl(b"^X", true, false).unwrap(), b"^X".to_vec());
        assert_eq!(
            translate_impl(b"\\e[1;5C", true, false).unwrap(),
            b"\x1b[1;5C".to_vec()
        );
        assert_eq!(
            translate_impl(b"\\M-x", true, false).unwrap(),
            vec![0x1b, b'x']
        );
        assert_eq!(translate_impl(b"\\M-x", false, false).unwrap(), vec![0xf8]);
        // Quotes are literal at the ABI layer; the parser trims them.
        assert_eq!(
            translate_impl(b"\"\\C-x\\C-r\"", true, false).unwrap(),
            b"\"\x18\x12\"".to_vec()
        );
        assert_eq!(
            translate_impl(b"\\x41\\101", true, false).unwrap(),
            b"AA".to_vec()
        );
        assert_eq!(
            translate_impl(b"\\M-\\C-x", true, false).unwrap(),
            vec![0x1b, 0x18]
        );
        // Parser convenience mode still strips quotes/handles caret.
        assert_eq!(translate_impl(b"^?", true, true).unwrap(), vec![0x7f]);
        assert!(translate_impl(b"\"unterminated", true, true).is_err());
    }

    #[test]
    fn keynames_round_trip_control_meta_and_csi() {
        assert_eq!(keyname_bytes(0x18), b"\\C-x".to_vec());
        assert_eq!(keyname_bytes(0x7f), b"\\C-?".to_vec());
        assert_eq!(keyname_bytes(ESC), b"\\e".to_vec());
        assert_eq!(untranslate_keyseq_bytes(0xe8), b"\\M-h".to_vec());
        assert_eq!(untranslate_keyseq_bytes(0x18), b"\\C-x".to_vec());
    }

    #[test]
    fn keymap_funmap_bind_variables_and_inputrc_round_trip() {
        ensure_initialized();

        // Default maps materialise with GNU structure.
        {
            let state = store();
            assert_eq!(state.current, standard_keymap_ptr());
            assert_eq!(
                keymap_name_impl(&state, standard_keymap_ptr()),
                Some("emacs")
            );
            assert_eq!(
                get_keymap_by_name_impl(&state, "VI-INSERT"),
                vi_insertion_keymap_ptr()
            );
            assert!(named_function_impl(&state, b"beginning-of-line").is_some());
        }
        let ctlx = unsafe { &*standard_keymap_ptr().add(24) };
        assert_eq!(ctlx.r#type, ISKMAP);
        assert_eq!(ctlx.function.cast::<KeymapEntry>(), ctlx_keymap_ptr());
        let meta = unsafe { &*standard_keymap_ptr().add(27) };
        assert_eq!(meta.r#type, ISKMAP);
        assert_eq!(meta.function.cast::<KeymapEntry>(), meta_keymap_ptr());

        // Custom map round-trip: bind, lookup, invoking sequences, copy, free.
        let map = make_bare_keymap();
        assert!(!map.is_null());
        assert!(empty_keymap(map));
        let function = Some(test_command as unsafe extern "C" fn(c_int, c_int) -> c_int);
        assert_eq!(
            unsafe { rl_bind_keyseq_in_map(c"\\C-x\\C-r".as_ptr(), function, map) },
            0
        );
        let mut kind = -1;
        // rl_function_of_keyseq takes raw key bytes, like GNU; translate the
        // bind spelling first.
        let mut raw = [0u8; 16];
        let mut raw_len = 0;
        assert_eq!(
            unsafe {
                rl_translate_keyseq(
                    c"\\C-x\\C-r".as_ptr(),
                    raw.as_mut_ptr().cast(),
                    &mut raw_len,
                )
            },
            0
        );
        let found = unsafe { rl_function_of_keyseq(raw.as_ptr().cast(), map, &mut kind) };
        assert!(same_function(found, function));
        assert_eq!(kind, ISFUNC as c_int);

        let sequences = invoking_keyseqs_impl(&store(), function_to_ptr(function), map);
        assert!(sequences.iter().any(|sequence| sequence == b"\\C-x\\C-r"));
        let copy = copy_keymap(map);
        let copied_child = unsafe { (*copy.add(0x18)).function.cast::<KeymapEntry>() };
        assert_eq!(
            unsafe { (*copied_child.add(0x12)).function },
            function_to_ptr(function)
        );
        unsafe {
            // A copied keymap shares child pointers with its source; detach
            // the shared child before freeing both arrays.
            (*copy.add(0x18)).r#type = ISFUNC;
            (*copy.add(0x18)).function = ptr::null_mut();
            rl_free_keymap(copy);
            rl_free_keymap(map);
        }

        // Funmap additions are case-insensitive and drive binds.
        let name = CString::new("ruline-test-command").unwrap();
        assert_eq!(
            unsafe { rl_add_funmap_entry(name.as_ptr(), function) },
            store().funmap.len() as c_int
        );
        assert!(same_function(
            unsafe { rl_named_function(name.as_ptr()) },
            function
        ));
        let upper = CString::new("RULINE-TEST-COMMAND").unwrap();
        assert!(same_function(
            unsafe { rl_named_function(upper.as_ptr()) },
            function
        ));

        let names = unsafe { rl_funmap_names() };
        assert!(!names.is_null());
        let mut found_name = false;
        let mut index = 0;
        while unsafe { !(*names.add(index)).is_null() } {
            if unsafe { CStr::from_ptr(*names.add(index)) }.to_bytes() == b"ruline-test-command" {
                found_name = true;
            }
            index += 1;
        }
        if !names.is_null() {
            unsafe { libc::free(names as *mut c_void) };
        }
        assert!(found_name);

        // Variables: booleans, string values, editing mode and keymap.
        let convert = CString::new("convert-meta").unwrap();
        let off = CString::new("off").unwrap();
        let on = CString::new("on").unwrap();
        assert_eq!(
            unsafe { rl_variable_bind(convert.as_ptr(), off.as_ptr()) },
            0
        );
        assert_eq!(
            unsafe { CStr::from_ptr(rl_variable_value(convert.as_ptr())) }.to_bytes(),
            b"off"
        );
        assert_eq!(
            unsafe { rl_variable_bind(convert.as_ptr(), on.as_ptr()) },
            0
        );
        let editing = CString::new("editing-mode").unwrap();
        let vi = CString::new("vi").unwrap();
        assert_eq!(
            unsafe { rl_variable_bind(editing.as_ptr(), vi.as_ptr()) },
            0
        );
        assert_eq!(unsafe { rl_get_keymap() }, vi_insertion_keymap_ptr());
        let emacs = CString::new("emacs").unwrap();
        assert_eq!(
            unsafe { rl_variable_bind(editing.as_ptr(), emacs.as_ptr()) },
            0
        );
        assert_eq!(unsafe { rl_get_keymap() }, standard_keymap_ptr());

        // parse_and_bind: key binding, macro binding, and $if/$else/$endif.
        let binding = CString::new("\"\\C-x\\C-t\": transpose-chars").unwrap();
        assert_eq!(
            unsafe { rl_parse_and_bind(binding.as_ptr() as *mut c_char) },
            0
        );
        assert!(same_function(
            lookup_spelling(c"\\C-x\\C-t", ptr::null_mut()),
            unsafe { rl_named_function(CString::new("transpose-chars").unwrap().as_ptr()) }
        ));

        let macro_line = CString::new("\"\\C-xm\": \"hi\"").unwrap();
        assert_eq!(
            unsafe { rl_parse_and_bind(macro_line.as_ptr() as *mut c_char) },
            0
        );
        let mut macro_kind = -1;
        let macro_pointer = lookup_spelling(c"\\C-xm", &mut macro_kind);
        assert_eq!(macro_kind, ISMACR as c_int);
        let macro_pointer = macro_pointer
            .map(|function| function as *const () as *const c_char)
            .unwrap_or(ptr::null());
        assert_eq!(unsafe { CStr::from_ptr(macro_pointer) }.to_bytes(), b"hi");

        let conditional = CString::new("$if mode=emacs").unwrap();
        assert_eq!(
            unsafe { rl_parse_and_bind(conditional.as_ptr() as *mut c_char) },
            0
        );
        let then_line = CString::new("\"\\C-xd\": dump-functions").unwrap();
        assert_eq!(
            unsafe { rl_parse_and_bind(then_line.as_ptr() as *mut c_char) },
            0
        );
        let else_line = CString::new("$else").unwrap();
        assert_eq!(
            unsafe { rl_parse_and_bind(else_line.as_ptr() as *mut c_char) },
            0
        );
        let skipped = CString::new("\"\\C-xk\": kill-line").unwrap();
        assert_eq!(
            unsafe { rl_parse_and_bind(skipped.as_ptr() as *mut c_char) },
            0
        );
        let endif_line = CString::new("$endif").unwrap();
        assert_eq!(
            unsafe { rl_parse_and_bind(endif_line.as_ptr() as *mut c_char) },
            0
        );

        let dumped = function_dump(&store(), true);
        let dumped = String::from_utf8_lossy(&dumped);
        assert!(dumped.contains("ruline-test-command"));
        let macros = macro_dump(&store(), true);
        assert!(String::from_utf8_lossy(&macros).contains("\\C-xm"));
        let variables = variable_dump(&store(), true);
        assert!(String::from_utf8_lossy(&variables).contains("set convert-meta on"));

        // Init-file reading and re-reading.
        let path = std::env::temp_dir().join(format!("ruline-inputrc-{}", std::process::id()));
        fs::write(
            &path,
            b"set editing-mode emacs\n\"\\C-xw\": \"macro text\"\n",
        )
        .unwrap();
        let path_c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { rl_read_init_file(path_c.as_ptr()) }, 0);
        assert!(lookup_spelling(c"\\C-xw", ptr::null_mut()).is_some());
        assert_eq!(unsafe { rl_re_read_init_file(0, 0) }, 0);
        let _ = fs::remove_file(&path);

        // Translate through the ABI buffer.
        let mut buffer = [0u8; 32];
        let mut length = -1;
        assert_eq!(
            unsafe {
                rl_translate_keyseq(
                    c"\\e[1;5C".as_ptr(),
                    buffer.as_mut_ptr() as *mut c_char,
                    &mut length,
                )
            },
            0
        );
        assert_eq!(length, 6);
        assert_eq!(&buffer[..6], &[0x1b, b'[', b'1', b';', b'5', b'C']);
        assert_eq!(buffer[6], 0);
    }
}
