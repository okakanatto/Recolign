//! Views and range queries. A view is an ordered list of rows; a range query
//! returns only the requested rows and columns, with long values cut to a
//! preview length. Full values and the original CSV text come from `detail`.

use std::sync::atomic::{AtomicBool, Ordering};

use memchr::memmem;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::compare::{Comparison, Kind, IGNORED_BIT, NONE};
use crate::error::{CompareError, Side};
use crate::pattern::Patterns;
use crate::table::Table;
use crate::tokenizer::FieldSpan;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    /// One row per unit, left order then right-only records. Both sides.
    Aligned,
    /// Left records in file order, left columns in file order.
    LeftSource,
    /// Right records in file order, right columns in file order.
    RightSource,
}

/// A direction to move the cell cursor in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// The spreadsheet's Ctrl+arrow among positions `0..n`, from `at`, towards
/// `n - 1` when `forward`. Near positions are looked at one by one, the
/// rest in parallel.
fn edge_along(n: usize, at: usize, forward: bool, filled: impl Fn(usize, &mut Vec<FieldSpan>) -> bool + Sync) -> usize {
    let end = if forward { n - 1 } else { 0 };
    if at == end {
        return at;
    }
    let step = |i: usize| if forward { i + 1 } else { i - 1 };
    // The first position from `from` (towards the end) whose cell is `want`.
    let seek = |from: usize, want: bool| -> Option<usize> {
        const NEAR: usize = 4096;
        let mut spans = Vec::new();
        let (a, b) = if forward { (from, n) } else { (0, from + 1) };
        let near = (b - a).min(NEAR);
        let (near_a, near_b, rest_a, rest_b) =
            if forward { (a, a + near, a + near, b) } else { (b - near, b, a, b - near) };
        let pick = |i: usize, spans: &mut Vec<FieldSpan>| filled(i, spans) == want;
        let hit = if forward {
            (near_a..near_b).find(|&i| pick(i, &mut spans))
        } else {
            (near_a..near_b).rev().find(|&i| pick(i, &mut spans))
        };
        hit.or_else(|| {
            let it = (rest_a..rest_b).into_par_iter().map_init(Vec::new, |spans, i| pick(i, spans));
            if forward {
                it.position_first(|x| x).map(|k| rest_a + k)
            } else {
                it.position_last(|x| x).map(|k| rest_a + k)
            }
        })
    };
    let mut spans = Vec::new();
    let next = step(at);
    if filled(at, &mut spans) && filled(next, &mut spans) {
        if next == end {
            return end;
        }
        // The last filled cell of the run: just before the first empty one.
        seek(step(next), false).map_or(end, |e| if forward { e - 1 } else { e + 1 })
    } else {
        seek(next, true).unwrap_or(end)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Filter {
    /// Empty means every kind.
    #[serde(default)]
    pub kinds: Vec<Kind>,
    /// Only units where this pair changed (ignored differences excluded).
    #[serde(default)]
    pub changed_pair: Option<u32>,
    #[serde(default)]
    pub pattern: Option<u32>,
    /// Substring of any cell value on either side (exact, case-sensitive).
    #[serde(default)]
    pub text: Option<String>,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
            && self.changed_pair.is_none()
            && self.pattern.is_none()
            && self.text.as_deref().is_none_or(str::is_empty)
    }
}

enum Rows {
    All(usize),
    /// Ascending unit indices (aligned) or record indices (source orders).
    List(Vec<u32>),
}

/// Row order of a key-aligned view. Source orders always keep file order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
    /// Left file order, then right-only records in right file order.
    #[default]
    Unit,
    /// Key values, compared as strings (code point order), column by column.
    Key,
    /// Changed, deleted, added, same.
    Kind,
    /// Number of changed columns.
    ChangedCount,
    /// Values of one display column (`slot`) on one side, as strings.
    Column,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Sort {
    #[serde(default)]
    pub by: SortBy,
    #[serde(default)]
    pub desc: bool,
    /// Display slot for `SortBy::Column`.
    #[serde(default)]
    pub slot: Option<usize>,
    /// Side whose values are used for `SortBy::Column` (default: left).
    #[serde(default)]
    pub side: Option<Side>,
}

impl Sort {
    pub fn is_default(&self) -> bool {
        self.by == SortBy::Unit && !self.desc
    }
}

/// Order-preserving encoding of a value: `0x00` becomes `0x00 0xFF` and the
/// value ends with `0x00 0x00`, so tuples compare element by element.
fn push_sortable(out: &mut Vec<u8>, v: &str) {
    for &b in v.as_bytes() {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

pub struct View {
    pub order: Order,
    pub filter: Filter,
    pub sort: Sort,
    rows: Rows,
    /// Item → row, for orders other than the natural one.
    inverse: Option<Vec<u32>>,
    /// Row positions whose unit is not `Same`, ascending.
    diff_rows: Vec<u32>,
}

/// A display column. In the aligned order both sides share one slot list so
/// horizontal positions line up; a side without the column has `None`.
#[derive(Clone, Debug, Serialize)]
pub struct Slot {
    pub left: Option<u32>,
    pub right: Option<u32>,
    pub pair: Option<u32>,
    pub key: bool,
    pub excluded: bool,
    pub name_left: Option<String>,
    pub name_right: Option<String>,
}

pub fn slots(cmp: &Comparison, order: Order) -> Vec<Slot> {
    let lname = |c: u32| Some(cmp.left.headers[c as usize].clone());
    let rname = |c: u32| Some(cmp.right.headers[c as usize].clone());
    let pair_slot = |k: u32| {
        let p = cmp.mapping.pairs[k as usize];
        Slot {
            left: Some(p.left),
            right: Some(p.right),
            pair: Some(k),
            key: cmp.is_key_pair(k),
            excluded: cmp.spec.excluded.contains(&k),
            name_left: lname(p.left),
            name_right: rname(p.right),
        }
    };
    match order {
        Order::Aligned => {
            let mut v: Vec<Slot> = cmp.spec.keys.iter().map(|&k| pair_slot(k)).collect();
            v.extend((0..cmp.mapping.pairs.len() as u32).filter(|&k| !cmp.is_key_pair(k)).map(pair_slot));
            v.extend(cmp.mapping.left_only.iter().map(|&c| Slot {
                left: Some(c),
                right: None,
                pair: None,
                key: false,
                excluded: false,
                name_left: lname(c),
                name_right: None,
            }));
            v.extend(cmp.mapping.right_only.iter().map(|&c| Slot {
                left: None,
                right: Some(c),
                pair: None,
                key: false,
                excluded: false,
                name_left: None,
                name_right: rname(c),
            }));
            v
        }
        Order::LeftSource => {
            let pairs = cmp.mapping.pair_of_left(cmp.left.ncols());
            (0..cmp.left.ncols() as u32)
                .map(|c| match pairs[c as usize] {
                    Some(k) => pair_slot(k),
                    None => Slot {
                        left: Some(c),
                        right: None,
                        pair: None,
                        key: false,
                        excluded: false,
                        name_left: lname(c),
                        name_right: None,
                    },
                })
                .collect()
        }
        Order::RightSource => {
            let pairs = cmp.mapping.pair_of_right(cmp.right.ncols());
            (0..cmp.right.ncols() as u32)
                .map(|c| match pairs[c as usize] {
                    Some(k) => pair_slot(k),
                    None => Slot {
                        left: None,
                        right: Some(c),
                        pair: None,
                        key: false,
                        excluded: false,
                        name_left: None,
                        name_right: rname(c),
                    },
                })
                .collect()
        }
    }
}

fn record_contains(t: &Table, r: usize, finder: &memmem::Finder, spans: &mut Vec<FieldSpan>) -> bool {
    // The record text contains every value as a substring unless the value
    // has `""` escapes, and a needle without `"` is unaffected by them, so
    // the raw text is a valid pre-filter. Values are then checked one by one
    // so a match cannot span two cells.
    if finder.needle().contains(&b'"') || finder.find(t.raw_text(r).as_bytes()).is_some() {
        t.split(r, usize::MAX, spans);
        spans.iter().any(|&s| finder.find(t.value(r, s).as_bytes()).is_some())
    } else {
        false
    }
}

impl View {
    pub fn build(
        cmp: &Comparison,
        patterns: Option<&Patterns>,
        order: Order,
        filter: Filter,
        cancel: &CancelToken,
    ) -> Result<View, CompareError> {
        Self::build_sorted(cmp, patterns, order, filter, Sort::default(), cancel)
    }

    /// Like `build`, with a row order. Sorting applies to the key-aligned
    /// order only; ties keep the key-aligned order.
    pub fn build_sorted(
        cmp: &Comparison,
        patterns: Option<&Patterns>,
        order: Order,
        filter: Filter,
        sort: Sort,
        cancel: &CancelToken,
    ) -> Result<View, CompareError> {
        Self::build_with(cmp, patterns, order, filter, sort, None, cancel)
    }

    /// Like `build_sorted`, keeping only units for which `keep` is true
    /// (conditions the engine does not know, such as review states).
    pub fn build_with(
        cmp: &Comparison,
        patterns: Option<&Patterns>,
        order: Order,
        filter: Filter,
        sort: Sort,
        keep: Option<&(dyn Fn(u32) -> bool + Sync)>,
        cancel: &CancelToken,
    ) -> Result<View, CompareError> {
        let sort = if order == Order::Aligned { sort } else { Sort::default() };
        let item_count = match order {
            Order::Aligned => cmp.units.len(),
            Order::LeftSource => cmp.left.len(),
            Order::RightSource => cmp.right.len(),
        };
        let unit_of = |item: usize| -> usize {
            match order {
                Order::Aligned | Order::LeftSource => item,
                Order::RightSource => cmp.right_unit[item] as usize,
            }
        };
        let rows = if filter.is_empty() && keep.is_none() {
            Rows::All(item_count)
        } else {
            let finder = filter
                .text
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|t| memmem::Finder::new(t.as_bytes()).into_owned());
            let stop = AtomicBool::new(false);
            let list: Vec<u32> = (0..item_count)
                .into_par_iter()
                .with_min_len(4096)
                .map_init(Vec::new, |spans, item| {
                    if item % 4096 == 0 && cancel.is_cancelled() {
                        stop.store(true, Ordering::Relaxed);
                    }
                    if stop.load(Ordering::Relaxed) {
                        return None;
                    }
                    let u = unit_of(item);
                    let unit = cmp.units[u];
                    if !filter.kinds.is_empty() && !filter.kinds.contains(&unit.kind) {
                        return None;
                    }
                    if let Some(k) = keep {
                        if !k(u as u32) {
                            return None;
                        }
                    }
                    if let Some(k) = filter.changed_pair {
                        if !cmp.changed_pairs(u).any(|e| e == k) {
                            return None;
                        }
                    }
                    if let Some(p) = filter.pattern {
                        if patterns.is_none_or(|ps| ps.unit_pattern[u] != p) {
                            return None;
                        }
                    }
                    if let Some(f) = &finder {
                        let hit = (unit.left != NONE && record_contains(&cmp.left, unit.left as usize, f, spans))
                            || (unit.right != NONE && record_contains(&cmp.right, unit.right as usize, f, spans));
                        if !hit {
                            return None;
                        }
                    }
                    Some(item as u32)
                })
                .flatten()
                .collect();
            if stop.load(Ordering::Relaxed) {
                return Err(CompareError::Cancelled);
            }
            Rows::List(list)
        };
        let (rows, inverse) = if sort.is_default() {
            (rows, None)
        } else {
            let mut items: Vec<u32> = match rows {
                Rows::All(n) => (0..n as u32).collect(),
                Rows::List(v) => v,
            };
            sort_units(cmp, &mut items, &sort)?;
            let mut inv = vec![NONE; item_count];
            for (row, &u) in items.iter().enumerate() {
                inv[u as usize] = row as u32;
            }
            (Rows::List(items), Some(inv))
        };
        let mut view = View { order, filter, sort, rows, inverse, diff_rows: Vec::new() };
        let n = view.len();
        view.diff_rows = (0..n)
            .into_par_iter()
            .with_min_len(65536)
            .filter(|&row| cmp.units[view.unit_at(cmp, row)].kind != Kind::Same)
            .map(|row| row as u32)
            .collect();
        Ok(view)
    }

    pub fn len(&self) -> usize {
        match &self.rows {
            Rows::All(n) => *n,
            Rows::List(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn diff_count(&self) -> usize {
        self.diff_rows.len()
    }

    fn item(&self, row: usize) -> usize {
        match &self.rows {
            Rows::All(_) => row,
            Rows::List(v) => v[row] as usize,
        }
    }

    pub fn unit_at(&self, cmp: &Comparison, row: usize) -> usize {
        let item = self.item(row);
        match self.order {
            Order::Aligned | Order::LeftSource => item,
            Order::RightSource => cmp.right_unit[item] as usize,
        }
    }

    /// Row showing `unit`, if the unit is in this view.
    pub fn row_of_unit(&self, cmp: &Comparison, unit: u32) -> Option<usize> {
        let u = cmp.units.get(unit as usize)?;
        let item = match self.order {
            Order::Aligned => unit,
            Order::LeftSource => u.left,
            Order::RightSource => u.right,
        };
        if item == NONE {
            return None;
        }
        match &self.rows {
            Rows::All(n) => ((item as usize) < *n).then_some(item as usize),
            Rows::List(v) => match &self.inverse {
                Some(inv) => inv.get(item as usize).filter(|&&r| r != NONE).map(|&r| r as usize),
                None => v.binary_search(&item).ok(),
            },
        }
    }

    /// Next (or previous) row whose unit is a difference. `None` at the end;
    /// the caller decides whether to wrap.
    pub fn next_diff(&self, from: Option<usize>, forward: bool) -> Option<usize> {
        let d = &self.diff_rows;
        if forward {
            let i = match from {
                None => 0,
                Some(f) => d.partition_point(|&r| r as usize <= f),
            };
            d.get(i).map(|&r| r as usize)
        } else {
            let f = from.unwrap_or(usize::MAX);
            let i = d.partition_point(|&r| (r as usize) < f);
            (i > 0).then(|| d[i - 1] as usize)
        }
    }

    /// Sides whose records this view searches: both in the key-aligned
    /// order, only the shown file's in a file order.
    fn search_sides(&self) -> (bool, bool) {
        match self.order {
            Order::Aligned => (true, true),
            Order::LeftSource => (true, false),
            Order::RightSource => (false, true),
        }
    }

    fn row_contains(&self, cmp: &Comparison, row: usize, f: &memmem::Finder, spans: &mut Vec<FieldSpan>) -> bool {
        let unit = cmp.units[self.unit_at(cmp, row)];
        let (l, r) = self.search_sides();
        (l && unit.left != NONE && record_contains(&cmp.left, unit.left as usize, f, spans))
            || (r && unit.right != NONE && record_contains(&cmp.right, unit.right as usize, f, spans))
    }

    /// The next row after `from` (before it when `forward` is false) with a
    /// cell containing `text`, continuing from the other end when the end
    /// is reached. `None` from starts at the first (or last) row. Matching
    /// is exact and case-sensitive, within one cell.
    pub fn find_text(
        &self,
        cmp: &Comparison,
        slots: &[Slot],
        text: &str,
        from: Option<usize>,
        forward: bool,
    ) -> Option<TextHit> {
        let n = self.len();
        if text.is_empty() || n == 0 {
            return None;
        }
        let f = memmem::Finder::new(text.as_bytes());
        let hit = |a: usize, b: usize| -> Option<usize> {
            let it = (a..b).into_par_iter().map_init(Vec::new, |spans, row| self.row_contains(cmp, row, &f, spans));
            if forward {
                it.position_first(|x| x).map(|i| a + i)
            } else {
                it.position_last(|x| x).map(|i| a + i)
            }
        };
        let (row, wrapped) = if forward {
            let start = from.map_or(0, |r| (r + 1).min(n));
            match hit(start, n) {
                Some(r) => (r, false),
                None => (hit(0, start.min(n))?, from.is_some()),
            }
        } else {
            let end = from.unwrap_or(n).min(n);
            match hit(0, end) {
                Some(r) => (r, false),
                None => (hit(end, n)?, from.is_some()),
            }
        };
        let unit = cmp.units[self.unit_at(cmp, row)];
        let (use_l, use_r) = self.search_sides();
        let cells = |t: &Table, rec: u32, side: Side| -> Vec<usize> {
            if rec == NONE {
                return Vec::new();
            }
            let mut spans = Vec::new();
            t.split(rec as usize, usize::MAX, &mut spans);
            slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    let c = match side {
                        Side::Left => s.left,
                        Side::Right => s.right,
                    }?;
                    f.find(t.value(rec as usize, spans[c as usize]).as_bytes()).map(|_| i)
                })
                .collect()
        };
        Some(TextHit {
            row,
            wrapped,
            left: if use_l { cells(&cmp.left, unit.left, Side::Left) } else { Vec::new() },
            right: if use_r { cells(&cmp.right, unit.right, Side::Right) } else { Vec::new() },
        })
    }

    /// Number of rows with a cell containing `text`.
    pub fn count_text(&self, cmp: &Comparison, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let f = memmem::Finder::new(text.as_bytes());
        (0..self.len())
            .into_par_iter()
            .with_min_len(4096)
            .map_init(Vec::new, |spans, row| self.row_contains(cmp, row, &f, spans))
            .filter(|&x| x)
            .count()
    }

    /// The next (or previous) difference row after `from` whose unit
    /// satisfies `pred`, without wrapping.
    pub fn next_diff_where(
        &self,
        cmp: &Comparison,
        from: Option<usize>,
        forward: bool,
        pred: impl Fn(u32) -> bool,
    ) -> Option<usize> {
        let d = &self.diff_rows;
        let unit = |r: u32| self.unit_at(cmp, r as usize) as u32;
        if forward {
            let i = from.map_or(0, |f| d.partition_point(|&r| r as usize <= f));
            d[i..].iter().find(|&&r| pred(unit(r))).map(|&r| r as usize)
        } else {
            let f = from.unwrap_or(usize::MAX);
            let i = d.partition_point(|&r| (r as usize) < f);
            d[..i].iter().rev().find(|&&r| pred(unit(r))).map(|&r| r as usize)
        }
    }

    /// Units of rows `[start, end)`.
    pub fn units_of_rows(&self, cmp: &Comparison, start: usize, end: usize) -> Vec<u32> {
        (start.min(self.len())..end.min(self.len())).map(|r| self.unit_at(cmp, r) as u32).collect()
    }

    /// Whether the cell of `slot` in `row` holds a value on `side`: the row
    /// has a record there, the column exists there, and the value is not
    /// empty.
    fn filled(&self, cmp: &Comparison, row: usize, slot: &Slot, side: Side, spans: &mut Vec<FieldSpan>) -> bool {
        let unit = cmp.units[self.unit_at(cmp, row)];
        let (t, rec, col) = match side {
            Side::Left => (&cmp.left, unit.left, slot.left),
            Side::Right => (&cmp.right, unit.right, slot.right),
        };
        let Some(c) = col else { return false };
        if rec == NONE {
            return false;
        }
        t.split(rec as usize, c as usize + 1, spans);
        !t.value(rec as usize, spans[c as usize]).is_empty()
    }

    /// Where Ctrl+arrow goes from the cell at `from` (row, and an index into
    /// `cols`, the slots in display order) in the pane showing `side`, as in
    /// a spreadsheet: along filled cells, to the last one before an empty
    /// cell; from an empty cell, or the last filled one, to the next filled
    /// cell; to the first or last row or column when there is none.
    pub fn edge(
        &self,
        cmp: &Comparison,
        slots: &[Slot],
        cols: &[usize],
        side: Side,
        from: (usize, usize),
        dir: Direction,
    ) -> (usize, usize) {
        let n = self.len();
        if n == 0 || cols.is_empty() {
            return (0, 0);
        }
        let (row, col) = (from.0.min(n - 1), from.1.min(cols.len() - 1));
        match dir {
            Direction::Up | Direction::Down => {
                let slot = &slots[cols[col]];
                let filled = |r: usize, spans: &mut Vec<FieldSpan>| self.filled(cmp, r, slot, side, spans);
                (edge_along(n, row, dir == Direction::Down, filled), col)
            }
            Direction::Left | Direction::Right => {
                let unit = cmp.units[self.unit_at(cmp, row)];
                let (t, rec) = match side {
                    Side::Left => (&cmp.left, unit.left),
                    Side::Right => (&cmp.right, unit.right),
                };
                let mut spans = Vec::new();
                if rec != NONE {
                    t.split(rec as usize, usize::MAX, &mut spans);
                }
                let values: Vec<bool> = cols
                    .iter()
                    .map(|&i| {
                        let c = match side {
                            Side::Left => slots[i].left,
                            Side::Right => slots[i].right,
                        };
                        rec != NONE && c.is_some_and(|c| !t.value(rec as usize, spans[c as usize]).is_empty())
                    })
                    .collect();
                (row, edge_along(cols.len(), col, dir == Direction::Right, |i, _| values[i]))
            }
        }
    }

    /// Rows `[start, start + count)` with cells for slot range `cols`.
    pub fn range(
        &self,
        cmp: &Comparison,
        patterns: Option<&Patterns>,
        slots: &[Slot],
        req: &RangeRequest,
    ) -> RangeResponse {
        let n = self.len();
        let start = req.start.min(n);
        let end = start.saturating_add(req.count).min(n);
        let col_a = req.col_start.min(slots.len());
        let col_b = col_a.saturating_add(req.col_count).min(slots.len());
        let visible: Vec<&Slot> =
            req.pinned.iter().filter_map(|&i| slots.get(i)).chain(slots[col_a..col_b].iter()).collect();
        let want_left = self.order != Order::RightSource;
        let want_right = self.order != Order::LeftSource;
        let mut spans = Vec::new();
        let rows = (start..end)
            .map(|row| {
                let u = self.unit_at(cmp, row);
                let unit = cmp.units[u];
                let flag = |pair: Option<u32>| -> u8 {
                    let Some(k) = pair else { return 0 };
                    match cmp.diff_entry(u, k) {
                        Some(e) if e & IGNORED_BIT != 0 => 2,
                        Some(_) => 1,
                        None => 0,
                    }
                };
                let mut side = |t: &Table, rec: u32, col_of: &dyn Fn(&Slot) -> Option<u32>| -> Option<RecOut> {
                    if rec == NONE {
                        return None;
                    }
                    let r = rec as usize;
                    t.split(r, usize::MAX, &mut spans);
                    let p = t.records[r];
                    let cells = visible
                        .iter()
                        .map(|s| match col_of(s) {
                            None => CellOut { v: None, cut: false, d: 0 },
                            Some(c) => {
                                let (v, cut) = preview(&t.value(r, spans[c as usize]), req.max_chars);
                                CellOut {
                                    v: Some(v),
                                    cut,
                                    d: if unit.kind == Kind::Changed || unit.ignored { flag(s.pair) } else { 0 },
                                }
                            }
                        })
                        .collect();
                    Some(RecOut { record: r as u64 + 1, line: p.line_start, line_end: p.line_end, cells })
                };
                let left = if want_left { side(&cmp.left, unit.left, &|s: &Slot| s.left) } else { None };
                let right = if want_right { side(&cmp.right, unit.right, &|s: &Slot| s.right) } else { None };
                RowOut {
                    row,
                    unit: u as u32,
                    kind: unit.kind,
                    ignored: unit.ignored,
                    pattern: patterns.map(|p| p.unit_pattern[u]).filter(|&p| p != NONE),
                    left,
                    right,
                }
            })
            .collect();
        RangeResponse { total: n, start, rows }
    }
}

fn kind_rank(k: Kind) -> u8 {
    match k {
        Kind::Changed => 0,
        Kind::Deleted => 1,
        Kind::Added => 2,
        Kind::Same => 3,
    }
}

/// Sorts key-aligned units. Ties keep ascending unit order.
fn sort_units(cmp: &Comparison, items: &mut [u32], sort: &Sort) -> Result<(), CompareError> {
    let dir = |o: std::cmp::Ordering| if sort.desc { o.reverse() } else { o };
    match sort.by {
        SortBy::Unit => {
            if sort.desc {
                items.reverse();
            }
        }
        SortBy::Kind => items.par_sort_by(|&a, &b| {
            dir(kind_rank(cmp.units[a as usize].kind).cmp(&kind_rank(cmp.units[b as usize].kind))).then(a.cmp(&b))
        }),
        SortBy::ChangedCount => {
            let count = |u: u32| cmp.changed_pairs(u as usize).count();
            items.par_sort_by(|&a, &b| dir(count(a).cmp(&count(b))).then(a.cmp(&b)));
        }
        SortBy::Key | SortBy::Column => {
            let column = if sort.by == SortBy::Column {
                let all = slots(cmp, Order::Aligned);
                let slot = sort.slot.and_then(|i| all.get(i).cloned()).ok_or_else(|| CompareError::KeyColumn {
                    message: "並べ替えに使う列が見つかりません。".into(),
                })?;
                Some((slot, sort.side.unwrap_or(Side::Left)))
            } else {
                None
            };
            // (sortable bytes, unit); `None` (no value on that side) sorts last.
            let mut keyed: Vec<(Option<Vec<u8>>, u32)> = items
                .par_iter()
                .map_init(Vec::new, |spans, &u| {
                    let mut out = Vec::new();
                    let ok = match &column {
                        None => {
                            for v in cmp.key_values(u as usize) {
                                push_sortable(&mut out, &v);
                            }
                            true
                        }
                        Some((slot, side)) => {
                            let unit = cmp.units[u as usize];
                            let (t, rec, col) = match side {
                                Side::Left => (&cmp.left, unit.left, slot.left),
                                Side::Right => (&cmp.right, unit.right, slot.right),
                            };
                            match col {
                                Some(c) if rec != NONE => {
                                    t.split(rec as usize, c as usize + 1, spans);
                                    push_sortable(&mut out, &t.value(rec as usize, spans[c as usize]));
                                    true
                                }
                                _ => false,
                            }
                        }
                    };
                    (ok.then_some(out), u)
                })
                .collect();
            keyed.par_sort_by(|(ka, a), (kb, b)| {
                let o = match (ka, kb) {
                    (Some(x), Some(y)) => dir(x.cmp(y)),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                };
                o.then(a.cmp(b))
            });
            for (dst, (_, u)) in items.iter_mut().zip(keyed) {
                *dst = u;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopySide {
    Left,
    Right,
    /// Left columns, then right columns.
    Both,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CopyRequest {
    /// First row (inclusive).
    pub start: usize,
    /// Last row (exclusive). Rows past the end are ignored.
    pub end: usize,
    /// Display slots in the order they are copied.
    pub slots: Vec<usize>,
    pub side: CopySide,
    #[serde(default)]
    pub header: bool,
}

/// Upper limit for one copy, in cells.
pub const COPY_MAX_CELLS: usize = 2_000_000;

fn tsv_cell(out: &mut String, v: &str) {
    if v.contains(['\t', '\n', '\r', '"']) {
        out.push('"');
        out.push_str(&v.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(v);
    }
}

impl View {
    /// Tab-separated text of a row and column range, as spreadsheet
    /// applications paste it. Values are copied unchanged; a value with a
    /// tab, line break or quote is quoted. Absent records and columns are
    /// empty cells.
    pub fn copy_tsv(
        &self,
        cmp: &Comparison,
        all_slots: &[Slot],
        req: &CopyRequest,
    ) -> Result<(String, usize, usize), String> {
        let start = req.start.min(self.len());
        let end = req.end.min(self.len()).max(start);
        let sides: Vec<Side> = match (self.order, req.side) {
            (Order::LeftSource, _) => vec![Side::Left],
            (Order::RightSource, _) => vec![Side::Right],
            (_, CopySide::Left) => vec![Side::Left],
            (_, CopySide::Right) => vec![Side::Right],
            (_, CopySide::Both) => vec![Side::Left, Side::Right],
        };
        let slots: Vec<&Slot> = req.slots.iter().filter_map(|&i| all_slots.get(i)).collect();
        let cells = (end - start) * slots.len() * sides.len();
        if cells > COPY_MAX_CELLS {
            return Err(format!(
                "{}行×{}列は、一度にコピーできる量（{}セル）を超えています。範囲を狭めてください。",
                end - start,
                slots.len() * sides.len(),
                COPY_MAX_CELLS
            ));
        }
        let mut out = String::new();
        if req.header {
            let mut first = true;
            for &side in &sides {
                for s in &slots {
                    if !first {
                        out.push('\t');
                    }
                    first = false;
                    let name = match side {
                        Side::Left => s.name_left.as_deref().or(s.name_right.as_deref()),
                        Side::Right => s.name_right.as_deref().or(s.name_left.as_deref()),
                    }
                    .unwrap_or("");
                    if sides.len() == 2 {
                        tsv_cell(&mut out, &format!("{}:{name}", side.label()));
                    } else {
                        tsv_cell(&mut out, name);
                    }
                }
            }
            out.push_str("\r\n");
        }
        let mut spans = Vec::new();
        for row in start..end {
            let unit = cmp.units[self.unit_at(cmp, row)];
            let mut first = true;
            for &side in &sides {
                let (t, rec) = match side {
                    Side::Left => (&cmp.left, unit.left),
                    Side::Right => (&cmp.right, unit.right),
                };
                if rec != NONE {
                    t.split(rec as usize, usize::MAX, &mut spans);
                }
                for s in &slots {
                    if !first {
                        out.push('\t');
                    }
                    first = false;
                    let col = match side {
                        Side::Left => s.left,
                        Side::Right => s.right,
                    };
                    if let (Some(c), true) = (col, rec != NONE) {
                        tsv_cell(&mut out, &t.value(rec as usize, spans[c as usize]));
                    }
                }
            }
            out.push_str("\r\n");
        }
        Ok((out, end - start, cells))
    }
}

/// A row found by text search, with the slots (indices into the view's
/// slot list) whose value contains the text, per side.
#[derive(Clone, Debug, Serialize)]
pub struct TextHit {
    pub row: usize,
    /// The search continued from the other end of the view.
    pub wrapped: bool,
    pub left: Vec<usize>,
    pub right: Vec<usize>,
}

fn preview(v: &str, max_chars: usize) -> (String, bool) {
    match v.char_indices().nth(max_chars) {
        Some((i, _)) => (v[..i].to_string(), true),
        None => (v.to_string(), false),
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct RangeRequest {
    pub start: usize,
    pub count: usize,
    pub col_start: usize,
    pub col_count: usize,
    #[serde(default = "default_preview")]
    pub max_chars: usize,
    /// Slots returned before the range (for example frozen key columns).
    #[serde(default)]
    pub pinned: Vec<usize>,
}

fn default_preview() -> usize {
    200
}

#[derive(Clone, Debug, Serialize)]
pub struct CellOut {
    /// `None` when the column does not exist on this side.
    pub v: Option<String>,
    /// The preview was cut; fetch the detail for the full value.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cut: bool,
    /// 1 = changed, 2 = differs only in an ignored way.
    #[serde(skip_serializing_if = "is_zero")]
    pub d: u8,
}

fn is_zero(v: &u8) -> bool {
    *v == 0
}

#[derive(Clone, Debug, Serialize)]
pub struct RecOut {
    /// 1-based data record number.
    pub record: u64,
    pub line: u64,
    pub line_end: u64,
    pub cells: Vec<CellOut>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RowOut {
    pub row: usize,
    pub unit: u32,
    pub kind: Kind,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub ignored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<u32>,
    pub left: Option<RecOut>,
    pub right: Option<RecOut>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RangeResponse {
    pub total: usize,
    pub start: usize,
    pub rows: Vec<RowOut>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecDetail {
    pub side: Side,
    pub record: u64,
    pub line_start: u64,
    pub line_end: u64,
    pub byte_start: u64,
    pub byte_end: u64,
    /// Original CSV text of the record, decoded for display.
    pub raw: String,
    /// `raw` is only the first [`DETAIL_RAW_CHARS`] characters.
    pub raw_cut: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DetailCell {
    pub slot: usize,
    pub name_left: Option<String>,
    pub name_right: Option<String>,
    pub key: bool,
    pub excluded: bool,
    pub left: Option<String>,
    pub right: Option<String>,
    /// The value is only its first [`DETAIL_VALUE_CHARS`] characters.
    pub left_cut: bool,
    pub right_cut: bool,
    pub d: u8,
}

#[derive(Clone, Debug, Serialize)]
pub struct Detail {
    pub unit: u32,
    pub kind: Kind,
    pub ignored: bool,
    pub key: Vec<String>,
    pub pattern: Option<u32>,
    pub left: Option<RecDetail>,
    pub right: Option<RecDetail>,
    /// Every aligned slot, so hidden columns' differences are never lost.
    pub cells: Vec<DetailCell>,
}

/// The longest value, key value and original text the detail of a record
/// carries, in characters. Longer ones are cut and marked, so that one huge
/// field cannot stall the screen; the grid's copy still gives whole values.
pub const DETAIL_VALUE_CHARS: usize = 10_000;
pub const DETAIL_KEY_CHARS: usize = 1_000;
pub const DETAIL_RAW_CHARS: usize = 50_000;

pub fn detail(cmp: &Comparison, patterns: Option<&Patterns>, unit: u32) -> Option<Detail> {
    let u = unit as usize;
    let un = *cmp.units.get(u)?;
    let slots = slots(cmp, Order::Aligned);
    let rec = |t: &Table, r: u32, side: Side| -> Option<(Vec<(String, bool)>, RecDetail)> {
        if r == NONE {
            return None;
        }
        let r = r as usize;
        let p = t.records[r];
        let (raw, raw_cut) = preview(&t.raw_text(r), DETAIL_RAW_CHARS);
        Some((
            t.values(r).iter().map(|v| preview(v, DETAIL_VALUE_CHARS)).collect::<Vec<_>>(),
            RecDetail {
                side,
                record: r as u64 + 1,
                line_start: p.line_start,
                line_end: p.line_end,
                byte_start: p.start,
                byte_end: p.end,
                raw,
                raw_cut,
            },
        ))
    };
    let value = |rec: &Option<(Vec<(String, bool)>, RecDetail)>, c: Option<u32>| {
        c.and_then(|c| rec.as_ref().map(|(v, _)| v[c as usize].clone()))
            .map_or((None, false), |(v, cut)| (Some(v), cut))
    };
    let left = rec(&cmp.left, un.left, Side::Left);
    let right = rec(&cmp.right, un.right, Side::Right);
    let cells = slots
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (l, left_cut) = value(&left, s.left);
            let (r, right_cut) = value(&right, s.right);
            DetailCell {
                slot: i,
                name_left: s.name_left.clone(),
                name_right: s.name_right.clone(),
                key: s.key,
                excluded: s.excluded,
                left: l,
                right: r,
                left_cut,
                right_cut,
                d: s.pair.map_or(0, |k| match cmp.diff_entry(u, k) {
                    Some(e) if e & IGNORED_BIT != 0 => 2,
                    Some(_) => 1,
                    None => 0,
                }),
            }
        })
        .collect();
    Some(Detail {
        unit,
        kind: un.kind,
        ignored: un.ignored,
        // A key value cut ends in …, as there is no room for a note beside it.
        key: cmp
            .key_values(u)
            .iter()
            .map(|k| match preview(k, DETAIL_KEY_CHARS) {
                (v, true) => v + "…",
                (v, false) => v,
            })
            .collect(),
        pattern: patterns.map(|p| p.unit_pattern[u]).filter(|&p| p != NONE),
        left: left.map(|(_, d)| d),
        right: right.map(|(_, d)| d),
        cells,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct PatternChange {
    pub pair: u32,
    pub name: String,
    pub before: String,
    pub after: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PatternSummary {
    pub id: u32,
    pub count: usize,
    pub first_unit: u32,
    pub changes: Vec<PatternChange>,
}

pub fn pattern_summaries(
    cmp: &Comparison,
    patterns: &Patterns,
    start: usize,
    count: usize,
    max_chars: usize,
) -> Vec<PatternSummary> {
    let mut ls = Vec::new();
    let mut rs = Vec::new();
    patterns
        .list
        .iter()
        .skip(start)
        .take(count)
        .map(|p| {
            let u = p.units[0] as usize;
            let unit = cmp.units[u];
            let (l, r) = (unit.left as usize, unit.right as usize);
            cmp.left.split(l, usize::MAX, &mut ls);
            cmp.right.split(r, usize::MAX, &mut rs);
            let changes = p
                .pairs
                .iter()
                .map(|&k| {
                    let pr = cmp.mapping.pairs[k as usize];
                    PatternChange {
                        pair: k,
                        name: cmp.left.headers[pr.left as usize].clone(),
                        before: preview(&cmp.left.value(l, ls[pr.left as usize]), max_chars).0,
                        after: preview(&cmp.right.value(r, rs[pr.right as usize]), max_chars).0,
                    }
                })
                .collect();
            PatternSummary { id: p.id, count: p.units.len(), first_unit: p.units[0], changes }
        })
        .collect()
}
