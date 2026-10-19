//! Engine-side latency of interactive operations, measured in-process.
//! IPC and rendering are not included here; the harness measures those.

use std::time::Instant;

use cdr_engine::pattern::Patterns;
use cdr_engine::view::{detail, slots, Filter, Order, RangeRequest, View};
use cdr_engine::{CancelToken, Comparison, Kind};
use serde_json::{json, Value};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

fn stats(mut v: Vec<f64>) -> Value {
    if v.is_empty() {
        return Value::Null;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| v[((v.len() as f64 - 1.0) * p).round() as usize];
    let r = |x: f64| (x * 1000.0).round() / 1000.0;
    json!({
        "n": v.len(),
        "p50_ms": r(q(0.5)),
        "p95_ms": r(q(0.95)),
        "p99_ms": r(q(0.99)),
        "max_ms": r(*v.last().unwrap()),
    })
}

fn elapsed_ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

const ROWS: usize = 40;
const COLS: usize = 20;

pub fn run(c: &Comparison, patterns: Option<&Patterns>, n: usize) -> Value {
    let cancel = CancelToken::new();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out = json!({ "rows_per_range": ROWS, "cols_per_range": COLS });

    // View construction (filters). Each is built 5 times.
    let first_changed = c.units.iter().position(|u| u.kind == Kind::Changed);
    let changed_pair = first_changed.and_then(|u| c.changed_pairs(u).next());
    let sample_text = (!c.left.is_empty()).then(|| {
        let r = c.left.len() / 2;
        let vals = c.left.values(r);
        vals[vals.len() / 2].clone()
    });
    let mut filters: Vec<(&str, Order, Filter)> = vec![
        ("aligned_all", Order::Aligned, Filter::default()),
        (
            "aligned_diff_only",
            Order::Aligned,
            Filter { kinds: vec![Kind::Changed, Kind::Added, Kind::Deleted], ..Default::default() },
        ),
        ("right_source_all", Order::RightSource, Filter::default()),
        (
            "right_source_diff_only",
            Order::RightSource,
            Filter { kinds: vec![Kind::Changed, Kind::Added], ..Default::default() },
        ),
    ];
    if let Some(k) = changed_pair {
        filters.push((
            "aligned_changed_column",
            Order::Aligned,
            Filter { changed_pair: Some(k), ..Default::default() },
        ));
    }
    if patterns.is_some_and(|p| !p.list.is_empty()) {
        filters.push(("aligned_largest_pattern", Order::Aligned, Filter { pattern: Some(0), ..Default::default() }));
    }
    if let Some(t) = &sample_text {
        filters.push(("aligned_text_search", Order::Aligned, Filter { text: Some(t.clone()), ..Default::default() }));
    }
    let mut builds = serde_json::Map::new();
    for (name, order, f) in &filters {
        let mut times = Vec::new();
        let mut len = 0;
        for _ in 0..5 {
            let t = Instant::now();
            let v = View::build(c, patterns, *order, f.clone(), &cancel).expect("view");
            times.push(elapsed_ms(t));
            len = v.len();
        }
        let mut s = stats(times);
        s["rows"] = json!(len);
        builds.insert(name.to_string(), s);
    }
    out["view_build"] = Value::Object(builds);

    let all = View::build(c, patterns, Order::Aligned, Filter::default(), &cancel).expect("view");
    let diff = View::build(
        c,
        patterns,
        Order::Aligned,
        Filter { kinds: vec![Kind::Changed, Kind::Added, Kind::Deleted], ..Default::default() },
        &cancel,
    )
    .expect("view");
    let right = View::build(c, patterns, Order::RightSource, Filter::default(), &cancel).expect("view");
    let s_al = slots(c, Order::Aligned);
    let s_r = slots(c, Order::RightSource);

    let range_probe = |view: &View, sl: &[cdr_engine::view::Slot], cols: usize, n: usize, rng: &mut Rng| {
        let (mut eng, mut ser, mut bytes) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..n {
            let start = rng.below(view.len().saturating_sub(ROWS) + 1);
            let col_start = rng.below(sl.len().saturating_sub(cols) + 1);
            let req = RangeRequest { start, count: ROWS, col_start, col_count: cols, max_chars: 200, pinned: vec![] };
            let t = Instant::now();
            let resp = view.range(c, patterns, sl, &req);
            eng.push(elapsed_ms(t));
            let t = Instant::now();
            let json = serde_json::to_vec(&resp).expect("json");
            ser.push(elapsed_ms(t));
            bytes.push(json.len() as f64);
        }
        bytes.sort_by(|a, b| a.partial_cmp(b).unwrap());
        json!({
            "engine": stats(eng),
            "serialize_json": stats(ser),
            "json_bytes_median": bytes.get(bytes.len() / 2).copied().unwrap_or(0.0),
        })
    };
    out["range_aligned"] = range_probe(&all, &s_al, COLS, n, &mut rng);
    out["range_aligned_all_columns"] = range_probe(&all, &s_al, s_al.len(), n.div_ceil(4), &mut rng);
    out["range_diff_only"] = range_probe(&diff, &s_al, COLS, n, &mut rng);
    out["range_right_source"] = range_probe(&right, &s_r, COLS, n, &mut rng);

    // Last rows (deep position; no OFFSET scan involved).
    let mut last = Vec::new();
    for _ in 0..n.min(200) {
        let t = Instant::now();
        let r = all.range(
            c,
            patterns,
            &s_al,
            &RangeRequest {
                start: all.len().saturating_sub(ROWS),
                count: ROWS,
                col_start: 0,
                col_count: COLS,
                max_chars: 200,
                pinned: vec![],
            },
        );
        last.push(elapsed_ms(t));
        assert_eq!(r.rows.last().map(|x| x.row + 1), Some(all.len()).filter(|&l| l > 0));
    }
    out["range_last_rows"] = stats(last);

    // Next difference + fetching the rows around it (what the UI does).
    let (mut nd, mut move_to) = (Vec::new(), Vec::new());
    for _ in 0..n {
        let from = rng.below(all.len().max(1));
        let t = Instant::now();
        let target = all.next_diff(Some(from), true);
        nd.push(elapsed_ms(t));
        if let Some(row) = target {
            let start = row.saturating_sub(ROWS / 2);
            let r = all.range(
                c,
                patterns,
                &s_al,
                &RangeRequest { start, count: ROWS, col_start: 0, col_count: COLS, max_chars: 200, pinned: vec![] },
            );
            let _ = serde_json::to_vec(&r).expect("json");
            move_to.push(elapsed_ms(t));
        }
    }
    out["next_diff"] = stats(nd);
    out["next_diff_with_range"] = stats(move_to);

    let (mut det, mut det_bytes) = (Vec::new(), Vec::new());
    for _ in 0..n {
        let u = rng.below(c.units.len().max(1)) as u32;
        let t = Instant::now();
        if let Some(d) = detail(c, patterns, u) {
            let json = serde_json::to_vec(&d).expect("json");
            det.push(elapsed_ms(t));
            det_bytes.push(json.len() as f64);
        }
    }
    det_bytes.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out["detail"] = stats(det);
    out["detail_json_bytes_median"] = json!(det_bytes.get(det_bytes.len() / 2).copied().unwrap_or(0.0));

    let mut fk = Vec::new();
    for _ in 0..n {
        if c.units.is_empty() {
            break;
        }
        let u = rng.below(c.units.len());
        let key = c.key_values(u);
        let t = Instant::now();
        let found = c.find_key(&key);
        let row = found.and_then(|f| diff.row_of_unit(c, f).or_else(|| all.row_of_unit(c, f)));
        fk.push(elapsed_ms(t));
        assert_eq!(found, Some(u as u32));
        assert!(row.is_some());
    }
    out["find_key_and_locate"] = stats(fk);
    out
}
