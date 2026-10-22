//! Running a comparison: the file preview, the memory estimate, starting,
//! cancelling and dropping a comparison, and its progress.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use cdr_engine::pattern::aggregate;
use cdr_engine::pipeline::{comparison_id, run, CompareRequest};
use cdr_engine::preview::preview;
use cdr_engine::view::{slots, Order};
use cdr_engine::{sysmem, CancelToken, CompareError, LoadOptions, Progress, Table};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{bad, ms, parse, Reply};
use crate::crash;
use crate::review::Reviews;
use crate::state::{order_key, Current, Phase, Ready, ReviewLoad, Views};
use crate::Service;

#[derive(Deserialize)]
struct PreviewReq {
    path: PathBuf,
    #[serde(default)]
    options: Option<LoadOptions>,
}

#[derive(Deserialize)]
struct MemoryReq {
    left: PathBuf,
    right: PathBuf,
}

/// Memory a comparison is expected to need, from the file sizes and the
/// number of lines, and what the system has available (plan 9.5, U-13).
/// Measured peaks were 1.0 to 1.4 times the two files' size (G1, up to 1M
/// rows, every record changed) plus 70 to 90 bytes a record, which counts
/// most in files with short lines; the estimate adds a margin for the change
/// patterns and review states.
pub(crate) fn memory_check(left: &Path, right: &Path) -> Value {
    let size = |p: &Path| std::fs::metadata(p).ok().filter(|m| m.is_file()).map_or(0, |m| m.len());
    let (l, r) = (size(left), size(right));
    let input = l + r;
    let records = lines(left, l) + lines(right, r);
    let needed = input + input * 3 / 5 + records * 96 + (64 << 20);
    let available = sysmem::available();
    json!({
        "input_bytes": input,
        "needed_bytes": needed,
        "available_bytes": available,
        "enough": available.is_none_or(|a| needed <= a),
    })
}

/// Lines in a file of `size` bytes, estimated from 4 MB of it: 8 pieces
/// spread over the file, not only its start, which can have no line breaks
/// before a great many short lines.
fn lines(path: &Path, size: u64) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    const PIECES: u64 = 8;
    const PIECE: u64 = 512 << 10;
    if size == 0 {
        return 0;
    }
    let Ok(mut file) = std::fs::File::open(path) else { return 0 };
    let mut sample = Vec::new();
    for i in 0..PIECES {
        let at = size.saturating_sub(PIECE) * i / (PIECES - 1);
        if i > 0 && at < sample.len() as u64 {
            break; // a small file: read whole by the first piece
        }
        let len = if i == 0 && size <= PIECES * PIECE { size } else { PIECE };
        if file.seek(SeekFrom::Start(at)).and_then(|_| (&mut file).take(len).read_to_end(&mut sample)).is_err() {
            return 0;
        }
    }
    if sample.is_empty() {
        return 0;
    }
    let count = |b: u8| sample.iter().filter(|&&c| c == b).count() as u64;
    // Files with CR line ends have no LF.
    let n = count(b'\n').max(count(b'\r')).max(1);
    (n * size).div_ceil(sample.len() as u64)
}

/// Watches the memory left while a comparison runs, and cancels it before
/// allocations would fail: a failed allocation ends the process without a
/// word, while a stopped comparison says why and keeps the app usable.
struct MemoryGuard {
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Memory left when the guard tripped; `u64::MAX` while it has not.
    low: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MemoryGuard {
    fn start(floor: u64, growth: u64, cancel: CancelToken) -> MemoryGuard {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let low = Arc::new(AtomicU64::new(u64::MAX));
        let base = sysmem::current();
        let check = move || match sysmem::headroom() {
            Some(left) if left < floor && sysmem::current().saturating_sub(base) >= growth => Some(left),
            _ => None,
        };
        // The first check before the comparison starts, then every 50 ms.
        if let Some(left) = check() {
            low.store(left, Ordering::SeqCst);
            cancel.cancel();
            return MemoryGuard { stop, low, thread: None };
        }
        let (st, lo) = (stop.clone(), low.clone());
        let thread = std::thread::spawn(move || {
            while !st.load(Ordering::SeqCst) {
                if let Some(left) = check() {
                    lo.store(left, Ordering::SeqCst);
                    cancel.cancel();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        MemoryGuard { stop, low, thread: Some(thread) }
    }

    /// Stops watching. The memory left if the guard stopped the comparison.
    fn finish(mut self) -> Option<u64> {
        self.stop_thread();
        let v = self.low.load(Ordering::SeqCst);
        (v != u64::MAX).then_some(v)
    }

    fn stop_thread(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
    }
}

impl Drop for MemoryGuard {
    /// Also when the comparison panicked before `finish`.
    fn drop(&mut self) {
        self.stop_thread();
    }
}

/// The comparison as the screen shows it in the toolbar and the export.
pub(crate) fn summary(r: &Ready) -> Value {
    let c = &r.cmp;
    let side = |t: &Table| {
        json!({
            "name": t.info.name, "path": t.info.path, "bytes": t.info.bytes, "records": t.len(), "columns": t.ncols(),
            "encoding": t.info.encoding, "delimiter": t.info.delimiter, "quoted": t.info.quoted, "fixed": t.info.fixed,
            "layout": t.info.layout_label(), "bom": t.info.bom, "sha256": t.sha256(),
            "line_endings": t.info.line_endings, "has_header": t.info.has_header,
        })
    };
    let name = |k: u32| c.left.headers[c.mapping.pairs[k as usize].left as usize].clone();
    json!({
        "comparison_id": *r.id.read().unwrap(),
        "left": side(&c.left),
        "right": side(&c.right),
        "counts": c.counts,
        "keys": c.spec.keys.iter().map(|&k| name(k)).collect::<Vec<_>>(),
        "excluded": c.spec.excluded.iter().map(|&k| name(k)).collect::<Vec<_>>(),
        "trim": c.spec.trim,
        "left_only": c.mapping.left_only.iter().map(|&i| c.left.headers[i as usize].clone()).collect::<Vec<_>>(),
        "right_only": c.mapping.right_only.iter().map(|&i| c.right.headers[i as usize].clone()).collect::<Vec<_>>(),
        "pair_names": (0..c.mapping.pairs.len() as u32).map(name).collect::<Vec<_>>(),
        "pair_changed": r.pair_changed,
        "pair_ignored": r.pair_ignored,
        "timings_ms": { "total_to_usable": ms(r.timings.total), "load": ms(r.timings.load), "compare": ms(r.timings.compare) },
    })
}

fn error_value(e: &CompareError) -> Value {
    let detail = match e {
        CompareError::KeyValidation(r) => serde_json::to_value(r.as_ref()).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    json!({ "code": e.code(), "message": e.to_string(), "detail": detail })
}

impl Service {
    /// The first rows of a file and the read options guessed for it.
    pub(crate) fn preview_file(&self, args: &Value) -> Reply {
        let q: PreviewReq = parse(args)?;
        let p = preview(&q.path, q.options, 20).map_err(|e| bad(e.to_string()))?;
        Ok(serde_json::to_value(p).unwrap())
    }

    pub(crate) fn memory_check_cmd(&self, args: &Value) -> Reply {
        let q: MemoryReq = parse(args)?;
        Ok(memory_check(&q.left, &q.right))
    }

    pub(crate) fn cancel(&self) {
        self.current.lock().unwrap().cancel.cancel();
    }

    /// Starts a comparison in the background. A running one is cancelled.
    pub fn start(self: &Arc<Self>, req: CompareRequest) {
        self.flush().ok();
        let (generation, progress, cancel) = {
            let mut cur = self.current.lock().unwrap();
            cur.cancel.cancel();
            let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            *cur = Current {
                generation,
                progress: Arc::new(Progress::default()),
                cancel: CancelToken::new(),
                started: Instant::now(),
            };
            (generation, cur.progress.clone(), cur.cancel.clone())
        };
        *self.phase.write().unwrap() = Phase::Running;
        *self.patterns.write().unwrap() = None;
        *self.views.lock().unwrap() = Views::default();
        let me = self.clone();
        std::thread::Builder::new()
            .name("comparison".into())
            .spawn(move || {
                let work = std::panic::AssertUnwindSafe(|| me.run_comparison(&req, generation, &progress, &cancel));
                if std::panic::catch_unwind(work).is_err() {
                    me.after_crash(generation);
                }
            })
            .expect("a thread for the comparison");
    }

    /// Whether `generation` is still the comparison the screen waits for.
    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    /// The comparison itself (on its own thread): read and compare, then make
    /// the result available, aggregate the change patterns and look for saved
    /// review states.
    fn run_comparison(
        self: &Arc<Self>,
        req: &CompareRequest,
        generation: u64,
        progress: &Progress,
        cancel: &CancelToken,
    ) {
        let guard = MemoryGuard::start(
            self.memory_floor.load(Ordering::SeqCst),
            self.memory_growth.load(Ordering::SeqCst),
            cancel.clone(),
        );
        let result = run(req, cancel, progress);
        let low = guard.finish();
        // Stopped by the guard, or finished just as it tripped: the memory
        // is too low to go on (the cancel token is spent too).
        let result = match (result, low) {
            (Ok(_) | Err(CompareError::Cancelled), Some(available)) => Err(CompareError::OutOfMemory { available }),
            (r, _) => r,
        };
        let out = match result {
            Ok(out) => out,
            Err(e) => {
                if self.is_current(generation) {
                    *self.phase.write().unwrap() = Phase::Failed(error_value(&e));
                }
                return;
            }
        };
        let c = out.comparison;
        let slots = [Order::Aligned, Order::LeftSource, Order::RightSource]
            .into_iter()
            .map(|o| (order_key(o), slots(&c, o)))
            .collect();
        let (pair_changed, pair_ignored) = c.pair_change_counts();
        let ready = Arc::new(Ready {
            cmp: c,
            id: RwLock::new(None),
            timings: out.timings,
            slots,
            pair_changed,
            pair_ignored,
            reviews: Mutex::new(Reviews::default()),
            review_load: RwLock::new(ReviewLoad::default()),
            groups: Mutex::new(HashMap::new()),
            targets: Mutex::new(HashMap::new()),
            save_error: RwLock::new(None),
            save_blocked: RwLock::new(None),
            lock: Mutex::new(None),
        });
        if !self.is_current(generation) {
            return;
        }
        *self.patterns_error.write().unwrap() = None;
        *self.phase.write().unwrap() = Phase::Ready(ready.clone());
        if let Ok(p) = aggregate(&ready.cmp, cancel) {
            if self.is_current(generation) {
                *self.patterns.write().unwrap() = Some(Arc::new(p));
            }
        }
        let id = comparison_id(&ready.cmp);
        *ready.id.write().unwrap() = id.clone();
        if self.is_current(generation) {
            self.load_reviews(&ready, id.as_deref());
        }
    }

    /// A bug stopped the comparison thread. Whatever was still to come is
    /// reported instead of being waited for forever.
    pub(crate) fn after_crash(&self, generation: u64) {
        if !self.is_current(generation) {
            return;
        }
        let mut phase = self.phase.write().unwrap_or_else(|e| e.into_inner());
        match &*phase {
            Phase::Running => {
                *phase =
                    Phase::Failed(json!({ "code": "internal", "message": crash::message("比較"), "detail": null }));
            }
            Phase::Ready(r) => {
                if self.patterns.read().unwrap_or_else(|e| e.into_inner()).is_none() {
                    *self.patterns_error.write().unwrap_or_else(|e| e.into_inner()) =
                        Some(crash::message("変更パターンの集計"));
                }
                let mut load = r.review_load.write().unwrap_or_else(|e| e.into_inner());
                if !load.ready {
                    *load = ReviewLoad {
                        ready: true,
                        problem: Some(crash::message("前回の確認状況の読み込み")),
                        ..Default::default()
                    };
                    *r.save_blocked.write().unwrap_or_else(|e| e.into_inner()) =
                        Some(crash::message("前回の確認状況の読み込み"));
                }
            }
            _ => {}
        }
    }

    /// Drops the current comparison (back to choosing files).
    pub fn reset(&self) {
        self.flush().ok();
        let mut cur = self.current.lock().unwrap();
        cur.cancel.cancel();
        self.generation.fetch_add(1, Ordering::SeqCst);
        cur.generation = self.generation.load(Ordering::SeqCst);
        *self.phase.write().unwrap() = Phase::Idle;
        *self.patterns.write().unwrap() = None;
        *self.views.lock().unwrap() = Views::default();
    }

    /// Progress of the running comparison, or the result.
    pub fn status(&self) -> Value {
        use std::sync::atomic::Ordering::Relaxed;
        let cur = self.current.lock().unwrap();
        let p = &cur.progress;
        let rd = |s: &cdr_engine::SideProgress| {
            json!({
                "bytes_total": s.bytes_total.load(Relaxed), "bytes_read": s.bytes_read.load(Relaxed),
                "bytes_indexed": s.bytes_indexed.load(Relaxed), "records": s.records.load(Relaxed), "keys": s.keys.load(Relaxed),
            })
        };
        let phase = match &*self.phase.read().unwrap() {
            Phase::Idle => json!({ "state": "idle" }),
            Phase::Running => json!({ "state": "running" }),
            Phase::Failed(e) => json!({ "state": "failed", "error": e }),
            Phase::Ready(r) => json!({ "state": "ready", "summary": summary(r) }),
        };
        json!({
            "generation": cur.generation,
            "phase": phase,
            "stage": p.stage(),
            "left": rd(&p.left),
            "right": rd(&p.right),
            "units_total": p.units_total.load(Relaxed),
            "units_compared": p.units_compared.load(Relaxed),
            "patterns_ready": self.patterns.read().unwrap().is_some(),
            "elapsed_ms": ms(cur.started.elapsed()),
            "memory": sysmem::read(),
        })
    }
}
