//! A short record of each drive's recent rips, to help spot a failing drive.

use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::Path;

/// Enough to show a pattern, and small enough never to need managing.
const ENTRIES_PER_DRIVE: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub drive_id: String,
    pub finished_at: i64,
    pub name: String,
    pub outcome: String,
}

fn parse_entry(value: &Value) -> Option<HistoryEntry> {
    Some(HistoryEntry {
        drive_id: value.get("drive")?.as_str()?.to_string(),
        finished_at: value.get("at")?.as_i64()?,
        name: value.get("name")?.as_str()?.to_string(),
        outcome: value.get("outcome")?.as_str()?.to_string(),
    })
}

/// Every entry, oldest first. A missing or damaged file is an empty history, because
/// history must never stand in the way of ripping.
pub fn read_all(path: &Path) -> Vec<HistoryEntry> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(Value::Array(values)) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    values.iter().filter_map(parse_entry).collect()
}

/// A drive's entries, newest first.
pub fn for_drive(entries: Vec<HistoryEntry>, drive_id: &str) -> Vec<HistoryEntry> {
    let mut found: Vec<HistoryEntry> = entries
        .into_iter()
        .filter(|entry| entry.drive_id == drive_id)
        .collect();
    found.reverse();
    found
}

/// Keeps only each drive's newest entries.
fn trimmed(entries: Vec<HistoryEntry>) -> Vec<HistoryEntry> {
    let mut kept = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let newer_for_drive = entries[index + 1..]
            .iter()
            .filter(|other| other.drive_id == entry.drive_id)
            .count();
        if newer_for_drive < ENTRIES_PER_DRIVE {
            kept.push(entry.clone());
        }
    }
    kept
}

/// Add an entry. Blocks on the disk, so call it off the GUI thread.
pub fn append(path: &Path, entry: HistoryEntry) -> io::Result<()> {
    let mut entries = read_all(path);
    entries.push(entry);
    let values: Vec<Value> = trimmed(entries)
        .iter()
        .map(|entry| {
            json!({
                "drive": entry.drive_id,
                "at": entry.finished_at,
                "name": entry.name,
                "outcome": entry.outcome,
            })
        })
        .collect();

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&values).map_err(io::Error::other)?;
    fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(drive_id: &str, finished_at: i64) -> HistoryEntry {
        HistoryEntry {
            drive_id: drive_id.to_string(),
            finished_at,
            name: "REPLICAS".to_string(),
            outcome: "Saved 1 title".to_string(),
        }
    }

    #[test]
    fn saved_entries_read_back_newest_first_for_their_drive() {
        let folder = TempDir::new().unwrap();
        let path = folder.path().join("state/history.json");
        append(&path, entry("A", 1)).unwrap();
        append(&path, entry("B", 2)).unwrap();
        append(&path, entry("A", 3)).unwrap();

        let found = for_drive(read_all(&path), "A");

        assert_eq!(found, vec![entry("A", 3), entry("A", 1)]);
    }

    #[test]
    fn keeps_only_the_newest_entries_of_each_drive() {
        let folder = TempDir::new().unwrap();
        let path = folder.path().join("history.json");
        append(&path, entry("B", 0)).unwrap();
        for at in 1..=(ENTRIES_PER_DRIVE as i64 + 5) {
            append(&path, entry("A", at)).unwrap();
        }

        let all = read_all(&path);
        let of_a = for_drive(all.clone(), "A");

        assert_eq!(of_a.len(), ENTRIES_PER_DRIVE);
        assert_eq!(of_a[0].finished_at, ENTRIES_PER_DRIVE as i64 + 5);
        assert_eq!(for_drive(all, "B").len(), 1);
    }

    #[test]
    fn a_missing_or_damaged_file_is_an_empty_history() {
        let folder = TempDir::new().unwrap();
        let path = folder.path().join("history.json");
        assert!(read_all(&path).is_empty());

        fs::write(&path, b"not json").unwrap();
        assert!(read_all(&path).is_empty());
        append(&path, entry("A", 1)).unwrap();
        assert_eq!(read_all(&path).len(), 1);
    }
}
