//! AC-01 as a property: shuffling the rows and the columns of either file
//! does not change what each key is judged to be, nor which columns changed.

use std::collections::BTreeMap;

use cdr_engine::pipeline::compare_tables;
use cdr_engine::{CancelToken, Comparison, Kind, LoadOptions, Progress, Table, TrimRule};
use proptest::prelude::*;

/// A table: column names, then rows of values (the first column is the key).
type Rows = Vec<Vec<String>>;

fn csv(names: &[String], rows: &Rows, cols: &[usize], order: &[usize]) -> String {
    let mut s = cols.iter().map(|&c| names[c].clone()).collect::<Vec<_>>().join(",");
    s.push('\n');
    for &r in order {
        s += &cols.iter().map(|&c| rows[r][c].clone()).collect::<Vec<_>>().join(",");
        s.push('\n');
    }
    s
}

fn compare(l: &str, r: &str) -> Comparison {
    let t = |s: &str| Table::from_bytes("t.csv", s.as_bytes().to_vec(), &LoadOptions::default()).unwrap();
    compare_tables(t(l), t(r), &["id".to_string()], &[], TrimRule::None, &CancelToken::new(), &Progress::default())
        .unwrap()
}

/// Key → (kind, names of the changed columns).
fn outcome(c: &Comparison) -> BTreeMap<String, (Kind, Vec<String>)> {
    (0..c.units.len())
        .map(|u| {
            let mut cols: Vec<String> =
                c.changed_pairs(u).map(|k| c.left.headers[c.mapping.pairs[k as usize].left as usize].clone()).collect();
            cols.sort();
            (c.key_values(u).join("\u{1f}"), (c.units[u].kind, cols))
        })
        .collect()
}

fn side(n: usize) -> impl Strategy<Value = Rows> {
    // Keys drawn without repeats; few distinct values so that rows collide.
    (
        proptest::sample::subsequence((0..40).collect::<Vec<u32>>(), n),
        proptest::collection::vec(proptest::collection::vec(0u8..3, 3), n),
    )
        .prop_map(|(keys, vals)| {
            keys.iter()
                .zip(vals)
                .map(|(k, v)| {
                    let mut row = vec![format!("k{k:02}")];
                    row.extend(v.iter().map(|x| ["", "a", "0a"][*x as usize].to_string()));
                    row
                })
                .collect()
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]
    #[test]
    fn row_and_column_order_do_not_change_the_result(
        left in (0usize..25).prop_flat_map(side),
        right in (0usize..25).prop_flat_map(side),
        lrows in Just(()).prop_perturb(|_, mut rng| rng.random::<u64>()),
        lcols in proptest::sample::select(vec![[0usize, 1, 2, 3], [3, 2, 1, 0], [1, 0, 3, 2], [2, 3, 0, 1]]),
        rcols in proptest::sample::select(vec![[0usize, 1, 2, 3], [2, 0, 3, 1], [3, 1, 0, 2]]),
    ) {
        let names: Vec<String> = ["id", "x", "y", "z"].iter().map(|s| s.to_string()).collect();
        let ident = |n: usize| (0..n).collect::<Vec<_>>();
        // A shuffled row order from the seed (Fisher–Yates with a small LCG).
        let shuffled = |n: usize, mut seed: u64| {
            let mut o = ident(n);
            for i in (1..n).rev() {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                o.swap(i, (seed >> 33) as usize % (i + 1));
            }
            o
        };
        let base = compare(&csv(&names, &left, &[0, 1, 2, 3], &ident(left.len())), &csv(&names, &right, &[0, 1, 2, 3], &ident(right.len())));
        let moved = compare(
            &csv(&names, &left, &lcols, &shuffled(left.len(), lrows)),
            &csv(&names, &right, &rcols, &shuffled(right.len(), lrows ^ 0x9e37_79b9_7f4a_7c15)),
        );
        prop_assert_eq!(outcome(&base), outcome(&moved));
        prop_assert_eq!(base.counts.changed, moved.counts.changed);
        prop_assert_eq!(base.counts.added, moved.counts.added);
        prop_assert_eq!(base.counts.deleted, moved.counts.deleted);
        prop_assert_eq!(base.counts.same, moved.counts.same);
    }
}
