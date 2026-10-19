//! Review states in memory: the state and note of each difference record,
//! and the undo and redo history.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Unchecked,
    Checked,
    Investigate,
}

impl Status {
    pub fn code(self) -> u8 {
        match self {
            Status::Unchecked => 0,
            Status::Checked => 1,
            Status::Investigate => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Unchecked => "未確認",
            Status::Checked => "確認済み",
            Status::Investigate => "要調査",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub status: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub memo: String,
    /// When it was last changed (ms since 1970).
    pub at: i64,
}

impl Entry {
    fn is_default(&self) -> bool {
        self.status == Status::Unchecked && self.memo.is_empty()
    }
}

/// One undoable change: the entries of the units before and after.
struct Op {
    label: String,
    changes: Vec<(u32, Option<Entry>, Option<Entry>)>,
}

#[derive(Default)]
pub struct Reviews {
    entries: HashMap<u32, Entry>,
    undo: Vec<Op>,
    redo: Vec<Op>,
    /// Counts changes; compared with `saved` to know what is unwritten.
    pub version: u64,
    /// The version last written to the file.
    pub saved: u64,
}

/// What a change does to each unit.
pub struct Change {
    pub status: Option<Status>,
    pub memo: Option<String>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl Reviews {
    pub fn get(&self, unit: u32) -> Option<&Entry> {
        self.entries.get(&unit)
    }

    pub fn status(&self, unit: u32) -> Status {
        self.entries.get(&unit).map_or(Status::Unchecked, |e| e.status)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&u32, &Entry)> {
        self.entries.iter()
    }

    /// Applies `change` to `units` as one undoable step. Returns how many
    /// units actually changed.
    pub fn apply(&mut self, units: &[u32], change: &Change, label: String) -> usize {
        let at = now_ms();
        let mut changes = Vec::new();
        for &u in units {
            let before = self.entries.get(&u).cloned();
            let mut e = before.clone().unwrap_or_default();
            if let Some(s) = change.status {
                e.status = s;
            }
            if let Some(m) = &change.memo {
                e.memo = m.clone();
            }
            let after = if e.is_default() { None } else { Some(e) };
            let same = match (&before, &after) {
                (Some(b), Some(a)) => b.status == a.status && b.memo == a.memo,
                (None, None) => true,
                _ => false,
            };
            if same {
                continue;
            }
            let after = after.map(|mut a| {
                a.at = at;
                a
            });
            self.set(u, after.clone());
            changes.push((u, before, after));
        }
        let n = changes.len();
        if n > 0 {
            self.undo.push(Op { label, changes });
            self.redo.clear();
            self.version += 1;
        }
        n
    }

    /// Changes not yet written to the file.
    pub fn dirty(&self) -> bool {
        self.version != self.saved
    }

    /// Records that the state at `version` was written.
    pub fn mark_saved(&mut self, version: u64) {
        self.saved = self.saved.max(version);
    }

    fn set(&mut self, u: u32, e: Option<Entry>) {
        match e {
            Some(e) => {
                self.entries.insert(u, e);
            }
            None => {
                self.entries.remove(&u);
            }
        }
    }

    /// Reverts the last step. Returns its label and size.
    pub fn undo(&mut self) -> Option<(String, usize)> {
        let op = self.undo.pop()?;
        for (u, before, _) in &op.changes {
            self.set(*u, before.clone());
        }
        let r = (op.label.clone(), op.changes.len());
        self.redo.push(op);
        self.version += 1;
        Some(r)
    }

    pub fn redo(&mut self) -> Option<(String, usize)> {
        let op = self.redo.pop()?;
        for (u, _, after) in &op.changes {
            self.set(*u, after.clone());
        }
        let r = (op.label.clone(), op.changes.len());
        self.undo.push(op);
        self.version += 1;
        Some(r)
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|o| o.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|o| o.label.as_str())
    }

    /// Entries restored from a file (no undo step).
    pub fn restore(&mut self, entries: impl IntoIterator<Item = (u32, Entry)>) -> usize {
        let mut n = 0;
        for (u, e) in entries {
            if !e.is_default() {
                self.entries.insert(u, e);
                n += 1;
            }
        }
        n
    }
}
