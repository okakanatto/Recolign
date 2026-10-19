//! Canonical JSON Lines export for the independent oracle (tools/oracle.py).
//!
//! Line 1 describes the inputs, the column mapping and the counts. Then one
//! line per unit in key-aligned order:
//!   {"u":unit,"t":kind,"k":[key...],
//!    "l":[record,line_start,line_end,byte_start,byte_end] | null,
//!    "r":[...] | null,
//!    "c":[[left_col,right_col,before,after],...],   changed
//!    "i":[[left_col,right_col,before,after],...],   ignored differences
//!    "p":pattern id | null}

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use cdr_engine::pattern::Patterns;
use cdr_engine::{Comparison, Table, NONE};
use serde_json::{json, Value};

fn pos(t: &Table, r: u32) -> Value {
    if r == NONE {
        return Value::Null;
    }
    let p = t.records[r as usize];
    json!([r as u64 + 1, p.line_start, p.line_end, p.start, p.end])
}

pub fn write(path: &Path, c: &Comparison, patterns: Option<&Patterns>) -> std::io::Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
    let head = json!({
        "left": { "headers": c.left.headers, "records": c.left.len(), "sha256": c.left.sha256() },
        "right": { "headers": c.right.headers, "records": c.right.len(), "sha256": c.right.sha256() },
        "pairs": c.mapping.pairs.iter().map(|p| [p.left, p.right]).collect::<Vec<_>>(),
        "left_only": c.mapping.left_only,
        "right_only": c.mapping.right_only,
        "keys": c.spec.keys,
        "counts": c.counts,
        "patterns": patterns.map(|p| p.list.len()),
    });
    serde_json::to_writer(&mut w, &head)?;
    w.write_all(b"\n")?;
    let mut ls = Vec::new();
    let mut rs = Vec::new();
    for (u, unit) in c.units.iter().enumerate() {
        if unit.left != NONE {
            c.left.split(unit.left as usize, usize::MAX, &mut ls);
        }
        if unit.right != NONE {
            c.right.split(unit.right as usize, usize::MAX, &mut rs);
        }
        let entry = |k: u32| {
            let p = c.mapping.pairs[k as usize];
            json!([
                p.left,
                p.right,
                c.left.value(unit.left as usize, ls[p.left as usize]),
                c.right.value(unit.right as usize, rs[p.right as usize]),
            ])
        };
        let line = json!({
            "u": u,
            "t": unit.kind,
            "k": c.key_values(u),
            "l": pos(&c.left, unit.left),
            "r": pos(&c.right, unit.right),
            "c": c.changed_pairs(u).map(entry).collect::<Vec<_>>(),
            "i": c.ignored_pairs(u).map(entry).collect::<Vec<_>>(),
            "p": patterns.map(|p| p.unit_pattern[u]).filter(|&p| p != NONE),
        });
        serde_json::to_writer(&mut w, &line)?;
        w.write_all(b"\n")?;
    }
    w.flush()
}
