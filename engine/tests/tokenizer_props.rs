//! Property tests: serialize random tables, parse them back, and compare with
//! the generated values and with the `csv` crate (an independent parser).

use cdr_engine::tokenizer::{index, split, unescape};
use proptest::prelude::*;

fn field() -> impl Strategy<Value = String> {
    proptest::collection::vec(prop::sample::select(vec!['a', 'b', ',', '"', '\r', '\n', ' ', 'あ', '\t', '0']), 0..6)
        .prop_map(|cs| cs.into_iter().collect())
}

fn table() -> impl Strategy<Value = (Vec<Vec<String>>, Vec<bool>, usize, bool)> {
    (2usize..5, 1usize..20).prop_flat_map(|(cols, rows)| {
        (
            proptest::collection::vec(proptest::collection::vec(field(), cols), rows),
            proptest::collection::vec(any::<bool>(), cols * rows),
            0usize..3,
            any::<bool>(),
        )
    })
}

fn serialize(rows: &[Vec<String>], force_quote: &[bool], terminator: &str, trailing: bool) -> String {
    let mut out = String::new();
    let mut k = 0;
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            if c > 0 {
                out.push(',');
            }
            let needs = v.contains([',', '"', '\r', '\n']);
            if needs || force_quote[k] {
                out.push('"');
                out.push_str(&v.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(v);
            }
            k += 1;
        }
        if r + 1 < rows.len() || trailing {
            out.push_str(terminator);
        }
    }
    out
}

fn parse_ours(src: &str) -> Vec<Vec<String>> {
    let b = src.as_bytes();
    let idx = index(b, 0, b',', true, |_| false).expect("valid input");
    let mut spans = Vec::new();
    idx.records
        .iter()
        .map(|p| {
            let rec = &b[p.start as usize..p.end as usize];
            let breaks = rec
                .iter()
                .enumerate()
                .filter(|&(i, &c)| c == b'\n' || (c == b'\r' && rec.get(i + 1) != Some(&b'\n')))
                .count();
            assert_eq!(p.line_end - p.line_start, breaks as u64);
            split(rec, b',', true, usize::MAX, &mut spans);
            spans
                .iter()
                .map(|s| {
                    let raw = &rec[s.start as usize..s.end as usize];
                    String::from_utf8(if s.escaped { unescape(raw) } else { raw.to_vec() }).unwrap()
                })
                .collect()
        })
        .collect()
}

fn parse_csv_crate(src: &str) -> Vec<Vec<String>> {
    csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(src.as_bytes())
        .records()
        .map(|r| r.unwrap().iter().map(str::to_string).collect())
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn roundtrip_matches_source_and_csv_crate((rows, quote, term, trailing) in table()) {
        let terminator = ["\r\n", "\n", "\r"][term];
        let src = serialize(&rows, &quote, terminator, trailing);
        let ours = parse_ours(&src);
        prop_assert_eq!(&ours, &rows);
        // The csv crate skips records that consist of a single empty field
        // only; rows here always have at least two fields.
        prop_assert_eq!(parse_csv_crate(&src), rows);
    }

    #[test]
    fn records_are_contiguous((rows, quote, term, trailing) in table()) {
        let terminator = ["\r\n", "\n", "\r"][term];
        let src = serialize(&rows, &quote, terminator, trailing);
        let idx = index(src.as_bytes(), 0, b',', true, |_| false).unwrap();
        let mut pos = 0u64;
        let mut line = 1u64;
        for p in &idx.records {
            prop_assert_eq!(p.start, pos);
            prop_assert_eq!(p.line_start, line);
            pos = p.end + terminator.len() as u64;
            line = p.line_end + 1;
        }
        prop_assert_eq!(pos.min(src.len() as u64), src.len() as u64);
    }
}

/// Values with no delimiter and no line break, but anything else (quotes too).
fn plain_field() -> impl Strategy<Value = String> {
    proptest::collection::vec(prop::sample::select(vec!['a', '"', ',', ' ', 'あ', '\t', '0']), 0..6)
        .prop_map(|cs| cs.into_iter().collect())
}

/// Values for fixed-width columns: no line break, no space at either end.
fn padded_field() -> impl Strategy<Value = String> {
    proptest::collection::vec(prop::sample::select(vec!['a', '"', '|', 'あ', '髙', 'ｱ', '0']), 0..4)
        .prop_map(|cs| cs.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Without quotes, `"` is an ordinary character and every record is one line.
    #[test]
    fn unquoted_records_split_back_into_their_values(
        rows in (2usize..5, 1usize..20).prop_flat_map(|(c, r)| proptest::collection::vec(proptest::collection::vec(plain_field(), c), r)),
        term in 0usize..3,
    ) {
        let terminator = ["\r\n", "\n", "\r"][term];
        let src = rows.iter().map(|r| r.join("|")).collect::<Vec<_>>().join(terminator);
        let b = src.as_bytes();
        let idx = index(b, 0, b'|', false, |_| false).expect("valid input");
        let mut spans = Vec::new();
        let back: Vec<Vec<String>> = idx.records.iter().map(|p| {
            let rec = &b[p.start as usize..p.end as usize];
            split(rec, b'|', false, usize::MAX, &mut spans);
            spans.iter().map(|s| String::from_utf8(rec[s.start as usize..s.end as usize].to_vec()).unwrap()).collect()
        }).collect();
        prop_assert_eq!(back, rows);
    }

    /// Fixed-width Windows-31J: values padded to their column's width in
    /// bytes come back as they were.
    #[test]
    fn fixed_width_values_come_back(
        rows in (1usize..5, 1usize..20).prop_flat_map(|(c, r)| proptest::collection::vec(proptest::collection::vec(padded_field(), c), r)),
        extra in 0u32..3,
    ) {
        use cdr_engine::fixed::{index as fixed_index, split as fixed_split, FixedWidth, WidthUnit};
        use cdr_engine::TextEncoding;
        let sjis = |v: &str| encoding_rs::SHIFT_JIS.encode(v).0.into_owned();
        let ncols = rows[0].len();
        let widths: Vec<u32> = (0..ncols)
            .map(|c| rows.iter().map(|r| sjis(&r[c]).len() as u32).max().unwrap() + extra)
            .map(|w| w.max(1))
            .collect();
        let mut src = Vec::new();
        for r in &rows {
            for (c, v) in r.iter().enumerate() {
                let bytes = sjis(v);
                src.extend_from_slice(&bytes);
                src.extend(std::iter::repeat_n(b' ', widths[c] as usize - bytes.len()));
            }
            src.extend_from_slice(b"\r\n");
        }
        let layout = FixedWidth { widths, unit: WidthUnit::Bytes, trim: true };
        let idx = fixed_index(&src, 0, &layout, TextEncoding::Windows31j, |_| false).expect("valid input");
        prop_assert_eq!(idx.records.len(), rows.len());
        let mut spans = Vec::new();
        for (p, row) in idx.records.iter().zip(&rows) {
            let rec = &src[p.start as usize..p.end as usize];
            fixed_split(rec, &layout, TextEncoding::Windows31j, usize::MAX, &mut spans);
            let back: Vec<String> = spans
                .iter()
                .map(|s| encoding_rs::SHIFT_JIS.decode(&rec[s.start as usize..s.end as usize]).0.into_owned())
                .collect();
            prop_assert_eq!(&back, row);
        }
    }
}
