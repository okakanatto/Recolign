//! Key validation, matching and classification.
//!
//! Units ("対応単位") are produced in the key-aligned display order: every
//! left record in left file order (same / changed / deleted), then every
//! right-only record in right file order (added). The unit index is therefore
//! stable for a given comparison and doubles as the record ID inside it.

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use hashbrown::hash_table::{Entry, HashTable};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::cancel::{CancelToken, Progress, SideProgress};
use crate::columns::ColumnMapping;
use crate::encoding::TextEncoding;
use crate::error::{CompareError, DuplicateGroup, KeyProblems, KeyValidationReport, RecordRef};
use crate::table::Table;
use crate::tokenizer::FieldSpan;

pub const NONE: u32 = u32::MAX;
/// Set on a diff entry whose difference is ignored by the comparison rules.
pub const IGNORED_BIT: u32 = 1 << 31;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrimRule {
    /// Values are compared exactly.
    #[default]
    None,
    /// Leading and trailing ASCII spaces and tabs are ignored in non-key
    /// columns. The difference is still recorded as an ignored difference.
    AsciiSpaceTab,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct CompareSpec {
    /// Indices into `ColumnMapping::pairs`.
    pub keys: Vec<u32>,
    /// Pair indices whose differences are recorded but not counted as changes.
    pub excluded: Vec<u32>,
    pub trim: TrimRule,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Kind {
    Same = 0,
    Changed = 1,
    Deleted = 2,
    Added = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unit {
    /// Left data record index, or `NONE`.
    pub left: u32,
    /// Right data record index, or `NONE`.
    pub right: u32,
    pub kind: Kind,
    /// Some value differs only in a way the comparison rules ignore.
    pub ignored: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Counts {
    pub left_records: u64,
    pub right_records: u64,
    pub units: u64,
    pub same: u64,
    /// Subset of `same` with ignored differences ("比較条件上の一致").
    pub same_with_ignored: u64,
    pub changed: u64,
    pub deleted: u64,
    pub added: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct CompareTimings {
    pub key_index: Duration,
    pub compare: Duration,
    pub finalize: Duration,
}

/// Open-addressing table from key hash to record index. Keys are not stored;
/// equality is checked by re-reading the key fields from the record.
pub(crate) struct KeyIndex {
    table: HashTable<(u64, u32)>,
    cols: Vec<usize>,
    limit: usize,
}

pub(crate) struct Scratch {
    spans: Vec<FieldSpan>,
    key: Vec<u8>,
}

impl Scratch {
    pub(crate) fn new() -> Self {
        Scratch { spans: Vec::new(), key: Vec::new() }
    }
}

/// Length-prefixed concatenation of decoded key values. Returns true if any
/// key value is empty.
fn encode_key(table: &Table, r: usize, spans: &[FieldSpan], cols: &[usize], out: &mut Vec<u8>) -> bool {
    out.clear();
    let mut empty = false;
    for &c in cols {
        let v = table.value(r, spans[c]);
        empty |= v.is_empty();
        out.extend_from_slice(&(v.len() as u64).to_le_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    empty
}

pub(crate) fn encode_key_strings(values: &[String], out: &mut Vec<u8>) {
    out.clear();
    for v in values {
        out.extend_from_slice(&(v.len() as u64).to_le_bytes());
        out.extend_from_slice(v.as_bytes());
    }
}

const LIST_CAP_GROUPS: usize = 1000;
const LIST_CAP_RECORDS: usize = 100;

fn record_ref(t: &Table, r: usize) -> RecordRef {
    let p = t.records[r];
    RecordRef { record: r as u64 + 1, line_start: p.line_start, line_end: p.line_end }
}

enum KeyBuildError {
    Cancelled,
    Problems(KeyProblems),
}

impl KeyIndex {
    fn build(
        table: &Table,
        cols: Vec<usize>,
        hasher: &ahash::RandomState,
        cancel: &CancelToken,
        progress: &SideProgress,
    ) -> Result<KeyIndex, KeyBuildError> {
        let limit = cols.iter().max().map_or(0, |m| m + 1);
        let n = table.len();
        let mut map: HashTable<(u64, u32)> = HashTable::with_capacity(n);
        let mut a = Scratch::new();
        let mut b = Scratch::new();
        let mut problems = KeyProblems::default();
        let mut dups: HashMap<u32, Vec<u32>> = HashMap::new();
        for r in 0..n {
            if r % 65536 == 0 {
                if cancel.is_cancelled() {
                    return Err(KeyBuildError::Cancelled);
                }
                progress.keys.store(r as u64, Ordering::Relaxed);
            }
            table.split(r, limit, &mut a.spans);
            if encode_key(table, r, &a.spans, &cols, &mut a.key) {
                problems.empty_total += 1;
                if problems.empty.len() < LIST_CAP_GROUPS {
                    problems.empty.push(record_ref(table, r));
                }
                continue;
            }
            let h = hasher.hash_one(&a.key[..]);
            let entry = map.entry(
                h,
                |&(hh, idx)| {
                    hh == h && {
                        table.split(idx as usize, limit, &mut b.spans);
                        encode_key(table, idx as usize, &b.spans, &cols, &mut b.key);
                        b.key == a.key
                    }
                },
                |&(hh, _)| hh,
            );
            match entry {
                Entry::Occupied(o) => dups.entry(o.get().1).or_default().push(r as u32),
                Entry::Vacant(v) => {
                    v.insert((h, r as u32));
                }
            }
        }
        progress.keys.store(n as u64, Ordering::Relaxed);
        if !dups.is_empty() {
            problems.duplicate_groups_total = dups.len() as u64;
            problems.duplicate_records_total = dups.values().map(|v| v.len() as u64 + 1).sum();
            let mut firsts: Vec<u32> = dups.keys().copied().collect();
            firsts.sort_unstable();
            for first in firsts.into_iter().take(LIST_CAP_GROUPS) {
                let mut spans = Vec::new();
                table.split(first as usize, limit, &mut spans);
                let key = cols.iter().map(|&c| table.value(first as usize, spans[c]).into_owned()).collect();
                let mut records = vec![record_ref(table, first as usize)];
                records.extend(dups[&first].iter().take(LIST_CAP_RECORDS - 1).map(|&r| record_ref(table, r as usize)));
                problems.duplicate_groups.push(DuplicateGroup { key, records });
            }
        }
        if problems.is_empty() {
            Ok(KeyIndex { table: map, cols, limit })
        } else {
            Err(KeyBuildError::Problems(problems))
        }
    }

    /// Record whose key bytes equal `key` (encoded with `encode_key`).
    fn find(&self, table: &Table, key: &[u8], h: u64, s: &mut Scratch) -> Option<u32> {
        self.table
            .find(h, |&(hh, idx)| {
                hh == h && {
                    table.split(idx as usize, self.limit, &mut s.spans);
                    encode_key(table, idx as usize, &s.spans, &self.cols, &mut s.key);
                    s.key == key
                }
            })
            .map(|&(_, idx)| idx)
    }
}

pub struct Comparison {
    pub left: Table,
    pub right: Table,
    pub mapping: ColumnMapping,
    pub spec: CompareSpec,
    pub units: Vec<Unit>,
    diff_offsets: Vec<u64>,
    diff_entries: Vec<u32>,
    /// Right data record index → unit index.
    pub right_unit: Vec<u32>,
    /// Units whose kind is not `Same`, ascending.
    pub diff_units: Vec<u32>,
    pub counts: Counts,
    /// (signature hash, unit) for every changed unit, in unit order.
    pub(crate) signatures: Vec<(u64, u32)>,
    pub(crate) hasher: ahash::RandomState,
    key_left: KeyIndex,
    key_right: KeyIndex,
    pub timings: CompareTimings,
}

struct ComparePlan {
    /// (pair index, left column, right column, excluded)
    pairs: Vec<(u32, usize, usize, bool)>,
    same_encoding: bool,
    both_utf8: bool,
    trim: TrimRule,
}

#[derive(Default)]
struct ChunkOut {
    units: Vec<Unit>,
    diff_counts: Vec<u32>,
    entries: Vec<u32>,
    signatures: Vec<(u64, u32)>,
}

fn trim_ascii_ws(s: &str) -> &str {
    s.trim_matches(|c| c == ' ' || c == '\t')
}

impl ComparePlan {
    /// Equality of two field values: raw bytes first, decoded text only when
    /// the raw bytes cannot decide.
    #[inline]
    fn values_equal(&self, lt: &Table, l: usize, ls: FieldSpan, rt: &Table, r: usize, rs: FieldSpan) -> bool {
        let a = lt.field_raw(l, ls);
        let b = rt.field_raw(r, rs);
        if self.same_encoding && ls.escaped == rs.escaped && a == b {
            return true;
        }
        if self.both_utf8 && !ls.escaped && !rs.escaped {
            return false;
        }
        lt.value(l, ls) == rt.value(r, rs)
    }
}

const CHUNK: usize = 8192;

impl Comparison {
    pub fn run(
        left: Table,
        right: Table,
        mapping: ColumnMapping,
        spec: CompareSpec,
        cancel: &CancelToken,
        progress: &Progress,
    ) -> Result<Comparison, CompareError> {
        if spec.keys.is_empty() {
            return Err(CompareError::NoKey);
        }
        let mut seen = vec![false; mapping.pairs.len()];
        for &k in &spec.keys {
            let Some(slot) = seen.get_mut(k as usize) else {
                return Err(CompareError::KeyColumn {
                    message: "突合キーに存在しない列の対応が指定されています。".into(),
                });
            };
            if std::mem::replace(slot, true) {
                return Err(CompareError::KeyColumn {
                    message: "突合キーに同じ列が重複して指定されています。重複のない列を指定してください。".into(),
                });
            }
        }
        if left.len() + right.len() >= NONE as usize {
            return Err(CompareError::TooManyUnits);
        }
        let hasher = ahash::RandomState::new();
        let lcols: Vec<usize> = spec.keys.iter().map(|&k| mapping.pairs[k as usize].left as usize).collect();
        let rcols: Vec<usize> = spec.keys.iter().map(|&k| mapping.pairs[k as usize].right as usize).collect();

        let t = Instant::now();
        progress.set_stage(crate::cancel::Stage::ValidatingKeys);
        let (kl, kr) = rayon::join(
            || KeyIndex::build(&left, lcols, &hasher, cancel, &progress.left),
            || KeyIndex::build(&right, rcols, &hasher, cancel, &progress.right),
        );
        let key_index = t.elapsed();
        let (key_left, key_right) = match (kl, kr) {
            (Err(KeyBuildError::Cancelled), _) | (_, Err(KeyBuildError::Cancelled)) => {
                return Err(CompareError::Cancelled)
            }
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                let take = |r: Result<KeyIndex, KeyBuildError>| match r {
                    Err(KeyBuildError::Problems(p)) => p,
                    _ => KeyProblems::default(),
                };
                return Err(CompareError::KeyValidation(Box::new(KeyValidationReport {
                    left_name: left.info.name.clone(),
                    right_name: right.info.name.clone(),
                    left: take(a),
                    right: take(b),
                })));
            }
        };

        let excluded: Vec<bool> = {
            let mut v = vec![false; mapping.pairs.len()];
            for &e in &spec.excluded {
                if let Some(x) = v.get_mut(e as usize) {
                    *x = true;
                }
            }
            v
        };
        let plan = ComparePlan {
            pairs: mapping
                .pairs
                .iter()
                .enumerate()
                .filter(|(k, _)| !seen[*k])
                .map(|(k, p)| (k as u32, p.left as usize, p.right as usize, excluded[k]))
                .collect(),
            same_encoding: left.text_encoding() == right.text_encoding(),
            both_utf8: left.text_encoding() == TextEncoding::Utf8 && right.text_encoding() == TextEncoding::Utf8,
            trim: spec.trim,
        };

        let t = Instant::now();
        progress.set_stage(crate::cancel::Stage::Comparing);
        progress.units_total.store(left.len() as u64, Ordering::Relaxed);
        let nl = left.len();
        let chunks: Vec<ChunkOut> = (0..nl.div_ceil(CHUNK))
            .into_par_iter()
            .map(|ci| {
                let mut out = ChunkOut::default();
                if cancel.is_cancelled() {
                    return out;
                }
                let a = ci * CHUNK;
                let b = (a + CHUNK).min(nl);
                let mut ls = Vec::new();
                let mut rs = Vec::new();
                let mut key = Vec::new();
                let mut s = Scratch::new();
                out.units.reserve(b - a);
                out.diff_counts.reserve(b - a);
                for l in a..b {
                    left.split(l, usize::MAX, &mut ls);
                    encode_key(&left, l, &ls, &key_left.cols, &mut key);
                    let h = hasher.hash_one(&key[..]);
                    let Some(r) = key_right.find(&right, &key, h, &mut s) else {
                        out.units.push(Unit { left: l as u32, right: NONE, kind: Kind::Deleted, ignored: false });
                        out.diff_counts.push(0);
                        continue;
                    };
                    let r = r as usize;
                    right.split(r, usize::MAX, &mut rs);
                    let before = out.entries.len();
                    let mut changed = false;
                    let mut ignored = false;
                    let mut sig = hasher.build_hasher();
                    for &(k, lc, rc, excl) in &plan.pairs {
                        if plan.values_equal(&left, l, ls[lc], &right, r, rs[rc]) {
                            continue;
                        }
                        if excl {
                            ignored = true;
                            out.entries.push(k | IGNORED_BIT);
                            continue;
                        }
                        let lv: Cow<str> = left.value(l, ls[lc]);
                        let rv: Cow<str> = right.value(r, rs[rc]);
                        if plan.trim == TrimRule::AsciiSpaceTab && trim_ascii_ws(&lv) == trim_ascii_ws(&rv) {
                            ignored = true;
                            out.entries.push(k | IGNORED_BIT);
                            continue;
                        }
                        changed = true;
                        out.entries.push(k);
                        sig.write_u32(k);
                        sig.write_usize(lv.len());
                        sig.write(lv.as_bytes());
                        sig.write_usize(rv.len());
                        sig.write(rv.as_bytes());
                    }
                    if changed {
                        out.signatures.push((sig.finish(), l as u32));
                    }
                    out.units.push(Unit {
                        left: l as u32,
                        right: r as u32,
                        kind: if changed { Kind::Changed } else { Kind::Same },
                        ignored,
                    });
                    out.diff_counts.push((out.entries.len() - before) as u32);
                }
                progress.units_compared.fetch_add((b - a) as u64, Ordering::Relaxed);
                out
            })
            .collect();
        if cancel.is_cancelled() {
            return Err(CompareError::Cancelled);
        }
        let compare = t.elapsed();

        let t = Instant::now();
        let nr = right.len();
        let mut units = Vec::with_capacity(nl + nr);
        let mut diff_offsets = Vec::with_capacity(nl + nr + 1);
        let entries_total: usize = chunks.iter().map(|c| c.entries.len()).sum();
        let mut diff_entries = Vec::with_capacity(entries_total);
        let mut signatures = Vec::with_capacity(chunks.iter().map(|c| c.signatures.len()).sum());
        diff_offsets.push(0u64);
        let mut right_unit = vec![NONE; nr];
        for ch in chunks {
            for (u, &dc) in ch.units.iter().zip(&ch.diff_counts) {
                if u.right != NONE {
                    right_unit[u.right as usize] = units.len() as u32;
                }
                units.push(*u);
                let last = *diff_offsets.last().expect("offset");
                diff_offsets.push(last + dc as u64);
            }
            diff_entries.extend_from_slice(&ch.entries);
            signatures.extend_from_slice(&ch.signatures);
        }
        let end = *diff_offsets.last().expect("offset");
        for (r, slot) in right_unit.iter_mut().enumerate() {
            if *slot == NONE {
                *slot = units.len() as u32;
                units.push(Unit { left: NONE, right: r as u32, kind: Kind::Added, ignored: false });
                diff_offsets.push(end);
            }
        }
        let mut counts = Counts {
            left_records: nl as u64,
            right_records: nr as u64,
            units: units.len() as u64,
            ..Default::default()
        };
        let mut diff_units = Vec::new();
        for (i, u) in units.iter().enumerate() {
            match u.kind {
                Kind::Same => {
                    counts.same += 1;
                    counts.same_with_ignored += u.ignored as u64;
                }
                Kind::Changed => counts.changed += 1,
                Kind::Deleted => counts.deleted += 1,
                Kind::Added => counts.added += 1,
            }
            if u.kind != Kind::Same {
                diff_units.push(i as u32);
            }
        }
        let finalize = t.elapsed();
        Ok(Comparison {
            left,
            right,
            mapping,
            spec,
            units,
            diff_offsets,
            diff_entries,
            right_unit,
            diff_units,
            counts,
            signatures,
            hasher,
            key_left,
            key_right,
            timings: CompareTimings { key_index, compare, finalize },
        })
    }

    /// Raw diff entries of unit `u` (pair index, possibly with `IGNORED_BIT`).
    pub fn diff_entries(&self, u: usize) -> &[u32] {
        &self.diff_entries[self.diff_offsets[u] as usize..self.diff_offsets[u + 1] as usize]
    }

    /// Pair indices whose values changed.
    pub fn changed_pairs(&self, u: usize) -> impl Iterator<Item = u32> + '_ {
        self.diff_entries(u).iter().filter(|&&e| e & IGNORED_BIT == 0).copied()
    }

    /// Pair indices whose values differ only in an ignored way.
    pub fn ignored_pairs(&self, u: usize) -> impl Iterator<Item = u32> + '_ {
        self.diff_entries(u).iter().filter(|&&e| e & IGNORED_BIT != 0).map(|&e| e & !IGNORED_BIT)
    }

    pub fn is_key_pair(&self, k: u32) -> bool {
        self.spec.keys.contains(&k)
    }

    /// For each pair: the number of units in which its value changed, and
    /// the number in which it differed only in a way the rules ignore.
    pub fn pair_change_counts(&self) -> (Vec<u64>, Vec<u64>) {
        let n = self.mapping.pairs.len();
        let mut changed = vec![0u64; n];
        let mut ignored = vec![0u64; n];
        for &u in &self.diff_units {
            for &e in self.diff_entries(u as usize) {
                if e & IGNORED_BIT == 0 {
                    changed[e as usize] += 1;
                } else {
                    ignored[(e & !IGNORED_BIT) as usize] += 1;
                }
            }
        }
        // Same units can carry ignored differences too.
        if self.counts.same_with_ignored > 0 {
            for (u, unit) in self.units.iter().enumerate() {
                if unit.kind == Kind::Same && unit.ignored {
                    for &e in self.diff_entries(u) {
                        ignored[(e & !IGNORED_BIT) as usize] += 1;
                    }
                }
            }
        }
        (changed, ignored)
    }

    /// Key values of unit `u`, taken from the left record when present.
    pub fn key_values(&self, u: usize) -> Vec<String> {
        let unit = self.units[u];
        let (t, r, cols) = if unit.left != NONE {
            (&self.left, unit.left as usize, &self.key_left.cols)
        } else {
            (&self.right, unit.right as usize, &self.key_right.cols)
        };
        let mut spans = Vec::new();
        t.split(r, usize::MAX, &mut spans);
        cols.iter().map(|&c| t.value(r, spans[c]).into_owned()).collect()
    }

    /// Unit whose key equals `key` exactly.
    pub fn find_key(&self, key: &[String]) -> Option<u32> {
        if key.len() != self.spec.keys.len() {
            return None;
        }
        let mut buf = Vec::new();
        encode_key_strings(key, &mut buf);
        let h = self.hasher.hash_one(&buf[..]);
        let mut s = Scratch::new();
        if let Some(l) = self.key_left.find(&self.left, &buf, h, &mut s) {
            return Some(l);
        }
        self.key_right.find(&self.right, &buf, h, &mut s).map(|r| self.right_unit[r as usize])
    }

    /// Memory held by the comparison's own index structures, in bytes
    /// (file contents excluded).
    pub fn index_bytes(&self) -> u64 {
        let rec = std::mem::size_of::<crate::tokenizer::RecordPos>() as u64;
        (self.left.records.capacity() as u64 + self.right.records.capacity() as u64) * rec
            + self.units.capacity() as u64 * std::mem::size_of::<Unit>() as u64
            + self.diff_offsets.capacity() as u64 * 8
            + self.diff_entries.capacity() as u64 * 4
            + self.right_unit.capacity() as u64 * 4
            + self.diff_units.capacity() as u64 * 4
            + self.signatures.capacity() as u64 * 16
            + (self.key_left.table.capacity() as u64 + self.key_right.table.capacity() as u64) * 17
    }
}
