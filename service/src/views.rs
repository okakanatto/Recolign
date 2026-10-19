//! Views of the open comparison (an order, filters, a sort, optionally one
//! change pattern or some review states), and what the screen reads from
//! them: rows, the next difference, a record's detail, searches, copying.

use std::sync::Arc;
use std::time::Instant;

use cdr_engine::view::{detail, pattern_summaries, CopyRequest, Direction, Filter, Order, RangeRequest, Sort, View};
use cdr_engine::{CancelToken, Side};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{bad, conflict, default_count, ms, parse, yes, ApiError, Reply};
use crate::groups::GroupRef;
use crate::review::Status;
use crate::state::order_key;
use crate::Service;

#[derive(Deserialize)]
struct ViewReq {
    order: Order,
    #[serde(default)]
    filter: Filter,
    #[serde(default)]
    sort: Sort,
    /// Only the records of one group.
    #[serde(default)]
    group: Option<GroupRef>,
    /// Only difference records in these review states.
    #[serde(default)]
    review: Option<Vec<Status>>,
}

#[derive(Deserialize)]
struct RowsReq {
    view: u64,
    #[serde(flatten)]
    range: RangeRequest,
}

#[derive(Deserialize)]
struct NextReq {
    view: u64,
    from: Option<usize>,
    forward: bool,
}

#[derive(Deserialize)]
struct EdgeReq {
    view: u64,
    /// The side whose values the pane shows.
    side: Side,
    row: usize,
    /// An index into `slots`.
    col: usize,
    dir: Direction,
    /// The pane's slots in display order.
    slots: Vec<usize>,
}

#[derive(Deserialize)]
struct LocateReq {
    view: u64,
    unit: u32,
}

#[derive(Deserialize)]
struct DetailReq {
    unit: u32,
}

#[derive(Deserialize)]
struct FindReq {
    #[serde(default)]
    key: Option<Vec<String>>,
    #[serde(default)]
    side: Option<Side>,
    #[serde(default)]
    line: Option<u64>,
}

#[derive(Deserialize)]
struct PatternsReq {
    #[serde(default)]
    start: usize,
    #[serde(default = "default_count")]
    count: usize,
}

#[derive(Deserialize)]
struct CopyReq {
    view: u64,
    #[serde(flatten)]
    copy: CopyRequest,
}

#[derive(Deserialize)]
struct FindTextReq {
    view: u64,
    text: String,
    #[serde(default)]
    from: Option<usize>,
    #[serde(default = "yes")]
    forward: bool,
    /// Also count the matching rows (a pass over the whole view).
    #[serde(default)]
    count: bool,
}

impl Service {
    /// Builds a view and returns its number, size and columns.
    pub(crate) fn view(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let v: ViewReq = parse(args)?;
        let patterns = self.patterns.read().unwrap().clone();
        if v.filter.pattern.is_some() && patterns.is_none() {
            return Err(conflict("変更パターンを集計中です。"));
        }
        let t = Instant::now();
        // Conditions the engine does not know: a group, review states.
        let members: Option<Vec<bool>> = match v.group {
            Some(g) => {
                let groups = self.groups(&r, g.rule, &g.skip)?;
                let grp = groups.list.get(g.id as usize).ok_or_else(|| conflict("その変更パターンはありません。"))?;
                let mut m = vec![false; r.cmp.units.len()];
                for &u in groups.units(grp) {
                    m[u as usize] = true;
                }
                Some(m)
            }
            None => None,
        };
        let review_mask: Option<Vec<bool>> = v.review.as_ref().map(|want| {
            let rv = r.reviews.lock().unwrap();
            r.cmp
                .units
                .iter()
                .enumerate()
                .map(|(u, unit)| unit.kind != cdr_engine::Kind::Same && want.contains(&rv.status(u as u32)))
                .collect()
        });
        let keep = |u: u32| {
            members.as_ref().is_none_or(|m| m[u as usize]) && review_mask.as_ref().is_none_or(|m| m[u as usize])
        };
        let keep_ref: Option<&(dyn Fn(u32) -> bool + Sync)> =
            if members.is_some() || review_mask.is_some() { Some(&keep) } else { None };
        let view =
            View::build_with(&r.cmp, patterns.as_deref(), v.order, v.filter, v.sort, keep_ref, &CancelToken::new())
                .map(Arc::new)
                .map_err(|e| bad(e.to_string()))?;
        let build_ms = ms(t.elapsed());
        let mut vs = self.views.lock().unwrap();
        vs.next += 1;
        let id = vs.next;
        vs.map.insert(id, view.clone());
        vs.order.push_back(id);
        while vs.order.len() > 32 {
            let old = vs.order.pop_front().unwrap();
            vs.map.remove(&old);
        }
        Ok(json!({
            "view": id,
            "total": view.len(),
            "diff_count": view.diff_count(),
            "build_ms": build_ms,
            "slots": r.slots[order_key(view.order)],
        }))
    }

    /// A range of rows (and columns) of a view, with their review states.
    pub(crate) fn rows(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: RowsReq = parse(args)?;
        let view = self.view_by_id(q.view)?;
        let patterns = self.patterns.read().unwrap().clone();
        let t = Instant::now();
        let resp = view.range(&r.cmp, patterns.as_deref(), &r.slots[order_key(view.order)], &q.range);
        let mut v = serde_json::to_value(resp).unwrap();
        {
            let rv = r.reviews.lock().unwrap();
            if let Some(rows) = v["rows"].as_array_mut() {
                for row in rows {
                    let u = row["unit"].as_u64().unwrap_or(0) as u32;
                    if let Some(e) = rv.get(u) {
                        row["review"] = json!(e.status.code());
                        if !e.memo.is_empty() {
                            row["memo"] = json!(true);
                        }
                    }
                }
            }
        }
        v["engine_ms"] = json!(ms(t.elapsed()));
        Ok(v)
    }

    /// The next (or previous) difference from a row.
    pub(crate) fn next(&self, args: &Value) -> Reply {
        let q: NextReq = parse(args)?;
        let v = self.view_by_id(q.view)?;
        Ok(json!({ "row": v.next_diff(q.from, q.forward) }))
    }

    /// Where Ctrl+arrow moves the cell cursor to, as in a spreadsheet.
    pub(crate) fn edge(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: EdgeReq = parse(args)?;
        let v = self.view_by_id(q.view)?;
        let all = &r.slots[order_key(v.order)];
        if q.slots.iter().any(|&i| i >= all.len()) {
            return Err(bad("表の列の指定が正しくありません。"));
        }
        let (row, col) = v.edge(&r.cmp, all, &q.slots, q.side, (q.row, q.col), q.dir);
        Ok(json!({ "row": row, "col": col }))
    }

    /// The row of a record in a view.
    pub(crate) fn locate(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: LocateReq = parse(args)?;
        let v = self.view_by_id(q.view)?;
        Ok(json!({ "row": v.row_of_unit(&r.cmp, q.unit) }))
    }

    /// Everything about one record: its cells on both sides, the original
    /// text, its change pattern and review state.
    pub(crate) fn detail(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: DetailReq = parse(args)?;
        let patterns = self.patterns.read().unwrap().clone();
        let mut d = detail(&r.cmp, patterns.as_deref(), q.unit)
            .map(|d| serde_json::to_value(d).unwrap())
            .ok_or_else(|| bad("指定したレコードがありません。"))?;
        let e = r.reviews.lock().unwrap().get(q.unit).cloned().unwrap_or_default();
        d["review"] = json!({ "status": e.status, "memo": e.memo, "at": e.at });
        Ok(d)
    }

    /// The record with a key, or at a line of one file.
    pub(crate) fn find(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: FindReq = parse(args)?;
        if let Some(key) = q.key {
            return Ok(json!({ "unit": r.cmp.find_key(&key) }));
        }
        let (Some(side), Some(line)) = (q.side, q.line) else {
            return Err(bad("キー、または左右と行番号を指定してください。"));
        };
        let unit = match side {
            Side::Left => r.cmp.left.record_at_line(line).map(|x| x as u32),
            Side::Right => r.cmp.right.record_at_line(line).map(|x| r.cmp.right_unit[x]),
        };
        Ok(json!({ "unit": unit }))
    }

    /// The next row containing a text, and how many rows contain it.
    pub(crate) fn find_text(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: FindTextReq = parse(args)?;
        let v = self.view_by_id(q.view)?;
        let hit = v.find_text(&r.cmp, &r.slots[order_key(v.order)], &q.text, q.from, q.forward);
        let count = q.count.then(|| v.count_text(&r.cmp, &q.text));
        Ok(json!({ "hit": hit, "count": count }))
    }

    /// A page of the full change patterns.
    pub(crate) fn patterns(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: PatternsReq = parse(args)?;
        match self.patterns.read().unwrap().clone() {
            Some(p) => Ok(json!({
                "total": p.list.len(),
                "items": pattern_summaries(&r.cmp, &p, q.start, q.count.min(500), 80),
            })),
            None => Err(conflict("変更パターンを集計中です。")),
        }
    }

    /// Tab-separated text for the clipboard. The desktop shell writes it to
    /// the clipboard itself, so a large copy does not pass through the page.
    pub fn copy(&self, args: &Value) -> Result<(String, usize, usize), ApiError> {
        let r = self.ready()?;
        let q: CopyReq = parse(args)?;
        let view = self.view_by_id(q.view)?;
        let all = &r.slots[order_key(view.order)];
        view.copy_tsv(&r.cmp, all, &q.copy).map_err(bad)
    }
}
