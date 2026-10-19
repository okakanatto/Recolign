//! Review states (未確認・確認済み・要調査) and notes, per difference record.
//!
//! States belong to a comparison: they are saved under its comparison ID,
//! which covers the SHA-256 of both files and every comparison condition,
//! and each entry is matched by its key values. So states are never carried
//! over to files whose content changed, or to different conditions (plan
//! 5.4, AC-10). Changes are undoable; the file is written in the background.
//!
//! - `state`: the states in memory, with undo and redo.
//! - `files`: the files they are saved in.
//! - `commands`: the commands the screen calls.

mod commands;
pub(crate) mod files;
pub(crate) mod state;

pub(crate) use state::{Entry, Reviews, Status};
