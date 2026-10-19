//! The saved review states on disk: one file per comparison ID, an index of
//! the last comparison per pair of files, lock files, and the rules that
//! keep a write from ever destroying earlier states.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub(crate) use super::state::now_ms;
use super::state::{Entry, Status};

/// The saved form: entries are identified by key values, not by position.
#[derive(Serialize, Deserialize)]
pub struct SavedFile {
    pub version: u32,
    pub comparison_id: String,
    pub left: SavedSide,
    pub right: SavedSide,
    pub keys: Vec<String>,
    pub saved_at: i64,
    pub entries: Vec<SavedEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedSide {
    pub path: Option<PathBuf>,
    pub name: String,
    pub sha256: Option<String>,
}

/// One entry: the key values and the `Entry` fields side by side. (Written
/// out rather than flattened, which is much slower to read.)
#[derive(Serialize, Deserialize)]
pub struct SavedEntry {
    pub key: Vec<String>,
    pub status: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub memo: String,
    pub at: i64,
}

impl SavedEntry {
    pub fn new(key: Vec<String>, e: Entry) -> Self {
        SavedEntry { key, status: e.status, memo: e.memo, at: e.at }
    }

    pub fn entry(self) -> (Vec<String>, Entry) {
        (self.key, Entry { status: self.status, memo: self.memo, at: self.at })
    }
}

/// Which comparison was last saved for a pair of files, to tell the user
/// when saved states could not be used because the files changed.
#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    /// "left path\nright path" → (comparison ID, saved at).
    pub pairs: HashMap<String, (String, i64)>,
}

pub fn file_for(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

pub fn index_file(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

pub fn pair_key(left: &Option<PathBuf>, right: &Option<PathBuf>) -> Option<String> {
    Some(format!("{}\n{}", left.as_ref()?.display(), right.as_ref()?.display()))
}

/// Writes `data` to `path` through a temporary file that is flushed to the
/// disk first, so a crash or a full disk never leaves a half-written file
/// in place of the previous one.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.part");
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(data)?;
        f.sync_all()
    });
    if let Err(e) = written {
        std::fs::remove_file(&tmp).ok();
        return Err(e);
    }
    std::fs::rename(&tmp, path)
}

/// Version of the saved format this build reads and writes.
pub const FORMAT: u32 = 1;

/// Why a saved file could not be used.
pub enum LoadProblem {
    /// It exists but could not be read (another program holds it, access
    /// denied). It may be fine: it is left alone and nothing is saved over it.
    Unreadable(std::io::Error),
    /// Not JSON of the expected shape (damaged or edited).
    Damaged,
    /// Written by a newer version of the app.
    Newer,
}

/// Reads a saved file. `Ok(None)` when there is none.
pub fn read_saved(path: &Path) -> Result<Option<SavedFile>, LoadProblem> {
    let text = match std::fs::read(path) {
        Ok(t) => t,
        // No file, or no folder yet (or a file where the folder should be,
        // which the first write reports): nothing saved.
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
            return Ok(None)
        }
        Err(e) => return Err(LoadProblem::Unreadable(e)),
    };
    // The version first (other fields skipped), so a newer format is told
    // apart from a damaged file.
    #[derive(Deserialize)]
    struct Head {
        version: u32,
    }
    let head: Head = serde_json::from_slice(&text).map_err(|_| LoadProblem::Damaged)?;
    if head.version > FORMAT {
        return Err(LoadProblem::Newer);
    }
    serde_json::from_slice(&text).map(Some).map_err(|_| LoadProblem::Damaged)
}

/// The lock file of a comparison: held while the comparison is open, so a
/// second window with the same files and conditions does not write over the
/// first one's states. The OS releases it when the process ends, even after
/// a crash, so it never goes stale.
pub fn lock_file(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.lock"))
}

/// Takes the lock. `Ok(None)` when another process holds it.
pub fn try_hold(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Whether `name` is one of the files this module writes: `index.json`,
/// `<ID>.json`, `<ID>.lock`, `<ID>.unreadable-<time>.json`, or the `.part`
/// file of a write in progress. Nothing else in the folder is touched.
pub fn is_ours(name: &str) -> bool {
    let name = name.strip_suffix(".part").unwrap_or(name);
    if name == "index.json" {
        return true;
    }
    let Some((id, rest)) = name.split_once('.') else { return false };
    let id_ok = id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit());
    let set_aside = rest
        .strip_prefix("unreadable-")
        .and_then(|r| r.strip_suffix(".json"))
        .is_some_and(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()));
    id_ok && (rest == "json" || rest == "lock" || set_aside)
}

/// Moves a file that could not be used out of the way, keeping it for the
/// user. Returns where it went.
pub fn set_aside(path: &Path) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let to = path.with_file_name(format!("{stem}.unreadable-{}.json", now_ms()));
    std::fs::rename(path, &to).ok().map(|_| to)
}

#[cfg(test)]
mod tests {
    use super::is_ours;

    #[test]
    fn only_the_files_this_module_writes_are_ours() {
        let id = "a".repeat(64);
        for name in [
            "index.json".to_string(),
            "index.json.part".to_string(),
            format!("{id}.json"),
            format!("{id}.json.part"),
            format!("{id}.lock"),
            format!("{id}.unreadable-1791912281000.json"),
        ] {
            assert!(is_ours(&name), "{name}");
        }
        for name in [
            "index.backup.json".to_string(),
            "notes.json".to_string(),
            format!("{id}.txt"),
            format!("{id}.old.json"),
            format!("{id}.unreadable-.json"),
            format!("{}.json", "g".repeat(64)),
            "a.json".to_string(),
        ] {
            assert!(!is_ours(&name), "{name}");
        }
    }
}
