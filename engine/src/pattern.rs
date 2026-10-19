//! Full change patterns: changed units whose set of changed columns and
//! before/after values are all identical.
//!
//! Signature hashes computed during comparison only group candidates. Every
//! unit that shares a hash with another unit is verified by re-reading its
//! values, so a hash collision cannot merge two different patterns.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use serde::Serialize;

use crate::cancel::CancelToken;
use crate::compare::{Comparison, NONE};
use crate::error::CompareError;
use crate::tokenizer::FieldSpan;

#[derive(Clone, Debug, Serialize)]
pub struct Pattern {
    pub id: u32,
    /// Member units, ascending. The first one is the representative.
    pub units: Vec<u32>,
    /// Changed pair indices, ascending.
    pub pairs: Vec<u32>,
}

pub struct Patterns {
    /// Sorted by member count (descending), then first unit.
    pub list: Vec<Pattern>,
    /// Pattern id per unit, `NONE` for units that are not changed.
    pub unit_pattern: Vec<u32>,
    /// Distinct signatures that shared a 64-bit hash (normally 0).
    pub hash_collisions: u64,
    pub elapsed: Duration,
}

/// Structured signature: for each changed pair, its index and the
/// length-prefixed before/after values.
pub(crate) fn signature_bytes(
    cmp: &Comparison,
    u: usize,
    ls: &mut Vec<FieldSpan>,
    rs: &mut Vec<FieldSpan>,
    out: &mut Vec<u8>,
) {
    out.clear();
    let unit = cmp.units[u];
    let (l, r) = (unit.left as usize, unit.right as usize);
    cmp.left.split(l, usize::MAX, ls);
    cmp.right.split(r, usize::MAX, rs);
    for k in cmp.changed_pairs(u) {
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

pub fn aggregate(cmp: &Comparison, cancel: &CancelToken) -> Result<Patterns, CompareError> {
    let t = Instant::now();
    let mut sigs = cmp.signatures.clone();
    sigs.par_sort_unstable();
    let mut runs = Vec::new();
    let mut a = 0;
    for i in 1..=sigs.len() {
        if i == sigs.len() || sigs[i].0 != sigs[a].0 {
            runs.push((a, i));
            a = i;
        }
    }
    let collisions = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let groups: Vec<Vec<Vec<u32>>> = runs
        .par_iter()
        .map(|&(a, b)| {
            if b - a == 1 {
                return vec![vec![sigs[a].1]];
            }
            if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                stop.store(true, Ordering::Relaxed);
                return Vec::new();
            }
            let (mut ls, mut rs, mut buf) = (Vec::new(), Vec::new(), Vec::new());
            let mut reps: Vec<(Vec<u8>, Vec<u32>)> = Vec::new();
            for &(_, u) in &sigs[a..b] {
                signature_bytes(cmp, u as usize, &mut ls, &mut rs, &mut buf);
                match reps.iter_mut().find(|(rep, _)| *rep == buf) {
                    Some((_, members)) => members.push(u),
                    None => reps.push((buf.clone(), vec![u])),
                }
            }
            if reps.len() > 1 {
                collisions.fetch_add(reps.len() as u64 - 1, Ordering::Relaxed);
            }
            reps.into_iter().map(|(_, m)| m).collect()
        })
        .collect();
    if cancel.is_cancelled() || stop.load(Ordering::Relaxed) {
        return Err(CompareError::Cancelled);
    }
    let mut members: Vec<Vec<u32>> = groups.into_iter().flatten().collect();
    members.par_sort_unstable_by(|x, y| y.len().cmp(&x.len()).then(x[0].cmp(&y[0])));
    let mut unit_pattern = vec![NONE; cmp.units.len()];
    let list = members
        .into_iter()
        .enumerate()
        .map(|(id, units)| {
            for &u in &units {
                unit_pattern[u as usize] = id as u32;
            }
            let pairs = cmp.changed_pairs(units[0] as usize).collect();
            Pattern { id: id as u32, units, pairs }
        })
        .collect();
    Ok(Patterns { list, unit_pattern, hash_collisions: collisions.into_inner(), elapsed: t.elapsed() })
}
