//! Cancellation and progress counters shared between a worker and the UI.
//!
//! Progress values are real counts (bytes read, records indexed, units
//! compared). The engine never reports an estimated percentage.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

#[derive(Clone, Default, Debug)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Default, Debug)]
pub struct SideProgress {
    pub bytes_total: AtomicU64,
    pub bytes_read: AtomicU64,
    pub bytes_indexed: AtomicU64,
    pub records: AtomicU64,
    pub keys: AtomicU64,
}

/// Pipeline stage. Stored as `u8` so the UI thread can poll it lock-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Stage {
    Idle = 0,
    /// Reading, decoding and indexing both files (in parallel).
    Reading = 1,
    ValidatingKeys = 3,
    Comparing = 4,
    Patterns = 5,
    Done = 6,
    Failed = 7,
    Cancelled = 8,
}

impl Stage {
    fn from_u8(v: u8) -> Stage {
        match v {
            1 => Stage::Reading,
            3 => Stage::ValidatingKeys,
            4 => Stage::Comparing,
            5 => Stage::Patterns,
            6 => Stage::Done,
            7 => Stage::Failed,
            8 => Stage::Cancelled,
            _ => Stage::Idle,
        }
    }
}

#[derive(Default, Debug)]
pub struct Progress {
    stage: AtomicU8,
    pub left: SideProgress,
    pub right: SideProgress,
    pub units_total: AtomicU64,
    pub units_compared: AtomicU64,
}

impl Progress {
    pub fn set_stage(&self, stage: Stage) {
        self.stage.store(stage as u8, Ordering::Relaxed);
    }

    pub fn stage(&self) -> Stage {
        Stage::from_u8(self.stage.load(Ordering::Relaxed))
    }
}
