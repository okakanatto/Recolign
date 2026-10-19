//! Column correspondence between the two files.
//!
//! Columns are identified by their original position. Automatic mapping by
//! name is only done when every name on both sides is non-empty and unique;
//! otherwise the caller must give the mapping explicitly. Columns are never
//! paired because they sit at the same position.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::CompareError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColumnPair {
    /// 0-based column position in the left file.
    pub left: u32,
    /// 0-based column position in the right file.
    pub right: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct ColumnMapping {
    /// In left column order.
    pub pairs: Vec<ColumnPair>,
    pub left_only: Vec<u32>,
    pub right_only: Vec<u32>,
    pub explicit: bool,
}

fn name_problems(side: &str, headers: &[String], out: &mut Vec<String>) {
    let mut seen: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, h) in headers.iter().enumerate() {
        if h.is_empty() {
            out.push(format!("{side}の第{}列の列名が空です。", i + 1));
        } else {
            seen.entry(h.as_str()).or_default().push(i);
        }
    }
    let mut dups: Vec<(&str, Vec<usize>)> = seen.into_iter().filter(|(_, v)| v.len() > 1).collect();
    dups.sort_by_key(|(_, v)| v[0]);
    for (name, cols) in dups {
        let list: Vec<String> = cols.iter().map(|c| format!("第{}列", c + 1)).collect();
        out.push(format!("{side}の{}が同じ列名「{name}」です。", list.join("と")));
    }
}

impl ColumnMapping {
    /// Pairs columns with the same name. Fails if any name is empty or
    /// repeated on either side.
    pub fn by_name(left: &[String], right: &[String]) -> Result<Self, CompareError> {
        let mut problems = Vec::new();
        name_problems("左", left, &mut problems);
        name_problems("右", right, &mut problems);
        if !problems.is_empty() {
            return Err(CompareError::ColumnMapping { problems });
        }
        let right_pos: HashMap<&str, usize> = right.iter().enumerate().map(|(i, h)| (h.as_str(), i)).collect();
        let mut pairs = Vec::new();
        let mut left_only = Vec::new();
        let mut used = vec![false; right.len()];
        for (i, h) in left.iter().enumerate() {
            match right_pos.get(h.as_str()) {
                Some(&j) => {
                    pairs.push(ColumnPair { left: i as u32, right: j as u32 });
                    used[j] = true;
                }
                None => left_only.push(i as u32),
            }
        }
        let right_only = (0..right.len()).filter(|&j| !used[j]).map(|j| j as u32).collect();
        Ok(ColumnMapping { pairs, left_only, right_only, explicit: false })
    }

    /// Uses the given pairs. Each column may appear in at most one pair.
    pub fn explicit(mut pairs: Vec<ColumnPair>, nleft: usize, nright: usize) -> Result<Self, CompareError> {
        let mut problems = Vec::new();
        let mut used_l = vec![false; nleft];
        let mut used_r = vec![false; nright];
        for p in &pairs {
            let (l, r) = (p.left as usize, p.right as usize);
            if l >= nleft || r >= nright {
                problems.push(format!("列の対応（左 第{}列 ↔ 右 第{}列）が列数の範囲外です。", l + 1, r + 1));
                continue;
            }
            if std::mem::replace(&mut used_l[l], true) {
                problems.push(format!("左の第{}列が複数の対応に含まれています。", l + 1));
            }
            if std::mem::replace(&mut used_r[r], true) {
                problems.push(format!("右の第{}列が複数の対応に含まれています。", r + 1));
            }
        }
        if !problems.is_empty() {
            return Err(CompareError::ColumnMapping { problems });
        }
        pairs.sort_by_key(|p| p.left);
        let left_only = (0..nleft).filter(|&i| !used_l[i]).map(|i| i as u32).collect();
        let right_only = (0..nright).filter(|&i| !used_r[i]).map(|i| i as u32).collect();
        Ok(ColumnMapping { pairs, left_only, right_only, explicit: true })
    }

    /// For each left column, the index of its pair.
    pub fn pair_of_left(&self, nleft: usize) -> Vec<Option<u32>> {
        let mut v = vec![None; nleft];
        for (k, p) in self.pairs.iter().enumerate() {
            v[p.left as usize] = Some(k as u32);
        }
        v
    }

    /// For each right column, the index of its pair.
    pub fn pair_of_right(&self, nright: usize) -> Vec<Option<u32>> {
        let mut v = vec![None; nright];
        for (k, p) in self.pairs.iter().enumerate() {
            v[p.right as usize] = Some(k as u32);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn maps_by_name_regardless_of_order() {
        let m = ColumnMapping::by_name(&s(&["id", "a", "b", "x"]), &s(&["b", "id", "y", "a"])).unwrap();
        assert_eq!(
            m.pairs,
            vec![ColumnPair { left: 0, right: 1 }, ColumnPair { left: 1, right: 3 }, ColumnPair { left: 2, right: 0 }]
        );
        assert_eq!(m.left_only, vec![3]);
        assert_eq!(m.right_only, vec![2]);
    }

    #[test]
    fn refuses_empty_and_duplicate_names() {
        let e = ColumnMapping::by_name(&s(&["id", "", "a", "a"]), &s(&["id", "a"])).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("左の第2列の列名が空"), "{msg}");
        assert!(msg.contains("左の第3列と第4列が同じ列名「a」"), "{msg}");
    }

    #[test]
    fn explicit_mapping_checks_reuse() {
        assert!(ColumnMapping::explicit(
            vec![ColumnPair { left: 0, right: 0 }, ColumnPair { left: 1, right: 0 }],
            2,
            2
        )
        .is_err());
        let m = ColumnMapping::explicit(vec![ColumnPair { left: 1, right: 0 }], 2, 2).unwrap();
        assert_eq!(m.left_only, vec![0]);
        assert_eq!(m.right_only, vec![1]);
    }
}
