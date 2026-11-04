//! A6 audit probes — history storage/navigation policy vs GNU history
//! library semantics (`add_history` keeps what it is given; dedup is an
//! application policy, e.g. bash HISTCONTROL, not a library behavior).
#![cfg(test)]

use super::{FileBackedHistory, History, HistoryItem, SearchDirection, SearchQuery};

fn all_texts(hist: &dyn History) -> Vec<String> {
    hist.search(SearchQuery::everything(SearchDirection::Forward, None))
        .unwrap()
        .iter()
        .map(|e| e.command_line.to_string())
        .collect()
}

#[test]
fn h1_save_keeps_consecutive_duplicates() {
    // GNU add_history("ls"); add_history("ls") stores both entries.
    let mut hist = FileBackedHistory::default();
    for cmd in ["ls", "ls", "pwd"] {
        hist.save(HistoryItem::from_command_line(cmd)).unwrap();
    }
    assert_eq!(all_texts(&hist), vec!["ls", "ls", "pwd"]);
}

#[test]
fn h2_save_keeps_empty_check_only() {
    // Non-consecutive duplicates are kept today; pin the behavior.
    let mut hist = FileBackedHistory::default();
    for cmd in ["ls", "pwd", "ls"] {
        hist.save(HistoryItem::from_command_line(cmd)).unwrap();
    }
    assert_eq!(all_texts(&hist), vec!["ls", "pwd", "ls"]);
}

#[test]
fn issue38_saved_duplicates_get_distinct_ids_and_load_back() {
    // A faithful store hands each save its own entry: the second "ls" is
    // a separate item with its own id, loadable on its own — the host's
    // HISTCONTROL layer, not the store, decides what to suppress.
    let mut hist = FileBackedHistory::default();
    let first = hist.save(HistoryItem::from_command_line("ls")).unwrap();
    let second = hist.save(HistoryItem::from_command_line("ls")).unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(hist.load(second.id.unwrap()).unwrap().command_line, "ls");
    assert_eq!(all_texts(&hist), vec!["ls", "ls"]);
}

#[test]
fn issue38_capacity_trim_counts_duplicates() {
    // Capacity trimming counts stored duplicates like any other entry:
    // with capacity 2, saving a,a,b keeps the second a and b.
    let mut hist = FileBackedHistory::new(2).unwrap();
    for cmd in ["a", "a", "b"] {
        hist.save(HistoryItem::from_command_line(cmd)).unwrap();
    }
    assert_eq!(all_texts(&hist), vec!["a", "b"]);
}

#[test]
fn h4_capacity_trims_oldest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hist");
    let mut h = FileBackedHistory::with_file(2, path).unwrap();
    for s in ["a", "b", "c"] {
        h.save(HistoryItem::from_command_line(s)).unwrap();
    }
    assert_eq!(all_texts(&h), vec!["b", "c"]);
}
