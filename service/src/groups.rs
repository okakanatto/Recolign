//! Change patterns: the changed records grouped by a rule the user picks
//! (plan 6, U-16), cached per rule and columns left out.

use std::sync::Arc;

use cdr_engine::groups::{group, GroupRule, Groups};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{bad, conflict, default_count, parse, ApiError, Reply};
use crate::state::Ready;
use crate::Service;

/// One group, as the screen refers to it (in a view or a bulk change).
#[derive(Clone, Deserialize)]
pub(crate) struct GroupRef {
    pub(crate) rule: GroupRule,
    pub(crate) id: u32,
    /// Columns left out of the grouping, by name.
    #[serde(default)]
    pub(crate) skip: Vec<String>,
}

#[derive(Deserialize)]
struct GroupsReq {
    rule: GroupRule,
    #[serde(default)]
    skip: Vec<String>,
    #[serde(default)]
    start: usize,
    #[serde(default = "default_count")]
    count: usize,
}

impl Service {
    /// Groups by `rule`, leaving out the columns named in `skip` (names that
    /// are not compared columns are ignored). Kept for the comparison.
    pub(crate) fn groups(&self, r: &Ready, rule: GroupRule, skip: &[String]) -> Result<Arc<Groups>, ApiError> {
        let c = &r.cmp;
        let mut pairs: Vec<u32> = (0..c.mapping.pairs.len() as u32)
            .filter(|&k| skip.iter().any(|n| *n == c.left.headers[c.mapping.pairs[k as usize].left as usize]))
            .collect();
        pairs.sort_unstable();
        let key = (rule, pairs);
        if let Some(g) = r.groups.lock().unwrap().get(&key) {
            return Ok(g.clone());
        }
        let patterns = self.patterns.read().unwrap().clone();
        if rule == GroupRule::Full && key.1.is_empty() && patterns.is_none() {
            if let Some(why) = self.patterns_error.read().unwrap().clone() {
                return Err(bad(why));
            }
            return Err(conflict("変更パターンを集計中です。"));
        }
        // Stopped with the comparison: going back to choosing files, or
        // starting another comparison, does not wait for it.
        let cancel = self.current.lock().unwrap().cancel.clone();
        let g = Arc::new(group(c, patterns.as_deref(), rule, &key.1, &cancel).map_err(|e| bad(e.to_string()))?);
        let mut cache = r.groups.lock().unwrap();
        if cache.len() >= 8 {
            cache.clear();
        }
        cache.insert(key, g.clone());
        Ok(g)
    }

    pub(crate) fn groups_page(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: GroupsReq = parse(args)?;
        let g = self.groups(&r, q.rule, &q.skip)?;
        let c = &r.cmp;
        let name = |k: u32| c.left.headers[c.mapping.pairs[k as usize].left as usize].clone();
        let cut = |v: &str| match v.char_indices().nth(80) {
            Some((i, _)) => format!("{}…", &v[..i]),
            None => v.to_string(),
        };
        let (mut ls, mut rs) = (Vec::new(), Vec::new());
        let items: Vec<Value> = g
            .list
            .iter()
            .skip(q.start)
            .take(q.count.min(500))
            .map(|grp| {
                let pairs = g.pairs(c, grp);
                let changes: Vec<Value> = match (q.rule, g.values(c, grp)) {
                    (GroupRule::Columns, _) => pairs.iter().map(|&k| json!({ "name": name(k) })).collect(),
                    (_, Some((b, a))) => vec![json!({ "name": name(pairs[0]), "before": cut(&b), "after": cut(&a) })],
                    (_, None) => {
                        // Full: every changed column of the first member, which all members share.
                        let u = g.first(grp) as usize;
                        let unit = c.units[u];
                        c.left.split(unit.left as usize, usize::MAX, &mut ls);
                        c.right.split(unit.right as usize, usize::MAX, &mut rs);
                        pairs
                            .iter()
                            .map(|&k| {
                                let p = c.mapping.pairs[k as usize];
                                json!({
                                    "name": name(k),
                                    "before": cut(&c.left.value(unit.left as usize, ls[p.left as usize])),
                                    "after": cut(&c.right.value(unit.right as usize, rs[p.right as usize])),
                                })
                            })
                            .collect()
                    }
                };
                json!({ "id": grp.id, "count": grp.len(), "with_other": grp.with_other, "first_unit": g.first(grp), "changes": changes })
            })
            .collect();
        Ok(json!({
            "rule": q.rule,
            "total": g.list.len(),
            "singles": g.singles,
            "changed_units": g.changed_units,
            "only_skipped": g.only_skipped,
            "skip": g.skip.iter().map(|&k| name(k)).collect::<Vec<_>>(),
            "items": items,
        }))
    }
}
