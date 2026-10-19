//! The review-state commands: loading the saved states of a comparison,
//! writing them in the background, marking records (one or many, fixed
//! first and then applied), undo and redo, and the saved files' upkeep.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use cdr_engine::Table;
use serde::Deserialize;
use serde_json::{json, Value};

use super::files;
use super::state::{Change, Status};
use crate::api::{bad, conflict, parse, Reply};
use crate::crash;
use crate::groups::GroupRef;
use crate::state::{Phase, Ready, ReviewLoad};
use crate::Service;

#[derive(Deserialize)]
struct NextReviewReq {
    view: u64,
    from: Option<usize>,
    forward: bool,
    status: Vec<Status>,
}

/// Records for a bulk review change: rows of a view, listed units, or a group.
#[derive(Deserialize)]
struct TargetReq {
    #[serde(default)]
    view: Option<u64>,
    #[serde(default)]
    start: usize,
    #[serde(default)]
    end: Option<usize>,
    #[serde(default)]
    units: Option<Vec<u32>>,
    #[serde(default)]
    group: Option<GroupRef>,
    /// Every record that has a review state or a note.
    #[serde(default)]
    reviewed: bool,
}

#[derive(Deserialize)]
struct SetReq {
    /// From `review_target`: the records fixed when the change was shown.
    #[serde(default)]
    token: Option<u64>,
    #[serde(default)]
    units: Option<Vec<u32>>,
    #[serde(default)]
    status: Option<Status>,
    #[serde(default)]
    memo: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

impl Service {
    /// Restores states saved for this comparison ID, and notes states saved
    /// for the same two files under another ID (not used). A saved file that
    /// cannot be used is moved aside and kept, never overwritten; if it
    /// cannot be moved, or cannot be read at all, nothing is saved for this
    /// comparison.
    pub(crate) fn load_reviews(&self, r: &Ready, id: Option<&str>) {
        let mut load = ReviewLoad { ready: true, ..Default::default() };
        let block = |why: String| *r.save_blocked.write().unwrap() = Some(why);
        match (&self.data_dir, id) {
            (Some(_), None) => block(
                "比較するファイルを識別できなかったため（SHA-256を計算できませんでした）、確認状況を保存できません。"
                    .into(),
            ),
            (Some(dir), Some(id)) => {
                // A folder that cannot be used now is reported by the first
                // write; the lock is taken again before each write.
                Self::hold_lock(r, dir, id);
                let path = files::file_for(dir, id);
                match files::read_saved(&path) {
                    Ok(Some(saved)) => {
                        let entries = saved.entries.into_iter().filter_map(|e| {
                            let (key, entry) = e.entry();
                            r.cmp.find_key(&key).map(|u| (u, entry))
                        });
                        load.restored = r.reviews.lock().unwrap().restore(entries);
                    }
                    Ok(None) => {}
                    Err(files::LoadProblem::Unreadable(e)) => {
                        load.problem = Some(format!(
                            "前回の確認状況のファイルを読めませんでした（{e}）。ほかのプログラムが使っている可能性があります。ファイルを上書きしないよう、この比較の確認状況は保存しません。アプリを開き直すと、もう一度読み込みます。"
                        ));
                        block(format!(
                            "前回の確認状況のファイル（{}）を読めなかったため、上書きしないよう保存を止めています。",
                            path.display()
                        ));
                    }
                    Err(problem) => {
                        let what = match problem {
                            files::LoadProblem::Newer => {
                                "前回の確認状況は新しい版のアプリで保存されたため、この版では読み込めません。"
                            }
                            _ => "前回の確認状況のファイルが壊れているため、読み込めませんでした。",
                        };
                        load.problem = Some(match files::set_aside(&path) {
                            Some(to) => format!(
                                "{what}ファイルは {} に残しました。確認状況は空の状態から始めます。",
                                to.display()
                            ),
                            None => {
                                block(format!("前回の確認状況のファイル（{}）を移せなかったため、上書きしないよう保存を止めています。", path.display()));
                                format!(
                                    "{what}ファイル（{}）は残してあり、この比較の確認状況は保存しません。",
                                    path.display()
                                )
                            }
                        });
                    }
                }
                let index: files::Index = std::fs::read_to_string(files::index_file(dir))
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default();
                if let Some(pk) = files::pair_key(&r.cmp.left.info.path, &r.cmp.right.info.path) {
                    if let Some((other, at)) = index.pairs.get(&pk) {
                        if other != id && files::file_for(dir, other).exists() {
                            load.stale_saved_at = Some(*at);
                        }
                    }
                }
            }
            (None, _) => {}
        }
        *r.review_load.write().unwrap() = load;
    }

    /// Takes the comparison's lock if this process does not hold it yet. If
    /// another process holds it, saving is blocked for this comparison.
    fn hold_lock(r: &Ready, dir: &Path, id: &str) {
        let mut held = r.lock.lock().unwrap();
        if held.is_some() {
            return;
        }
        match files::try_hold(&files::lock_file(dir, id)) {
            Ok(Some(f)) => *held = Some(f),
            Ok(None) => {
                *r.save_blocked.write().unwrap() = Some(
                    "同じファイルと条件の比較を、別のウィンドウでも開いています。確認状況の保存は先に開いたウィンドウだけで行い、このウィンドウで付けた内容は保存しません。".into(),
                )
            }
            Err(_) => {} // the write that follows reports the folder
        }
    }

    /// Starts the thread that writes changed review states (once).
    pub(crate) fn ensure_saver(self: &Arc<Self>) {
        if self.data_dir.is_none() {
            return;
        }
        self.saver.get_or_init(|| {
            let weak = Arc::downgrade(self);
            std::thread::Builder::new()
                .name("review saver".into())
                .spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_millis(700));
                    let Some(me) = weak.upgrade() else { break };
                    // A bug in one write must not end the saving for good: it
                    // is reported as the save error, and the next change tries again.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| me.write_reviews(false))).is_err() {
                        if let Ok(r) = me.ready() {
                            *r.save_error.write().unwrap_or_else(|e| e.into_inner()) =
                                Some(crash::message("確認状況の保存"));
                        }
                    }
                })
                .expect("a thread for saving review states");
        });
    }

    /// Writes the review states if they changed since the last write.
    /// Returns why they could not be written, if some are still unsaved.
    pub fn flush(&self) -> Result<(), String> {
        // Called when the window closes: a bug here must not end the app.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.write_reviews(true))).is_err() {
            return Err(crash::message("確認状況の保存"));
        }
        let r = match &*self.phase.read().unwrap() {
            Phase::Ready(r) => r.clone(),
            _ => return Ok(()),
        };
        if self.data_dir.is_none() || !r.reviews.lock().unwrap().dirty() {
            return Ok(());
        }
        let e = r.save_error.read().unwrap().clone();
        Err(e.unwrap_or_else(|| "確認状況を保存できません。".to_string()))
    }

    /// Writes the review states if they changed. The states are copied
    /// under the lock and written without it, so marking records never
    /// waits for the disk. After a failure, the background writer waits a
    /// few seconds before trying again; `force` tries at once.
    fn write_reviews(&self, force: bool) {
        let Some(dir) = &self.data_dir else { return };
        // (A lock left by a bug in an earlier write is taken over, not fatal.)
        let mut failed_at = self.save.lock().unwrap_or_else(|e| e.into_inner());
        if !force && failed_at.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(5)) {
            return;
        }
        let r = match &*self.phase.read().unwrap() {
            Phase::Ready(r) => r.clone(),
            _ => return,
        };
        let Some(id) = r.id.read().unwrap().clone() else { return };
        Self::hold_lock(&r, dir, &id);
        if let Some(why) = r.save_blocked.read().unwrap().clone() {
            if r.reviews.lock().unwrap().dirty() {
                *r.save_error.write().unwrap() = Some(why);
            }
            return;
        }
        let (version, entries) = {
            let rv = r.reviews.lock().unwrap();
            if !rv.dirty() {
                return;
            }
            (rv.version, rv.entries().map(|(&u, e)| (u, e.clone())).collect::<Vec<_>>())
        };
        let c = &r.cmp;
        let side =
            |t: &Table| files::SavedSide { path: t.info.path.clone(), name: t.info.name.clone(), sha256: t.sha256() };
        let name = |k: u32| c.left.headers[c.mapping.pairs[k as usize].left as usize].clone();
        let saved_at = files::now_ms();
        let file = files::SavedFile {
            version: files::FORMAT,
            comparison_id: id.clone(),
            left: side(&c.left),
            right: side(&c.right),
            keys: c.spec.keys.iter().map(|&k| name(k)).collect(),
            saved_at,
            entries: entries
                .into_iter()
                .map(|(u, entry)| files::SavedEntry::new(c.key_values(u as usize), entry))
                .collect(),
        };
        let path = files::file_for(dir, &id);
        let result = if file.entries.is_empty() {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            serde_json::to_vec(&file).map_err(std::io::Error::other).and_then(|data| files::write_atomic(&path, &data))
        };
        if let Err(e) = result {
            *failed_at = Some(Instant::now());
            *r.save_error.write().unwrap() = Some(format!(
                "確認状況を保存できません（{e}）。保存先のフォルダ（{}）に書き込めるか、ディスクの空きがあるかを確かめてください。保存できるまで、変更はアプリを閉じると失われます。",
                dir.display()
            ));
            return;
        }
        *failed_at = None;
        *r.save_error.write().unwrap() = None;
        r.reviews.lock().unwrap().mark_saved(version);
        if let Some(pk) = files::pair_key(&c.left.info.path, &c.right.info.path) {
            let ip = files::index_file(dir);
            let mut index: files::Index =
                std::fs::read_to_string(&ip).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            if file.entries.is_empty() {
                index.pairs.remove(&pk);
            } else {
                index.pairs.insert(pk, (id, saved_at));
            }
            if let Ok(data) = serde_json::to_vec(&index) {
                files::write_atomic(&ip, &data).ok();
            }
        }
    }

    /// The files this app saved review states in: how many comparisons and
    /// bytes, or (with `clear`) deletes them. Only the names the app writes
    /// are touched, and a comparison open in another window is skipped.
    fn review_store(&self, clear: bool) -> Value {
        let Some(dir) = &self.data_dir else {
            return json!({ "comparisons": 0, "set_aside": 0, "bytes": 0, "deleted": 0, "failed": 0, "in_use": 0 });
        };
        let _w = self.save.lock().unwrap_or_else(|e| e.into_inner());
        let (mut comparisons, mut set_aside, mut bytes) = (0u64, 0u64, 0u64);
        let (mut deleted, mut failed, mut in_use) = (0u64, 0u64, 0u64);
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !files::is_ours(&name) || !e.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let id = name.split('.').next().unwrap_or("");
            let saved = name.ends_with(".json") && name != "index.json" && !name.contains(".unreadable-");
            if saved {
                comparisons += 1;
            } else if name.contains(".unreadable-") {
                set_aside += 1;
            }
            bytes += e.metadata().map_or(0, |m| m.len());
            if !clear || name.ends_with(".lock") {
                continue;
            }
            // A comparison held open by another window keeps its states.
            if saved && matches!(files::try_hold(&files::lock_file(dir, id)), Ok(None)) {
                in_use += 1;
                continue;
            }
            match std::fs::remove_file(e.path()) {
                Ok(()) => deleted += 1,
                Err(_) => failed += 1,
            }
        }
        if clear {
            // Lock files nobody holds are removed last.
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".lock") && files::is_ours(&name) && matches!(files::try_hold(&e.path()), Ok(Some(_)))
                {
                    std::fs::remove_file(e.path()).ok();
                }
            }
        }
        json!({ "comparisons": comparisons, "set_aside": set_aside, "bytes": bytes, "deleted": deleted, "failed": failed, "in_use": in_use })
    }

    pub(crate) fn review_summary(&self, r: &Ready) -> Value {
        let rv = r.reviews.lock().unwrap();
        let total = r.cmp.diff_units.len();
        let (mut checked, mut investigate, mut memos) = (0usize, 0usize, 0usize);
        for (&u, e) in rv.entries() {
            if r.cmp.units[u as usize].kind == cdr_engine::Kind::Same {
                continue;
            }
            match e.status {
                Status::Checked => checked += 1,
                Status::Investigate => investigate += 1,
                Status::Unchecked => {}
            }
            if !e.memo.is_empty() {
                memos += 1;
            }
        }
        let load = r.review_load.read().unwrap().clone();
        json!({
            "ready": load.ready,
            "saved": self.data_dir.is_some(),
            "restored": load.restored,
            "stale_saved_at": load.stale_saved_at,
            "load_problem": load.problem,
            "save_error": r.save_error.read().unwrap().clone(),
            "unsaved": rv.dirty() && self.data_dir.is_some(),
            "total": total,
            "checked": checked,
            "investigate": investigate,
            "unchecked": total - checked - investigate,
            "memos": memos,
            "undo": rv.undo_label(),
            "redo": rv.redo_label(),
        })
    }

    /// Fixes the records of a bulk change and describes them, so the user
    /// sees exactly what the change will apply to.
    pub(crate) fn review_target(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: TargetReq = parse(args)?;
        let mut units: Vec<u32> = if let Some(u) = q.units {
            u
        } else if let Some(g) = q.group {
            let groups = self.groups(&r, g.rule, &g.skip)?;
            let grp = groups.list.get(g.id as usize).ok_or_else(|| conflict("その変更パターンはありません。"))?;
            groups.units(grp).to_vec()
        } else if let Some(id) = q.view {
            let v = self.view_by_id(id)?;
            v.units_of_rows(&r.cmp, q.start, q.end.unwrap_or(usize::MAX))
        } else if q.reviewed {
            let mut u: Vec<u32> = r.reviews.lock().unwrap().entries().map(|(&u, _)| u).collect();
            u.sort_unstable();
            u
        } else {
            return Err(bad("対象のレコードを指定してください。"));
        };
        let all = units.len();
        let mut seen = std::collections::HashSet::new();
        units.retain(|&u| {
            (u as usize) < r.cmp.units.len() && r.cmp.units[u as usize].kind != cdr_engine::Kind::Same && seen.insert(u)
        });
        let c = &r.cmp;
        let (mut changed, mut added, mut deleted) = (0, 0, 0);
        let mut cols: HashMap<u32, usize> = HashMap::new();
        let rv = r.reviews.lock().unwrap();
        let (mut checked, mut investigate, mut memos) = (0, 0, 0);
        for &u in &units {
            if rv.get(u).is_some_and(|e| !e.memo.is_empty()) {
                memos += 1;
            }
            match c.units[u as usize].kind {
                cdr_engine::Kind::Changed => changed += 1,
                cdr_engine::Kind::Added => added += 1,
                cdr_engine::Kind::Deleted => deleted += 1,
                cdr_engine::Kind::Same => {}
            }
            for k in c.changed_pairs(u as usize) {
                *cols.entry(k).or_default() += 1;
            }
            match rv.status(u) {
                Status::Checked => checked += 1,
                Status::Investigate => investigate += 1,
                Status::Unchecked => {}
            }
        }
        drop(rv);
        let mut cols: Vec<(u32, usize)> = cols.into_iter().collect();
        cols.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let name = |k: u32| c.left.headers[c.mapping.pairs[k as usize].left as usize].clone();
        let token = self.next_target.fetch_add(1, Ordering::SeqCst);
        let count = units.len();
        let mut t = r.targets.lock().unwrap();
        if t.len() > 16 {
            t.clear();
        }
        t.insert(token, units);
        Ok(json!({
            "token": token,
            "count": count,
            "skipped_same": all - count,
            "kinds": { "changed": changed, "added": added, "deleted": deleted },
            "columns": cols.iter().take(40).map(|&(k, n)| json!({ "name": name(k), "count": n })).collect::<Vec<_>>(),
            "columns_total": cols.len(),
            "already": { "checked": checked, "investigate": investigate, "memos": memos },
        }))
    }

    pub(crate) fn review_set(self: &Arc<Self>, args: &Value) -> Reply {
        let r = self.ready()?;
        if !r.review_load.read().unwrap().ready {
            return Err(conflict("前回の確認状況を読み込んでいます。少し待ってから操作してください。"));
        }
        let q: SetReq = parse(args)?;
        let units = match (q.token, q.units) {
            (Some(t), _) => r
                .targets
                .lock()
                .unwrap()
                .remove(&t)
                .ok_or_else(|| conflict("対象の確認が古くなりました。もう一度操作してください。"))?,
            (None, Some(u)) => u,
            _ => return Err(bad("対象のレコードを指定してください。")),
        };
        if units
            .iter()
            .any(|&u| (u as usize) >= r.cmp.units.len() || r.cmp.units[u as usize].kind == cdr_engine::Kind::Same)
        {
            return Err(bad("一致しているレコードには確認状況を付けません。"));
        }
        if q.status.is_none() && q.memo.is_none() {
            return Err(bad("変える内容を指定してください。"));
        }
        let label = q.label.unwrap_or_else(|| match (q.status, &q.memo) {
            (Some(s), _) => format!("{}件を{}にする", units.len(), s.label()),
            (None, _) => "メモを変える".to_string(),
        });
        let n = r.reviews.lock().unwrap().apply(&units, &Change { status: q.status, memo: q.memo }, label);
        self.ensure_saver();
        let mut s = self.review_summary(&r);
        s["changed"] = json!(n);
        Ok(s)
    }

    /// Writes pending states now (the screen asks before leaving).
    pub(crate) fn review_save(&self) -> Reply {
        let r = self.ready()?;
        self.flush().map_err(bad)?;
        Ok(self.review_summary(&r))
    }

    pub(crate) fn review_summary_cmd(&self) -> Reply {
        let r = self.ready()?;
        Ok(self.review_summary(&r))
    }

    /// Undoes or redoes the last change.
    pub(crate) fn review_undo_redo(self: &Arc<Self>, redo: bool) -> Reply {
        let r = self.ready()?;
        let done = {
            let mut rv = r.reviews.lock().unwrap();
            if redo {
                rv.redo()
            } else {
                rv.undo()
            }
        };
        self.ensure_saver();
        let mut s = self.review_summary(&r);
        s["done"] = json!(done.map(|(label, n)| json!({ "label": label, "count": n })));
        Ok(s)
    }

    /// The next (or previous) difference in the given review states.
    pub(crate) fn next_review(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: NextReviewReq = parse(args)?;
        let v = self.view_by_id(q.view)?;
        let rv = r.reviews.lock().unwrap();
        Ok(json!({ "row": v.next_diff_where(&r.cmp, q.from, q.forward, |u| q.status.contains(&rv.status(u))) }))
    }

    /// How many comparisons have saved states (`info`), or deletes them (`clear`).
    pub(crate) fn review_store_cmd(&self, args: &Value) -> Reply {
        #[derive(Deserialize)]
        struct Q {
            action: String,
        }
        let q: Q = parse(args)?;
        match q.action.as_str() {
            "info" => Ok(self.review_store(false)),
            "clear" => {
                if matches!(&*self.phase.read().unwrap(), Phase::Ready(_) | Phase::Running) {
                    return Err(conflict("比較を閉じてから削除してください。"));
                }
                Ok(self.review_store(true))
            }
            _ => Err(bad("action は info か clear です。")),
        }
    }
}
