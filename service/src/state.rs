//! What the service holds: the phase of the comparison (idle, running,
//! failed, ready), the open comparison with everything derived from it, and
//! the views the screen has opened.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use cdr_engine::groups::{GroupRule, Groups};
use cdr_engine::pipeline::PipelineTimings;
use cdr_engine::view::{Order, Slot, View};
use cdr_engine::{CancelToken, Comparison, Progress};
use serde_json::{json, Value};

use crate::api::{conflict, ApiError};
use crate::review::Reviews;
use crate::Service;

/// A grouping rule with the pairs it leaves out.
pub(crate) type GroupKey = (GroupRule, Vec<u32>);

pub(crate) struct Ready {
    pub(crate) cmp: Comparison,
    /// Known once the background SHA-256 of both files has finished.
    pub(crate) id: RwLock<Option<String>>,
    pub(crate) timings: PipelineTimings,
    pub(crate) slots: HashMap<&'static str, Vec<Slot>>,
    /// Per pair: units where the value changed / differed only in an ignored way.
    pub(crate) pair_changed: Vec<u64>,
    pub(crate) pair_ignored: Vec<u64>,
    pub(crate) reviews: Mutex<Reviews>,
    /// Whether saved review states have been looked for (needs the ID).
    pub(crate) review_load: RwLock<ReviewLoad>,
    /// Groupings already computed, per rule and skipped pairs.
    pub(crate) groups: Mutex<HashMap<GroupKey, Arc<Groups>>>,
    /// Units fixed for a bulk change when it was previewed.
    pub(crate) targets: Mutex<HashMap<u64, Vec<u32>>>,
    /// Why the last write of the review states failed; cleared by a
    /// successful write.
    pub(crate) save_error: RwLock<Option<String>>,
    /// Why the review states of this comparison must not be written at all
    /// (a saved file that could not be read is still in place, another
    /// window has the same comparison open, the files could not be
    /// identified). Shown as the save error.
    pub(crate) save_blocked: RwLock<Option<String>>,
    /// The comparison's lock file, held while it is open.
    pub(crate) lock: Mutex<Option<std::fs::File>>,
}

#[derive(Clone, Default, serde::Serialize)]
pub(crate) struct ReviewLoad {
    pub(crate) ready: bool,
    /// Entries restored from an earlier session.
    pub(crate) restored: usize,
    /// States saved for the same two files under another comparison ID
    /// (the files or conditions changed); not used.
    pub(crate) stale_saved_at: Option<i64>,
    /// The saved file existed but could not be used; what happened to it.
    pub(crate) problem: Option<String>,
}

pub(crate) enum Phase {
    Idle,
    Running,
    Failed(Value),
    Ready(Arc<Ready>),
}

#[derive(Default)]
pub(crate) struct Views {
    pub(crate) next: u64,
    pub(crate) map: HashMap<u64, Arc<View>>,
    pub(crate) order: VecDeque<u64>,
}

pub(crate) struct Current {
    pub(crate) generation: u64,
    pub(crate) progress: Arc<Progress>,
    pub(crate) cancel: CancelToken,
    pub(crate) started: Instant,
}

/// The key of a table order in `Ready::slots`.
pub(crate) fn order_key(o: Order) -> &'static str {
    match o {
        Order::Aligned => "aligned",
        Order::LeftSource => "left_source",
        Order::RightSource => "right_source",
    }
}

impl Service {
    /// The open comparison, or why there is none.
    pub(crate) fn ready(&self) -> Result<Arc<Ready>, ApiError> {
        match &*self.phase.read().unwrap() {
            Phase::Ready(r) => Ok(r.clone()),
            Phase::Running => Err(conflict("比較の処理中です。")),
            Phase::Idle => Err(conflict("比較するファイルが選ばれていません。")),
            Phase::Failed(e) => {
                Err(ApiError { status: 409, body: json!({ "error": e["message"].clone(), "detail": e.clone() }) })
            }
        }
    }

    /// A view the screen opened earlier.
    pub(crate) fn view_by_id(&self, id: u64) -> Result<Arc<View>, ApiError> {
        self.views
            .lock()
            .unwrap()
            .map
            .get(&id)
            .cloned()
            .ok_or_else(|| conflict("表示の条件が古くなりました。表示し直してください。"))
    }
}
