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
#[ignore = "A6-F11: known divergence, see deep-audit A6 REPORT.md"]
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
fn h4_capacity_trims_oldest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hist");
    let mut h = FileBackedHistory::with_file(2, path).unwrap();
    for s in ["a", "b", "c"] {
        h.save(HistoryItem::from_command_line(s)).unwrap();
    }
    assert_eq!(all_texts(&h), vec!["b", "c"]);
}
