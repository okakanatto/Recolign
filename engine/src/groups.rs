//! Grouping of changed records by simple, user-selectable rules (plan 6,
//! U-16). Every rule is an exact equality on what changed; there is no
//! similarity or guessing.
//!
//! - `Columns`: the same set of changed columns (values not compared).
//! - `PerColumn`: one column with the same before → after values. A record
//!   is counted once for each of its changed columns.
//! - `Full`: the same changed columns with the same before/after values
//!   (the full change patterns of `pattern.rs`).
//!
//! Columns can be left out of the grouping (for example a time stamp that
//! changes in every record). Records whose changes are all in such columns
//! are not grouped and are counted apart.
//!
//! Members of all groups live in one array, so millions of one-member
//! groups (every record changed differently) cost a few bytes each.
//! Values are hashed only to find candidates; members that share a hash
//! are compared by value, so a collision never merges two different
//! changes.

use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::compare::{Comparison, Kind, NONE};
use crate::error::CompareError;
use crate::pattern::Patterns;
use crate::tokenizer::FieldSpan;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRule {
    Columns,
    PerColumn,
    Full,
}

#[derive(Clone, Copy, Debug)]
pub struct Group {
    pub id: u32,
    start: u32,
    len: u32,
    /// `PerColumn`: the changed pair; `NONE` for the other rules.
    pair: u32,
    /// `PerColumn`: members that also changed in other columns (not
    /// counting the columns left out of the grouping).
    pub with_other: u32,
}

impl Group {
    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub struct Groups {
    pub rule: GroupRule,
    /// Pairs left out of the grouping, ascending.
    pub skip: Vec<u32>,
    /// Changed records whose changes are all in skipped pairs.
    pub only_skipped: usize,
    /// Largest first; ties by first member.
    pub list: Vec<Group>,
    /// Members of every group; each group's members are ascending.
    members: Vec<u32>,
    /// Changed records looked at.
    pub changed_units: usize,
    /// Groups with exactly one member.
    pub singles: usize,
}

impl Groups {
    pub fn units(&self, g: &Group) -> &[u32] {
        &self.members[g.start as usize..(g.start + g.len) as usize]
    }

    pub fn first(&self, g: &Group) -> u32 {
        self.members[g.start as usize]
    }

    /// Changed pairs that define the group, ascending.
    pub fn pairs(&self, cmp: &Comparison, g: &Group) -> Vec<u32> {
        if g.pair != NONE {
            vec![g.pair]
        } else {
            cmp.changed_pairs(self.first(g) as usize).filter(|k| self.skip.binary_search(k).is_err()).collect()
        }
    }

    /// `PerColumn`: the before and after values, which all members share.
    pub fn values(&self, cmp: &Comparison, g: &Group) -> Option<(String, String)> {
        (g.pair != NONE).then(|| {
            let (b, a) = cell_values(cmp, self.first(g), g.pair);
            (b.into_owned(), a.into_owned())
        })
    }
}

/// Before and after values of changed pair `k` in unit `u`.
fn cell_values(cmp: &Comparison, u: u32, k: u32) -> (std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>) {
    let unit = cmp.units[u as usize];
    let p = cmp.mapping.pairs[k as usize];
    let (l, r) = (unit.left as usize, unit.right as usize);
    let (mut ls, mut rs) = (Vec::new(), Vec::new());
    cmp.left.split(l, p.left as usize + 1, &mut ls);
    cmp.right.split(r, p.right as usize + 1, &mut rs);
    (cmp.left.value(l, ls[p.left as usize]), cmp.right.value(r, rs[p.right as usize]))
}

type Run = (u32, u32, u32, u32);

/// Builds the groups from runs of `members` (start, len, pair, with_other).
fn finish(
    rule: GroupRule,
    skip: Vec<u32>,
    members: Vec<u32>,
    runs: Vec<Run>,
    changed_units: usize,
    only_skipped: usize,
) -> Groups {
    let mut list: Vec<Group> =
        runs.into_iter().map(|(start, len, pair, with_other)| Group { id: 0, start, len, pair, with_other }).collect();
    list.par_sort_unstable_by(|a, b| b.len.cmp(&a.len).then(members[a.start as usize].cmp(&members[b.start as usize])));
    for (i, g) in list.iter_mut().enumerate() {
        g.id = i as u32;
    }
    let singles = list.iter().filter(|g| g.len == 1).count();
    Groups { rule, skip, only_skipped, list, members, changed_units, singles }
}

/// Groups the changed records by `rule`, leaving the pairs in `skip` out.
pub fn group(
    cmp: &Comparison,
    patterns: Option<&Patterns>,
    rule: GroupRule,
    skip: &[u32],
    cancel: &CancelToken,
) -> Result<Groups, CompareError> {
    let mut skip = skip.to_vec();
    skip.sort_unstable();
    skip.dedup();
    let skipped = |k: &u32| skip.binary_search(k).is_ok();
    let changed: Vec<u32> =
        cmp.diff_units.iter().copied().filter(|&u| cmp.units[u as usize].kind == Kind::Changed).collect();
    let n = changed.len();
    // Records with at least one change outside the skipped pairs.
    let (changed, only_skipped) = if skip.is_empty() {
        (changed, 0)
    } else {
        let kept: Vec<u32> =
            changed.into_iter().filter(|&u| cmp.changed_pairs(u as usize).any(|k| !skipped(&k))).collect();
        let dropped = n - kept.len();
        (kept, dropped)
    };
    if cancel.is_cancelled() {
        return Err(CompareError::Cancelled);
    }
    let (members, runs) = match rule {
        GroupRule::Columns => {
            // Sorting by the changed-pair list puts equal sets next to each other.
            let pairs = |u: u32| cmp.changed_pairs(u as usize).filter(|k| !skipped(k));
            let mut order = changed;
            order.par_sort_by(|&a, &b| pairs(a).cmp(pairs(b)).then(a.cmp(&b)));
            let mut runs = Vec::new();
            let mut a = 0;
            for i in 1..=order.len() {
                if i == order.len() || !pairs(order[i]).eq(pairs(order[a])) {
                    runs.push((a as u32, (i - a) as u32, NONE, 0));
                    a = i;
                }
            }
            (order, runs)
        }
        GroupRule::PerColumn => per_column(cmp, &changed, &skip, cancel)?,
        GroupRule::Full if skip.is_empty() => {
            let p = patterns.ok_or(CompareError::Cancelled)?;
            let mut members = Vec::with_capacity(n);
            let mut runs = Vec::with_capacity(p.list.len());
            for pat in &p.list {
                runs.push((members.len() as u32, pat.units.len() as u32, NONE, 0));
                members.extend_from_slice(&pat.units);
            }
            (members, runs)
        }
        GroupRule::Full => full_without(cmp, &changed, &skip, cancel)?,
    };
    Ok(finish(rule, skip, members, runs, n, only_skipped))
}

/// Length-prefixed (pair, before, after) of the changed pairs not skipped.
fn signature(
    cmp: &Comparison,
    u: u32,
    skip: &[u32],
    ls: &mut Vec<FieldSpan>,
    rs: &mut Vec<FieldSpan>,
    out: &mut Vec<u8>,
) {
    out.clear();
    let unit = cmp.units[u as usize];
    let (l, r) = (unit.left as usize, unit.right as usize);
    cmp.left.split(l, usize::MAX, ls);
    cmp.right.split(r, usize::MAX, rs);
    for k in cmp.changed_pairs(u as usize).filter(|k| skip.binary_search(k).is_err()) {
        let p = cmp.mapping.pairs[k as usize];
        let lv = cmp.left.value(l, ls[p.left as usize]);
        let rv = cmp.right.value(r, rs[p.right as usize]);
        out.extend_from_slice(&k.to_le_bytes());
        out.extend_from_slice(&(lv.len() as u64).to_le_bytes());
        out.extend_from_slice(lv.as_bytes());
        out.extend_from_slice(&(rv.len() as u64).to_le_bytes());
        out.extend_from_slice(rv.as_bytes());
    }
}

/// Full change patterns over the pairs not skipped: hashed, sorted, and
/// verified by value within each run of equal hashes.
fn full_without(
    cmp: &Comparison,
    changed: &[u32],
    skip: &[u32],
    cancel: &CancelToken,
) -> Result<(Vec<u32>, Vec<Run>), CompareError> {
    let stop = AtomicBool::new(false);
    let cancelled = || {
        if cancel.is_cancelled() {
            stop.store(true, Ordering::Relaxed);
        }
        stop.load(Ordering::Relaxed)
    };
    let mut sigs: Vec<(u64, u32)> = changed
        .par_chunks(4096)
        .flat_map_iter(|chunk| {
            let (mut ls, mut rs, mut buf) = (Vec::new(), Vec::new(), Vec::new());
            let skip_all = cancelled();
            chunk
                .iter()
                .filter(move |_| !skip_all)
                .map(move |&u| {
                    signature(cmp, u, skip, &mut ls, &mut rs, &mut buf);
                    (cmp.hasher.hash_one(&buf[..]), u)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    if cancelled() {
        return Err(CompareError::Cancelled);
    }
    sigs.par_sort_unstable();
    let mut bounds = Vec::new();
    let mut a = 0;
    for i in 1..=sigs.len() {
        if i == sigs.len() || sigs[i].0 != sigs[a].0 {
            bounds.push((a, i));
            a = i;
        }
    }
    let checked: Vec<Vec<Vec<u32>>> = bounds
        .par_iter()
        .map(|&(a, b)| {
            if b - a == 1 || cancelled() {
                return Vec::new();
            }
            let (mut ls, mut rs, mut buf) = (Vec::new(), Vec::new(), Vec::new());
            let mut reps: Vec<(Vec<u8>, Vec<u32>)> = Vec::new();
            for &(_, u) in &sigs[a..b] {
                signature(cmp, u, skip, &mut ls, &mut rs, &mut buf);
                match reps.iter_mut().find(|(rep, _)| *rep == buf) {
                    Some((_, m)) => m.push(u),
                    None => reps.push((buf.clone(), vec![u])),
                }
            }
            if reps.len() == 1 {
                return Vec::new();
            }
            reps.into_iter().map(|(_, m)| m).collect()
        })
        .collect();
    if cancelled() {
        return Err(CompareError::Cancelled);
    }
    let mut members: Vec<u32> = sigs.iter().map(|s| s.1).collect();
    let runs = split_runs(&mut members, &bounds, &checked, |_| NONE);
    Ok((members, runs))
}

/// For each run of `cells` (hash, pair, unit) in `bounds`, whether its
/// members' values differ, which only a hash collision makes them do. Every
/// member is compared with its run's first member by value. The records are
/// gone through in ascending order, each split once, so that a record with
/// many changed columns is not split again for each of them; a run's first
/// member, its lowest record, always comes before its other members.
fn collisions(
    cmp: &Comparison,
    cells: &[(u64, u32, u32)],
    bounds: &[(usize, usize)],
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Vec<bool> {
    let mut collided = vec![false; bounds.len()];
    // Runs of more than one member, by index into `bounds`.
    let multi: Vec<usize> = (0..bounds.len()).filter(|&i| bounds[i].1 - bounds[i].0 > 1).collect();
    let mut visits: Vec<(u32, u32)> =
        multi.iter().flat_map(|&i| (bounds[i].0..bounds[i].1).map(|c| (cells[c].2, c as u32))).collect();
    visits.par_sort_unstable();
    let run_of = |c: usize| multi.partition_point(|&i| bounds[i].0 <= c) - 1;
    let mut first: Vec<Option<(std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>)>> = vec![None; multi.len()];
    let (mut ls, mut rs) = (Vec::new(), Vec::new());
    let mut split = NONE;
    for (n, &(u, c)) in visits.iter().enumerate() {
        if n % 4096 == 0 && cancelled() {
            break;
        }
        let unit = cmp.units[u as usize];
        let (l, r) = (unit.left as usize, unit.right as usize);
        if u != split {
            cmp.left.split(l, usize::MAX, &mut ls);
            cmp.right.split(r, usize::MAX, &mut rs);
            split = u;
        }
        let p = cmp.mapping.pairs[cells[c as usize].1 as usize];
        let v = (cmp.left.value(l, ls[p.left as usize]), cmp.right.value(r, rs[p.right as usize]));
        let m = run_of(c as usize);
        match &first[m] {
            None => first[m] = Some(v),
            Some(f) if *f != v => collided[multi[m]] = true,
            Some(_) => {}
        }
    }
    collided
}

/// Runs from the hash runs `bounds`; a run that `checked` split (a hash
/// collision) becomes one run per part, with its members reordered.
fn split_runs(
    members: &mut [u32],
    bounds: &[(usize, usize)],
    checked: &[Vec<Vec<u32>>],
    pair_of: impl Fn(usize) -> u32,
) -> Vec<Run> {
    let mut runs = Vec::with_capacity(bounds.len());
    for (i, &(a, b)) in bounds.iter().enumerate() {
        let k = pair_of(a);
        if checked[i].is_empty() {
            runs.push((a as u32, (b - a) as u32, k, 0));
            continue;
        }
        let mut at = a;
        for part in &checked[i] {
            members[at..at + part.len()].copy_from_slice(part);
            runs.push((at as u32, part.len() as u32, k, 0));
            at += part.len();
        }
    }
    runs
}

fn per_column(
    cmp: &Comparison,
    changed: &[u32],
    skip: &[u32],
    cancel: &CancelToken,
) -> Result<(Vec<u32>, Vec<Run>), CompareError> {
    let stop = AtomicBool::new(false);
    let cancelled = || {
        if cancel.is_cancelled() {
            stop.store(true, Ordering::Relaxed);
        }
        stop.load(Ordering::Relaxed)
    };
    // (hash of pair and values, pair, unit) for every changed cell.
    let chunks: Vec<Vec<(u64, u32, u32)>> = changed
        .par_chunks(4096)
        .map(|chunk| {
            let mut out = Vec::new();
            if cancelled() {
                return out;
            }
            let (mut ls, mut rs) = (Vec::new(), Vec::new());
            for &u in chunk {
                let unit = cmp.units[u as usize];
                let (l, r) = (unit.left as usize, unit.right as usize);
                cmp.left.split(l, usize::MAX, &mut ls);
                cmp.right.split(r, usize::MAX, &mut rs);
                for k in cmp.changed_pairs(u as usize).filter(|k| skip.binary_search(k).is_err()) {
                    let p = cmp.mapping.pairs[k as usize];
                    let lv = cmp.left.value(l, ls[p.left as usize]);
                    let rv = cmp.right.value(r, rs[p.right as usize]);
                    let mut h = cmp.hasher.build_hasher();
                    h.write_u32(k);
                    h.write_usize(lv.len());
                    h.write(lv.as_bytes());
                    h.write_usize(rv.len());
                    h.write(rv.as_bytes());
                    out.push((h.finish(), k, u));
                }
            }
            out
        })
        .collect();
    if cancelled() {
        return Err(CompareError::Cancelled);
    }
    let mut cells: Vec<(u64, u32, u32)> = Vec::with_capacity(chunks.iter().map(Vec::len).sum());
    for c in chunks {
        cells.extend_from_slice(&c);
    }
    // Pair first, then hash, then unit: members of a run come out ascending.
    cells.par_sort_unstable_by_key(|&(h, k, u)| (k, h, u));
    let mut bounds = Vec::new();
    let mut a = 0;
    for i in 1..=cells.len() {
        if i == cells.len() || (cells[i].0, cells[i].1) != (cells[a].0, cells[a].1) {
            bounds.push((a, i));
            a = i;
        }
    }
    let mut members: Vec<u32> = cells.iter().map(|c| c.2).collect();
    let pairs_of = |k: usize| cells[k].1;
    let collided = collisions(cmp, &cells, &bounds, &cancelled);
    if cancelled() {
        return Err(CompareError::Cancelled);
    }
    // A run whose members differ (a hash collision) is split by value,
    // reordering its members.
    let checked: Vec<Vec<Vec<u32>>> = bounds
        .par_iter()
        .zip(collided.par_iter())
        .map(|(&(a, b), &collided)| {
            if !collided || cancelled() {
                return Vec::new();
            }
            let k = pairs_of(a);
            let first = cell_values(cmp, cells[a].2, k);
            let mut rest: Vec<(std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>, Vec<u32>)> = Vec::new();
            let mut same = vec![cells[a].2];
            for c in &cells[a + 1..b] {
                let v = cell_values(cmp, c.2, k);
                if v == first {
                    same.push(c.2);
                } else {
                    match rest.iter_mut().find(|(x, y, _)| (x, y) == (&v.0, &v.1)) {
                        Some((_, _, m)) => m.push(c.2),
                        None => rest.push((v.0, v.1, vec![c.2])),
                    }
                }
            }
            if rest.is_empty() {
                return Vec::new();
            }
            let mut parts = vec![same];
            parts.extend(rest.into_iter().map(|(_, _, m)| m));
            parts
        })
        .collect();
    if cancelled() {
        return Err(CompareError::Cancelled);
    }
    let mut runs = split_runs(&mut members, &bounds, &checked, pairs_of);
    drop(cells);
    // Members that also changed in another column (columns left out of the
    // grouping do not count: they are the noise the user set aside).
    runs.par_iter_mut().for_each(|run| {
        let s = &members[run.0 as usize..(run.0 + run.1) as usize];
        let counted =
            |u: u32| cmp.changed_pairs(u as usize).filter(|k| skip.binary_search(k).is_err()).nth(1).is_some();
        run.3 = s.iter().filter(|&&u| counted(u)).count() as u32;
    });
    Ok((members, runs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::compare_tables;
    use crate::{LoadOptions, Progress, Table, TrimRule};

    fn cmp(a: &str, b: &str) -> Comparison {
        let t = |s: &str| Table::from_bytes("t.csv", s.as_bytes().to_vec(), &LoadOptions::default()).unwrap();
        let keys = ["id".to_string()];
        compare_tables(t(a), t(b), &keys, &[], TrimRule::None, &CancelToken::new(), &Progress::default()).unwrap()
    }

    /// Cells that share a hash but not their values are told apart, also
    /// when a record's other changed columns come between them.
    #[test]
    fn a_run_whose_values_differ_is_found() {
        let c = cmp("id,a,b\n1,x,p\n2,x,p\n3,y,p\n", "id,a,b\n1,X,q\n2,X,q\n3,X,q\n");
        // (hash, pair, unit): one run for each column, with a made-up hash
        // shared by values that differ in column a (unit 2 is y → X).
        let (a, b) = (1, 2);
        let cells = [(7, a, 0), (7, a, 1), (7, a, 2), (9, b, 0), (9, b, 1), (9, b, 2)];
        let bounds = [(0, 3), (3, 6)];
        assert_eq!(collisions(&c, &cells, &bounds, &|| false), [true, false]);
        let one = [(7, a, 0), (7, a, 1)];
        assert_eq!(collisions(&c, &one, &[(0, 2)], &|| false), [false]);
    }
}
