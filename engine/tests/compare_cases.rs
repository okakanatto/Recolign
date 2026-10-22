//! Comparison semantics. Includes the 24 cases of the prototype engine test
//! (tests/test_engine.js), re-expressed against this engine, plus the sample
//! data expectations (samples/expected.json) and cases the prototype did not
//! cover (column add/delete, comparison rules, encodings, views).

use std::path::PathBuf;

use cdr_engine::compare::IGNORED_BIT;
use cdr_engine::error::{CompareError, LoadError, Side};
use cdr_engine::export::{export_csv, export_xlsx, ExportEncoding, ExportError, ExportFormat, ExportRequest};
use cdr_engine::pattern::aggregate;
use cdr_engine::pipeline::{compare_tables, run, CompareRequest, SideInput};
use cdr_engine::view::{detail, slots, Filter, Order, RangeRequest, View};
use cdr_engine::{CancelToken, Comparison, Kind, LoadOptions, Progress, Table, TextEncoding, TrimRule, NONE};

fn table(src: &str) -> Table {
    Table::from_bytes("t.csv", src.as_bytes().to_vec(), &LoadOptions::default()).unwrap()
}

fn cmp_with(a: &str, b: &str, keys: &[&str], excluded: &[&str], trim: TrimRule) -> Result<Comparison, CompareError> {
    let keys: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
    let excluded: Vec<String> = excluded.iter().map(|s| s.to_string()).collect();
    compare_tables(table(a), table(b), &keys, &excluded, trim, &CancelToken::new(), &Progress::default())
}

fn cmp(a: &str, b: &str) -> Comparison {
    cmp_with(a, b, &["id"], &[], TrimRule::None).unwrap()
}

fn counts(c: &Comparison) -> (u64, u64, u64, u64, u64) {
    let k = c.counts;
    (k.units, k.changed, k.added, k.deleted, k.same)
}

fn assert_formula(c: &Comparison) {
    let k = c.counts;
    assert_eq!(k.left_records, k.same + k.changed + k.deleted);
    assert_eq!(k.right_records, k.same + k.changed + k.added);
    assert_eq!(k.units, k.same + k.changed + k.deleted + k.added);
}

// --- Prototype engine cases -------------------------------------------------

#[test]
fn p01_row_and_column_order_do_not_matter() {
    let c = cmp("id,v\n001,a\n002,b\n", "v,id\nb,002\na,001\n");
    assert_eq!(c.counts.same, 2);
    assert_formula(&c);
}

#[test]
fn p02_kinds_are_exclusive() {
    let c = cmp("id,v\na,x\nb,y\nc,z\n", "id,v\nc,Z\na,x\nd,q\n");
    assert_eq!(counts(&c), (4, 1, 1, 1, 1));
    assert_formula(&c);
}

#[test]
fn p03_leading_zeros_are_kept() {
    let c = cmp("id,v\na,001\n", "id,v\na,1\n");
    assert_eq!(c.counts.changed, 1);
    assert_eq!(c.left.values(0)[1], "001");
}

#[test]
fn p04_key_leading_zeros_are_not_matched_implicitly() {
    let c = cmp("id,v\n001,a\n", "id,v\n1,a\n");
    assert_eq!((c.counts.added, c.counts.deleted), (1, 1));
}

#[test]
fn p05_whitespace_difference_is_a_change() {
    assert_eq!(cmp("id,v\na,x\n", "id,v\na,x \n").counts.changed, 1);
}

#[test]
fn p06_quoting_alone_is_not_a_value_difference() {
    assert_eq!(cmp("id,v\na,\"x\"\n", "id,v\na,x\n").counts.same, 1);
}

#[test]
fn p07_raw_text_keeps_quotes() {
    assert_eq!(table("id,v\na,\"x\"\n").raw_text(0), "a,\"x\"");
}

#[test]
fn p08_quoted_newline_comma_and_quote() {
    let t = table("id,v\r\na,\"x,\r\n\"\"y\"\"\"\r\nb,z\r\n");
    assert_eq!(t.values(0)[1], "x,\r\n\"y\"");
    assert_eq!((t.records[0].line_start, t.records[0].line_end), (2, 3));
    assert_eq!(t.records[1].line_start, 4);
}

#[test]
fn p09_bom_is_removed_from_header() {
    assert_eq!(table("\u{FEFF}id,v\na,x").headers[0], "id");
}

#[test]
fn p10_empty_string_and_null_text_differ() {
    assert_eq!(cmp("id,v\na,\n", "id,v\na,NULL\n").counts.changed, 1);
    assert_eq!(table("id,v\na,\n").values(0)[1], "");
}

#[test]
fn p11_duplicate_key_on_one_side_stops() {
    let e = cmp_with("id,v\na,x\na,y\n", "id,v\na,z\n", &["id"], &[], TrimRule::None).err().unwrap();
    match &e {
        CompareError::KeyValidation(r) => {
            assert_eq!(r.left.duplicate_groups_total, 1);
            assert_eq!(r.left.duplicate_records_total, 2);
            assert_eq!(r.left.duplicate_groups[0].records.iter().map(|x| x.line_start).collect::<Vec<_>>(), vec![2, 3]);
            assert!(r.right.is_empty());
        }
        other => panic!("{other}"),
    }
    assert!(e.to_string().contains("重複キー"));
}

#[test]
fn a_long_duplicate_key_is_cut_in_the_report() {
    let long = "鍵".repeat(5_000);
    let e =
        cmp_with(&format!("id,v\n{long},x\n{long},y\n"), "id,v\na,z\n", &["id"], &[], TrimRule::None).err().unwrap();
    match &e {
        CompareError::KeyValidation(r) => {
            let key = &r.left.duplicate_groups[0].key[0];
            assert_eq!(key.chars().count(), cdr_engine::view::DETAIL_KEY_CHARS + 1);
            assert!(key.ends_with("鍵…"));
        }
        other => panic!("{other}"),
    }
    assert!(e.to_string().len() < 10_000);
}

#[test]
fn p12_empty_key_stops() {
    let e = cmp_with("id,v\n,x\n", "id,v\na,z\n", &["id"], &[], TrimRule::None).err().unwrap();
    assert!(matches!(&e, CompareError::KeyValidation(r) if r.left.empty_total == 1));
}

#[test]
fn p13_composite_key_with_separator_characters_does_not_collide() {
    let a = "id,sub,v\na|b,c,1\na,b|c,2\n";
    let b = "id,sub,v\na,b|c,2\na|b,c,1\n";
    let c = cmp_with(a, b, &["id", "sub"], &[], TrimRule::None).unwrap();
    assert_eq!(c.counts.same, 2);
}

#[test]
fn p14_full_pattern_separates_extra_changes() {
    let c = cmp("id,v,w\na,x,1\nb,x,1\nc,x,1\n", "id,v,w\na,y,1\nb,y,1\nc,y,2\n");
    let p = aggregate(&c, &CancelToken::new()).unwrap();
    assert_eq!(p.list.len(), 2);
    assert_eq!(p.list[0].units, vec![0, 1]);
    assert_eq!(p.list[1].units, vec![2]);
}

#[test]
fn p15_missing_fields_stop_instead_of_padding() {
    let e = Table::from_bytes("t.csv", b"id,v\na\n".to_vec(), &LoadOptions::default()).unwrap_err();
    assert!(matches!(e, LoadError::FieldCount { found: 1, expected: 2, .. }));
}

#[test]
fn p16_duplicate_header_needs_explicit_mapping() {
    let e = cmp_with("id,id\na,b\n", "id,id\na,b\n", &["id"], &[], TrimRule::None).err().unwrap();
    assert!(matches!(e, CompareError::ColumnMapping { .. }));
}

#[test]
fn p17_blank_line_in_the_middle_is_reported() {
    let e = Table::from_bytes("t.csv", b"id,v\na,x\n\nb,z\n".to_vec(), &LoadOptions::default()).unwrap_err();
    assert!(matches!(e, LoadError::FieldCount { blank: true, line: 3, .. }));
}

#[test]
fn p18_invalid_quotes_stop() {
    for src in ["id,v\na,\"x", "id,v\na,x\"q\n", "id,v\na,\"x\" y\n"] {
        let e = Table::from_bytes("t.csv", src.as_bytes().to_vec(), &LoadOptions::default()).unwrap_err();
        assert!(matches!(e, LoadError::Csv { .. }), "{src}");
    }
}

#[test]
fn p19_crlf_and_lf_inside_values_differ() {
    assert_eq!(cmp("id,v\na,\"x\r\ny\"\n", "id,v\na,\"x\ny\"\n").counts.changed, 1);
}

#[test]
fn p20_header_only_files_compare_as_empty() {
    assert_eq!(cmp("id,v\n", "id,v\n").counts.units, 0);
}

#[test]
fn p21_repeated_key_column_is_rejected() {
    let e = cmp_with("id,v\na,x\n", "id,v\na,x\n", &["id", "id"], &[], TrimRule::None).err().unwrap();
    assert!(matches!(e, CompareError::KeyColumn { .. }));
}

// p22 (formula-like export values) belongs to the export stage (G3).

#[test]
fn p23_tab_separated() {
    let t = Table::from_bytes(
        "t.tsv",
        b"id\tv\r\na\t\"x\ty\"\r\n".to_vec(),
        &LoadOptions { delimiter: b'\t', ..Default::default() },
    )
    .unwrap();
    assert_eq!(t.values(0), vec!["a", "x\ty"]);
}

#[test]
fn p24_markup_is_plain_text() {
    let v = "<img src=x onerror=alert(1)>";
    let c = cmp(&format!("id,v\na,{v}\n"), &format!("id,v\na,{v}\n"));
    assert_eq!(c.counts.same, 1);
}

// --- Cases beyond the prototype -----------------------------------------------

#[test]
fn columns_added_and_removed_are_structure_not_changes() {
    let c = cmp("id,v,old\na,1,x\nb,2,y\n", "id,new,v\na,q,1\nb,r,9\n");
    assert_eq!((c.counts.same, c.counts.changed), (1, 1));
    assert_eq!(c.mapping.left_only, vec![2]);
    assert_eq!(c.mapping.right_only, vec![1]);
    let s = slots(&c, Order::Aligned);
    let names: Vec<_> = s.iter().map(|x| (x.name_left.as_deref(), x.name_right.as_deref())).collect();
    assert_eq!(names, vec![(Some("id"), Some("id")), (Some("v"), Some("v")), (Some("old"), None), (None, Some("new"))]);
}

#[test]
fn excluded_column_is_tracked_as_ignored_difference() {
    let c = cmp_with("id,v,t\na,1,x\nb,2,y\n", "id,v,t\na,1,X\nb,3,Y\n", &["id"], &["t"], TrimRule::None).unwrap();
    assert_eq!((c.counts.same, c.counts.same_with_ignored, c.counts.changed), (1, 1, 1));
    assert_eq!(c.ignored_pairs(0).collect::<Vec<_>>(), vec![2]);
    assert_eq!(c.changed_pairs(1).collect::<Vec<_>>(), vec![1]);
    assert_eq!(c.ignored_pairs(1).collect::<Vec<_>>(), vec![2]);
    assert!(c.diff_entries(1).contains(&(2 | IGNORED_BIT)));
    // Per column: v changed once; t differed in an ignored way twice.
    assert_eq!(c.pair_change_counts(), (vec![0, 1, 0], vec![0, 0, 2]));
}

#[test]
fn excluded_key_is_rejected() {
    assert!(cmp_with("id,v\na,1\n", "id,v\na,1\n", &["id"], &["id"], TrimRule::None).is_err());
}

#[test]
fn trim_rule_is_explicit_and_recorded() {
    let a = "id,v\na, x\t\n";
    let b = "id,v\na,x\n";
    assert_eq!(cmp(a, b).counts.changed, 1);
    let c = cmp_with(a, b, &["id"], &[], TrimRule::AsciiSpaceTab).unwrap();
    assert_eq!((c.counts.same, c.counts.same_with_ignored), (1, 1));
    // Full-width space is not ASCII whitespace.
    let c = cmp_with("id,v\na,\u{3000}x\n", b, &["id"], &[], TrimRule::AsciiSpaceTab).unwrap();
    assert_eq!(c.counts.changed, 1);
}

#[test]
fn trim_does_not_apply_to_keys() {
    let c = cmp_with("id,v\na ,1\n", "id,v\na,1\n", &["id"], &[], TrimRule::AsciiSpaceTab).unwrap();
    assert_eq!((c.counts.added, c.counts.deleted), (1, 1));
}

#[test]
fn unicode_forms_are_not_unified() {
    // Full-width vs half-width, case, NFC vs NFD.
    assert_eq!(cmp("id,v\na,ＡＢＣ\n", "id,v\na,ABC\n").counts.changed, 1);
    assert_eq!(cmp("id,v\na,abc\n", "id,v\na,ABC\n").counts.changed, 1);
    assert_eq!(cmp("id,v\na,\u{30AC}\n", "id,v\na,\u{30AB}\u{3099}\n").counts.changed, 1);
}

#[test]
fn key_of_whitespace_only_is_a_value() {
    let c = cmp("id,v\n ,1\n", "id,v\n ,1\n");
    assert_eq!(c.counts.same, 1);
}

#[test]
fn duplicate_and_empty_keys_are_reported_for_both_sides() {
    let e = cmp_with("id,v\na,1\na,2\n,3\n", "id,v\nb,1\nb,2\nb,3\n", &["id"], &[], TrimRule::None).err().unwrap();
    match e {
        CompareError::KeyValidation(r) => {
            assert_eq!((r.left.duplicate_groups_total, r.left.empty_total), (1, 1));
            assert_eq!((r.right.duplicate_groups_total, r.right.duplicate_records_total), (1, 3));
        }
        other => panic!("{other}"),
    }
}

fn sjis_table(bytes: &[u8]) -> Table {
    Table::from_bytes(
        "s.csv",
        bytes.to_vec(),
        &LoadOptions { encoding: TextEncoding::Windows31j, ..Default::default() },
    )
    .unwrap()
}

#[test]
fn utf8_and_windows31j_files_compare_by_decoded_text() {
    // id,v / 001,髙﨑①〜  — the same text in two encodings.
    let utf8 = table("id,v\n001,髙﨑①～\n");
    let sjis = sjis_table(b"id,v\n001,\xfb\xfc\xfa\xb1\x87\x40\x81\x60\n");
    let c = compare_tables(utf8, sjis, &["id".into()], &[], TrimRule::None, &CancelToken::new(), &Progress::default())
        .unwrap();
    assert_eq!(c.counts.same, 1);
}

#[test]
fn windows31j_nec_and_ibm_duplicates_are_the_same_character() {
    // U+2235 (∵) is 0x81E6, 0x879A and 0xFA5B in Windows-31J.
    let a = sjis_table(b"id,v\n1,\x81\xe6\n2,\x87\x9a\n");
    let b = sjis_table(b"id,v\n1,\xfa\x5b\n2,\x81\xe6\n");
    assert_eq!(a.values(0)[1], "\u{2235}");
    let c =
        compare_tables(a, b, &["id".into()], &[], TrimRule::None, &CancelToken::new(), &Progress::default()).unwrap();
    assert_eq!(c.counts.same, 2);
    // The original bytes differ even though the values are equal.
    assert_ne!(c.left.record_bytes(0), c.right.record_bytes(0));
}

#[test]
fn escaped_quotes_compare_by_value() {
    let c = cmp("id,v\na,\"x\"\"y\"\n", "id,v\na,\"x\"\"y\"\n");
    assert_eq!(c.counts.same, 1);
    let c = cmp("id,v\na,\"x\"\"y\"\n", "id,v\na,\"x\"\"z\"\n");
    assert_eq!(c.counts.changed, 1);
}

#[test]
fn views_ranges_and_navigation() {
    let c = cmp("id,v,w\na,1,x\nb,2,y\nc,3,z\n", "w,id,v\nz,c,3\nY,b,2\nq,d,4\n");
    // Aligned order: left order, then right-only in right order.
    assert_eq!(
        c.units.iter().map(|u| u.kind).collect::<Vec<_>>(),
        vec![Kind::Deleted, Kind::Changed, Kind::Same, Kind::Added]
    );
    let cancel = CancelToken::new();
    let all = View::build(&c, None, Order::Aligned, Filter::default(), &cancel).unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(all.next_diff(None, true), Some(0));
    assert_eq!(all.next_diff(Some(1), true), Some(3));
    assert_eq!(all.next_diff(Some(3), true), None, "no silent wrap-around");
    assert_eq!(all.next_diff(Some(3), false), Some(1));
    assert_eq!(all.next_diff(Some(0), false), None);

    let s = slots(&c, Order::Aligned);
    let r = all.range(
        &c,
        None,
        &s,
        &RangeRequest { start: 0, count: 10, col_start: 0, col_count: 10, max_chars: 200, pinned: vec![] },
    );
    assert_eq!(r.total, 4);
    assert!(r.rows[0].right.is_none(), "deleted unit has no right record");
    assert!(r.rows[3].left.is_none(), "added unit has no left record");
    let changed = &r.rows[1];
    assert_eq!(changed.right.as_ref().unwrap().cells[2].v.as_deref(), Some("Y"));
    assert_eq!(changed.right.as_ref().unwrap().cells[2].d, 1);
    assert_eq!(changed.right.as_ref().unwrap().cells[1].d, 0);

    // Right source order keeps the file order and colours by key partner.
    let right = View::build(&c, None, Order::RightSource, Filter::default(), &cancel).unwrap();
    let rs = slots(&c, Order::RightSource);
    let rr = right.range(
        &c,
        None,
        &rs,
        &RangeRequest { start: 0, count: 10, col_start: 0, col_count: 10, max_chars: 200, pinned: vec![] },
    );
    assert_eq!(rr.rows.iter().map(|x| x.kind).collect::<Vec<_>>(), vec![Kind::Same, Kind::Changed, Kind::Added]);
    assert!(rr.rows.iter().all(|x| x.left.is_none()));
    assert_eq!(rr.rows[1].right.as_ref().unwrap().cells[0].d, 1, "column w changed");
    assert_eq!(right.row_of_unit(&c, 1), Some(1));
    assert_eq!(right.row_of_unit(&c, 0), None, "deleted unit has no right row");

    let diff = View::build(
        &c,
        None,
        Order::Aligned,
        Filter { kinds: vec![Kind::Changed, Kind::Added, Kind::Deleted], ..Default::default() },
        &cancel,
    )
    .unwrap();
    assert_eq!(diff.len(), 3);
    assert_eq!(diff.row_of_unit(&c, 3), Some(2));
    assert_eq!(diff.row_of_unit(&c, 2), None);

    let by_col =
        View::build(&c, None, Order::Aligned, Filter { changed_pair: Some(2), ..Default::default() }, &cancel).unwrap();
    assert_eq!(by_col.len(), 1);
}

#[test]
fn range_distinguishes_absent_column_absent_record_and_empty_value() {
    let c = cmp("id,v,old\na,,x\nb,1,y\n", "id,v\na,\nc,2\n");
    let all = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    let r = all.range(
        &c,
        None,
        &s,
        &RangeRequest { start: 0, count: 3, col_start: 0, col_count: 3, max_chars: 200, pinned: vec![] },
    );
    let a = &r.rows[0];
    assert_eq!(a.left.as_ref().unwrap().cells[1].v.as_deref(), Some(""), "empty string");
    assert_eq!(a.right.as_ref().unwrap().cells[2].v, None, "column not present on the right");
    assert!(r.rows[1].right.is_none(), "record not present on the right");
}

#[test]
fn preview_is_cut_but_detail_is_full() {
    let long: String = "あ".repeat(500);
    let c = cmp(&format!("id,v\na,{long}\n"), &format!("id,v\na,{long}い\n"));
    let all = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    let r = all.range(
        &c,
        None,
        &s,
        &RangeRequest { start: 0, count: 1, col_start: 0, col_count: 2, max_chars: 100, pinned: vec![] },
    );
    let cell = &r.rows[0].left.as_ref().unwrap().cells[1];
    assert!(cell.cut);
    assert_eq!(cell.v.as_ref().unwrap().chars().count(), 100);
    assert_eq!(cell.d, 1, "change is detected on the full value");
    let d = detail(&c, None, 0).unwrap();
    assert_eq!(d.cells[1].left.as_ref().unwrap().chars().count(), 500);
    assert!(!d.cells[1].left_cut);
    assert_eq!(d.left.as_ref().unwrap().raw, format!("a,{long}"));
}

#[test]
fn the_detail_of_a_record_cuts_huge_values_and_says_so() {
    use cdr_engine::view::{DETAIL_KEY_CHARS, DETAIL_RAW_CHARS, DETAIL_VALUE_CHARS};
    let huge = "あ".repeat(DETAIL_RAW_CHARS + 10);
    let c = cmp(&format!("id,v\n{huge},{huge}\n"), &format!("id,v\n{huge},x\n"));
    let d = detail(&c, None, 0).unwrap();
    assert_eq!(d.key[0].chars().count(), DETAIL_KEY_CHARS + 1);
    assert!(d.key[0].ends_with('…'));
    let v = &d.cells[1];
    assert_eq!((v.left.as_ref().unwrap().chars().count(), v.left_cut), (DETAIL_VALUE_CHARS, true));
    assert_eq!((v.right.as_deref(), v.right_cut), (Some("x"), false));
    assert_eq!(v.d, 1);
    let raw = d.left.as_ref().unwrap();
    assert_eq!((raw.raw.chars().count(), raw.raw_cut), (DETAIL_RAW_CHARS, true));
}

#[test]
fn files_with_more_columns_than_a_sheet_are_refused() {
    let names = |n: usize| (1..=n).map(|i| format!("c{i}")).collect::<Vec<_>>().join(",");
    let ok = format!("{}\n", names(cdr_engine::table::MAX_COLUMNS));
    assert!(Table::from_bytes("t.csv", ok.into_bytes(), &LoadOptions::default()).is_ok());
    let wide = format!("{}\n", names(cdr_engine::table::MAX_COLUMNS + 1));
    let e = Table::from_bytes("t.csv", wide.into_bytes(), &LoadOptions::default()).err().unwrap();
    assert_eq!(e.code(), "too_many_columns");
    assert!(e.to_string().contains("16384列まで"), "{e}");
}

#[test]
fn text_filter_matches_single_cells_only() {
    let c = cmp("id,v,w\na,x,y\nb,\"p\"\"q\",z\n", "id,v,w\na,x,y\nb,\"p\"\"q\",z\n");
    let cancel = CancelToken::new();
    let f = |t: &str| {
        View::build(&c, None, Order::Aligned, Filter { text: Some(t.into()), ..Default::default() }, &cancel)
            .unwrap()
            .len()
    };
    assert_eq!(f("x"), 1);
    assert_eq!(f("x,y"), 0, "a match must not span two cells");
    assert_eq!(f("p\"q"), 1, "escaped quotes are searched as values");
    assert_eq!(f("p\"\"q"), 0);
}

#[test]
fn find_key_and_line() {
    let c = cmp("id,v\na,\"1\n2\"\nb,3\n", "id,v\nc,1\nb,3\n");
    assert_eq!(c.find_key(&["b".into()]), Some(1));
    assert_eq!(c.find_key(&["c".into()]), Some(2));
    assert_eq!(c.find_key(&["z".into()]), None);
    assert_eq!(c.left.record_at_line(3), Some(0));
    assert_eq!(c.left.record_at_line(4), Some(1));
}

#[test]
fn cancelled_comparison_returns_no_result() {
    let cancel = CancelToken::new();
    cancel.cancel();
    let r = compare_tables(
        table("id,v\na,1\n"),
        table("id,v\na,1\n"),
        &["id".into()],
        &[],
        TrimRule::None,
        &cancel,
        &Progress::default(),
    );
    assert!(matches!(r, Err(CompareError::Cancelled)));
}

// --- Sample data (samples/expected.json) ------------------------------------

fn samples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../samples")
}

#[test]
fn sample_data_matches_expected_json() {
    let req = CompareRequest {
        left: SideInput { path: samples().join("before.csv"), options: LoadOptions::default() },
        right: SideInput { path: samples().join("after.csv"), options: LoadOptions::default() },
        keys: vec!["社員番号".into()],
        excluded: vec![],
        trim: TrimRule::None,
    };
    let out = run(&req, &CancelToken::new(), &Progress::default()).unwrap();
    let c = &out.comparison;
    assert_eq!((c.counts.left_records, c.counts.right_records), (64, 64));
    assert_eq!(counts(c), (67, 12, 3, 3, 49));
    assert_formula(c);
    assert!(c.left.info.bom);
    assert_eq!(
        c.left.wait_sha256().unwrap().0,
        "8cc87c421f50eaad87fa839d7916f762a42074e38bca0ad08963a8fc78c2799b",
        "matches CONTENTS_SHA256.txt"
    );
    assert_eq!(cdr_engine::pipeline::comparison_id(c).unwrap().len(), 64);

    let p = aggregate(c, &CancelToken::new()).unwrap();
    let top = &p.list[0];
    assert_eq!(top.units.len(), 5);
    let s = cdr_engine::view::pattern_summaries(c, &p, 0, 1, 100);
    assert_eq!(s[0].changes.len(), 1);
    assert_eq!(
        (s[0].changes[0].name.as_str(), s[0].changes[0].before.as_str(), s[0].changes[0].after.as_str()),
        ("部署", "営業一課", "営業企画課")
    );

    // 001021 changes 部署 the same way and also 役職: a separate pattern.
    let u = c.find_key(&["001021".into()]).unwrap();
    let pid = p.unit_pattern[u as usize];
    assert_ne!(pid, top.id);
    assert_eq!(p.list[pid as usize].units.len(), 1);
    assert_eq!(p.list[pid as usize].pairs.len(), 2);

    // Key-aligned order follows the left file; right-only keys come last.
    assert_eq!(c.key_values(0), vec!["001001"]);
    assert_eq!(c.key_values(64), vec!["001065"]);
    assert!(c.units[64..].iter().all(|u| u.kind == Kind::Added && u.left == NONE));

    // 001032 has a quoted line break: it spans two physical lines.
    let u = c.find_key(&["001032".into()]).unwrap() as usize;
    let rec = c.left.records[c.units[u].left as usize];
    assert_eq!((rec.line_start, rec.line_end), (33, 34));
    let d = detail(c, Some(&p), u as u32).unwrap();
    assert!(d.left.unwrap().raw.contains("初回面談\n本人確認済"));

    // 001013: trailing space only; raw text keeps the quotes.
    let u = c.find_key(&["001013".into()]).unwrap();
    let d = detail(c, Some(&p), u).unwrap();
    assert_eq!(d.kind, Kind::Changed);
    assert!(d.left.as_ref().unwrap().raw.starts_with("\"001013\""));
    let changed: Vec<_> = d.cells.iter().filter(|x| x.d == 1).collect();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].right.as_deref(), Some("確認済 "));
}

// --- G2: sorting, copying, preview -----------------------------------------

use cdr_engine::view::{CopyRequest, CopySide, Sort, SortBy};

fn sorted(c: &Comparison, sort: Sort) -> View {
    View::build_sorted(c, None, Order::Aligned, Filter::default(), sort, &CancelToken::new()).unwrap()
}

fn keys_in_view(c: &Comparison, v: &View) -> Vec<String> {
    (0..v.len()).map(|r| c.key_values(v.unit_at(c, r)).join("|")).collect()
}

#[test]
fn sort_by_key_kind_count_and_column() {
    let c = cmp("id,v,w\nb,1,x\na,2,y\nc,3,z\n", "id,v,w\nc,9,Z\na,2,y\nd,4,q\n");
    // Natural order: b(del) a(same) c(changed 2 cols) d(added)
    let by_key = sorted(&c, Sort { by: SortBy::Key, ..Default::default() });
    assert_eq!(keys_in_view(&c, &by_key), vec!["a", "b", "c", "d"]);
    let desc = sorted(&c, Sort { by: SortBy::Key, desc: true, ..Default::default() });
    assert_eq!(keys_in_view(&c, &desc), vec!["d", "c", "b", "a"]);
    let by_kind = sorted(&c, Sort { by: SortBy::Kind, ..Default::default() });
    assert_eq!(keys_in_view(&c, &by_kind), vec!["c", "b", "d", "a"]);
    let by_count = sorted(&c, Sort { by: SortBy::ChangedCount, desc: true, ..Default::default() });
    assert_eq!(keys_in_view(&c, &by_count)[0], "c");
    // Column v on the right side: d=4, a=2, c=9; b has no right record → last.
    let by_col =
        sorted(&c, Sort { by: SortBy::Column, slot: Some(1), side: Some(cdr_engine::Side::Right), desc: false });
    assert_eq!(keys_in_view(&c, &by_col), vec!["a", "d", "c", "b"]);
    // Locating and navigating still work in a sorted view.
    let u_c = c.find_key(&["c".into()]).unwrap();
    assert_eq!(by_key.row_of_unit(&c, u_c), Some(2));
    assert_eq!(by_key.next_diff(None, true), Some(1), "b (deleted) is the first difference in key order");
}

#[test]
fn key_sort_is_element_wise_for_composite_keys() {
    let c = cmp_with(
        "k1,k2,v\na,b,1\nab,,2\na,a,3\n",
        "k1,k2,v\na,b,1\nab,x,2\na,a,3\n",
        &["k1", "k2"],
        &[],
        TrimRule::None,
    );
    // Empty key values are rejected, so use a non-empty variant.
    assert!(c.is_err());
    let c = cmp_with(
        "k1,k2,v\na,b,1\nab,c,2\na,a,3\n",
        "k1,k2,v\na,b,1\nab,c,2\na,a,3\n",
        &["k1", "k2"],
        &[],
        TrimRule::None,
    )
    .unwrap();
    let v = sorted(&c, Sort { by: SortBy::Key, ..Default::default() });
    assert_eq!(keys_in_view(&c, &v), vec!["a|a", "a|b", "ab|c"]);
}

#[test]
fn copy_tsv_quotes_values_and_leaves_absent_cells_empty() {
    let c = cmp("id,v\na,\"x\ty\"\nb,\"he said \"\"hi\"\"\"\n", "id,v\na,plain\nc,new\n");
    let v = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    let req = CopyRequest { start: 0, end: usize::MAX, slots: vec![0, 1], side: CopySide::Both, header: true };
    let (text, rows, cells) = v.copy_tsv(&c, &s, &req).unwrap();
    assert_eq!((rows, cells), (3, 12));
    let lines: Vec<&str> = text.split("\r\n").collect();
    assert_eq!(lines[0], "左:id\t左:v\t右:id\t右:v");
    assert_eq!(lines[1], "a\t\"x\ty\"\ta\tplain");
    assert_eq!(lines[2], "b\t\"he said \"\"hi\"\"\"\t\t");
    assert_eq!(lines[3], "\t\tc\tnew");
    let left_only = CopyRequest { start: 1, end: 2, slots: vec![1], side: CopySide::Left, header: false };
    assert_eq!(v.copy_tsv(&c, &s, &left_only).unwrap().0, "\"he said \"\"hi\"\"\"\r\n");
}

fn export_req(side: CopySide, encoding: ExportEncoding) -> ExportRequest {
    ExportRequest {
        start: 0,
        end: None,
        slots: vec![0, 1],
        side,
        kind_column: true,
        line_numbers: true,
        changed_columns: true,
        format: ExportFormat::Csv,
        encoding,
        neutralize: false,
    }
}

#[test]
fn export_csv_can_neutralize_formula_like_values() {
    let c = cmp("id,v\n1,=SUM(A1)\n2,-5\n3,a=b\n", "id,v\n1,=SUM(A1)\n2,-5\n3,a=b\n");
    let v = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    let mut req = export_req(CopySide::Left, ExportEncoding::Utf8);
    (req.kind_column, req.line_numbers, req.changed_columns) = (false, false, false);
    let mut out = Vec::new();
    export_csv(&v, &c, &s, &req, None, &mut out, &CancelToken::new()).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "id,v\r\n1,=SUM(A1)\r\n2,-5\r\n3,a=b\r\n",
        "values unchanged by default"
    );
    req.neutralize = true;
    let mut out = Vec::new();
    export_csv(&v, &c, &s, &req, None, &mut out, &CancelToken::new()).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "id,v\r\n1,'=SUM(A1)\r\n2,-5\r\n3,a=b\r\n");
}

#[test]
fn formula_like_values_include_full_width_forms_and_leave_numbers() {
    use cdr_engine::export::formula_like;
    for v in [
        "=1+1",
        "@SUM(A1)",
        "+81-3-1234-5678",
        "-1+cmd|' /C calc'!A0",
        "-A1",
        "\tx",
        "\rx",
        " =1+1",
        "\u{3000}=1+1",
        "\u{FF1D}1+1",
        "\u{FF20}x",
        "\u{FF0B}1+1",
        "\u{FF0D}x",
        "=",
    ] {
        assert!(formula_like(v), "{v:?}");
    }
    for v in ["", "a=b", "-5", "+5", "-1,000.5", "\u{FF0D}\u{FF11}\u{FF10}", "-", "+", "\u{FF0D}", "x@y"] {
        assert!(!formula_like(v), "{v:?}");
    }
}

#[test]
fn export_csv_writes_kinds_lines_changed_columns_and_quoted_values() {
    let c = cmp("id,v\na,\"x,y\"\nb,old\n", "id,v\nb,new\nc,\"q\"\"\"\n");
    let v = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    let mut out = Vec::new();
    let n = export_csv(
        &v,
        &c,
        &s,
        &export_req(CopySide::Both, ExportEncoding::Utf8Bom),
        None,
        &mut out,
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(n, 3);
    assert!(out.starts_with(b"\xEF\xBB\xBF"));
    let text = String::from_utf8(out[3..].to_vec()).unwrap();
    let lines: Vec<&str> = text.split("\r\n").collect();
    assert_eq!(lines[0], "種類,左の行,右の行,変更した列,左:id,左:v,右:id,右:v");
    assert_eq!(lines[1], "左のみ,2,,,a,\"x,y\",,");
    assert_eq!(lines[2], "変更,3,2,v,b,old,b,new");
    assert_eq!(lines[3], "右のみ,,3,,,,c,\"q\"\"\"");
    assert_eq!(lines[4], "");
}

#[test]
fn export_csv_in_windows31j_and_unrepresentable_characters() {
    let c = cmp("id,v\n1,髙橋①\n2,𠮷野家\n", "id,v\n1,髙橋①\n2,𠮷野家\n");
    let v = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    let mut req = export_req(CopySide::Left, ExportEncoding::Windows31j);
    req.end = Some(1);
    let mut out = Vec::new();
    export_csv(&v, &c, &s, &req, None, &mut out, &CancelToken::new()).unwrap();
    let (text, _, bad) = encoding_rs::SHIFT_JIS.decode(&out);
    assert!(!bad);
    assert_eq!(text, "種類,左の行,変更した列,id,v\r\n一致,2,,1,髙橋①\r\n");
    // 𠮷 (U+20BB7) has no Windows-31J code: the export stops and says where.
    req.end = None;
    let err = export_csv(&v, &c, &s, &req, None, &mut Vec::new(), &CancelToken::new()).unwrap_err();
    match &err {
        ExportError::Unrepresentable { line, column, ch } => assert_eq!((*line, column.as_str(), *ch), (3, "v", '𠮷')),
        e => panic!("{e}"),
    }
    assert!(err.to_string().contains("UTF-8で書き出すか"));
}

#[test]
fn export_csv_in_windows31j_stops_at_characters_that_would_read_back_as_others() {
    // The Shift_JIS encoder writes these as the bytes of \, ~ and －: the
    // file would read back with other characters, so the export stops.
    for (value, ch) in [("¥100", '¥'), ("a‾b", '‾'), ("−HYPERLINK", '−')] {
        let c = cmp(&format!("id,v\n1,{value}\n"), &format!("id,v\n1,{value}\n"));
        let v = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
        let s = slots(&c, Order::Aligned);
        let req = export_req(CopySide::Left, ExportEncoding::Windows31j);
        match export_csv(&v, &c, &s, &req, None, &mut Vec::new(), &CancelToken::new()).unwrap_err() {
            ExportError::Unrepresentable { line, column, ch: found } => {
                assert_eq!((line, column.as_str(), found), (2, "v", ch))
            }
            e => panic!("{e}"),
        }
    }
}

#[test]
fn export_xlsx_writes_a_workbook_and_respects_limits() {
    let c = cmp("id,v\na,=1+1\nb,old\n", "id,v\nb,new\nc,007\n");
    let v = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    let dir = std::env::temp_dir().join(format!("cdr-xlsx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("out.xlsx");
    let mut req = export_req(CopySide::Both, ExportEncoding::Utf8Bom);
    req.format = ExportFormat::Xlsx;
    let info = vec![("キー".to_string(), "id".to_string())];
    let out = std::fs::File::create(&path).unwrap();
    let n = export_xlsx(&v, &c, &s, &req, None, &info, out, &CancelToken::new()).unwrap();
    assert_eq!(n, 3);
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.starts_with(b"PK"), "an xlsx file is a zip archive");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn find_text_moves_between_matching_rows_and_wraps() {
    let c = cmp("id,v,w\n1,apple,x\n2,pear,apple pie\n3,plum,y\n", "id,v,w\n1,apple,x\n3,plum,apple\n");
    let v = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    assert_eq!(v.count_text(&c, "apple"), 3);
    let h = v.find_text(&c, &s, "apple", None, true).unwrap();
    assert_eq!((h.row, h.wrapped, h.left.clone(), h.right.clone()), (0, false, vec![1], vec![1]));
    let h = v.find_text(&c, &s, "apple", Some(0), true).unwrap();
    assert_eq!((h.row, h.left.clone(), h.right.clone()), (1, vec![2], vec![]));
    let h = v.find_text(&c, &s, "apple", Some(1), true).unwrap();
    assert_eq!((h.row, h.left.clone(), h.right.clone()), (2, vec![], vec![2]));
    let h = v.find_text(&c, &s, "apple", Some(2), true).unwrap();
    assert_eq!((h.row, h.wrapped), (0, true));
    let h = v.find_text(&c, &s, "apple", Some(0), false).unwrap();
    assert_eq!((h.row, h.wrapped), (2, true));
    assert!(v.find_text(&c, &s, "banana", None, true).is_none());
    // A file order searches only that file's records.
    let rv = View::build(&c, None, Order::RightSource, Filter::default(), &CancelToken::new()).unwrap();
    let rs = slots(&c, Order::RightSource);
    assert_eq!(rv.count_text(&c, "pie"), 0);
    assert_eq!(rv.find_text(&c, &rs, "apple", Some(0), true).unwrap().row, 1);
}

#[test]
fn preview_suggests_encoding_and_delimiter() {
    let dir = std::env::temp_dir().join(format!("cdr-preview-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sjis.tsv");
    // 社員番号<TAB>氏名 / 001<TAB>髙田
    std::fs::write(&path, b"\x8e\xd0\x88\xf5\x94\xd4\x8d\x86\t\x8e\x81\x96\xbc\r\n001\t\xfb\xfc\x93\x63\r\n").unwrap();
    let p = cdr_engine::preview::preview(&path, None, 20).unwrap();
    assert_eq!(p.suggested.encoding, Some(TextEncoding::Windows31j));
    assert_eq!(p.suggested.delimiter, "\t");
    assert_eq!(p.headers, vec!["社員番号", "氏名"]);
    assert_eq!(p.rows, vec![vec!["001".to_string(), "髙田".to_string()]]);
    assert!(p.error.is_none() && !p.partial);
    // Reading with the wrong encoding is reported, not replaced.
    let wrong = cdr_engine::preview::preview(
        &path,
        Some(LoadOptions { encoding: TextEncoding::Utf8, delimiter: b'\t', ..Default::default() }),
        20,
    )
    .unwrap();
    assert!(wrong.error.unwrap().contains("UTF-8として読めません"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn group_rules_are_exact_and_count_as_documented() {
    use cdr_engine::groups::{group, GroupRule};
    let c = cmp(
        "id,dept,role\n1,A,x\n2,A,x\n3,A,x\n4,A,x\n5,B,y\n6,C,z\n",
        "id,dept,role\n1,A2,x\n2,A2,x\n3,A2,x\n4,A2,X\n5,B,Y\n6,C,z\n",
    );
    let p = aggregate(&c, &CancelToken::new()).unwrap();
    let t = CancelToken::new();
    // Changed-column sets: {dept} ×3, {dept, role} ×1, {role} ×1.
    let g = group(&c, Some(&p), GroupRule::Columns, &[], &t).unwrap();
    let sizes: Vec<(Vec<u32>, usize)> = g.list.iter().map(|x| (g.pairs(&c, x), x.len())).collect();
    assert_eq!(sizes, vec![(vec![1], 3), (vec![1, 2], 1), (vec![2], 1)]);
    assert_eq!(g.changed_units, 5);
    // Per column: dept A→A2 ×4 (one of them also changed role), role x→X ×1, role y→Y ×1.
    let g = group(&c, Some(&p), GroupRule::PerColumn, &[], &t).unwrap();
    let first = &g.list[0];
    assert_eq!((g.pairs(&c, first), first.len(), first.with_other), (vec![1], 4, 1));
    assert_eq!(g.values(&c, first), Some(("A".to_string(), "A2".to_string())));
    assert_eq!(g.units(first), &[0, 1, 2, 3]);
    assert_eq!(g.list.len(), 3);
    // Full patterns keep the record with the extra change apart (3 + 1 + 1).
    let g = group(&c, Some(&p), GroupRule::Full, &[], &t).unwrap();
    assert_eq!(g.list.iter().map(|x| x.len()).collect::<Vec<_>>(), vec![3, 1, 1]);
}

/// Leaving a column out of the grouping: a time stamp that differs in every
/// record no longer keeps equal changes apart, and records that changed
/// only in that column are counted, not grouped.
#[test]
fn groups_can_leave_columns_out() {
    use cdr_engine::groups::{group, GroupRule};
    let c = cmp(
        "id,dept,stamp\n1,A,t0\n2,A,t0\n3,A,t0\n4,B,t0\n5,C,t0\n",
        "id,dept,stamp\n1,A2,t1\n2,A2,t2\n3,A2,t3\n4,B,t4\n5,C2,t5\n",
    );
    let p = aggregate(&c, &CancelToken::new()).unwrap();
    let t = CancelToken::new();
    let stamp = 2u32;
    let full = group(&c, Some(&p), GroupRule::Full, &[], &t).unwrap();
    assert_eq!(full.singles, 5, "every record differs in its time stamp");
    let g = group(&c, Some(&p), GroupRule::Full, &[stamp], &t).unwrap();
    assert_eq!(g.list.iter().map(|x| x.len()).collect::<Vec<_>>(), vec![3, 1]);
    assert_eq!(g.units(&g.list[0]), &[0, 1, 2]);
    assert_eq!(g.pairs(&c, &g.list[0]), vec![1], "the skipped column is not part of the pattern");
    assert_eq!((g.only_skipped, g.changed_units), (1, 5));
    let g = group(&c, Some(&p), GroupRule::Columns, &[stamp], &t).unwrap();
    assert_eq!(g.list.iter().map(|x| x.len()).collect::<Vec<_>>(), vec![4]);
    let g = group(&c, Some(&p), GroupRule::PerColumn, &[stamp], &t).unwrap();
    assert_eq!(
        g.list.iter().map(|x| (g.values(&c, x).unwrap(), x.len())).collect::<Vec<_>>(),
        vec![(("A".to_string(), "A2".to_string()), 3), (("C".to_string(), "C2".to_string()), 1),]
    );
    assert_eq!(g.list[0].with_other, 0, "a change in a skipped column is not 'another change'");
    let g = group(&c, Some(&p), GroupRule::PerColumn, &[], &t).unwrap();
    let dept = g.list.iter().find(|x| g.values(&c, x) == Some(("A".to_string(), "A2".to_string()))).unwrap();
    assert_eq!(dept.with_other, 3, "without skipping, the time stamp is another change");
}

/// Per-column groups match a plain map from (column, before, after) to
/// records, on data with few distinct values (many members per group).
#[test]
fn per_column_groups_match_a_plain_map() {
    use cdr_engine::groups::{group, GroupRule};
    use std::collections::BTreeMap;
    let vals = ["a", "b", "", "a ", "ａ"];
    let mut seed = 7u64;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let (mut l, mut r) = (String::from("id,x,y,z\n"), String::from("id,x,y,z\n"));
    for i in 0..3000 {
        let row: Vec<&str> = (0..6).map(|_| vals[next() % vals.len()]).collect();
        l.push_str(&format!("{i},{},{},{}\n", row[0], row[1], row[2]));
        r.push_str(&format!("{i},{},{},{}\n", row[3], row[4], row[5]));
    }
    let c = cmp(&l, &r);
    let p = aggregate(&c, &CancelToken::new()).unwrap();
    let g = group(&c, Some(&p), GroupRule::PerColumn, &[], &CancelToken::new()).unwrap();
    let mut want: BTreeMap<(u32, String, String), Vec<u32>> = BTreeMap::new();
    for &u in c.diff_units.iter() {
        if c.units[u as usize].kind != Kind::Changed {
            continue;
        }
        let (lv, rv) =
            (c.left.values(c.units[u as usize].left as usize), c.right.values(c.units[u as usize].right as usize));
        for k in c.changed_pairs(u as usize) {
            let pr = c.mapping.pairs[k as usize];
            want.entry((k, lv[pr.left as usize].clone(), rv[pr.right as usize].clone())).or_default().push(u);
        }
    }
    let mut got: BTreeMap<(u32, String, String), Vec<u32>> = BTreeMap::new();
    for x in &g.list {
        let (b, a) = g.values(&c, x).unwrap();
        let k = g.pairs(&c, x)[0];
        assert!(got.insert((k, b, a), g.units(x).to_vec()).is_none());
    }
    assert_eq!(got, want);
    assert!(g.list.windows(2).all(|w| w[0].len() >= w[1].len()));
    assert_eq!(g.singles, g.list.iter().filter(|x| x.len() == 1).count());
}

/// The same data as CSV in UTF-8 on the left and as fixed-width
/// Windows-31J text without quotes on the right.
#[test]
fn files_of_different_layouts_compare_by_value() {
    use cdr_engine::{FixedWidth, WidthUnit};
    let left = "id,name,city\n01,山田,東京\n02,佐藤,大阪\n03,\"a\"\"b\",x\n";
    let mut right = Vec::new();
    right.extend_from_slice(b"id  name    city  \r\n");
    right.extend_from_slice(b"01  \x8eR\x93c    \x93\x8c\x8b\x9e  \r\n"); // 山田 東京
    right.extend_from_slice(b"02  \x8d\xb2\x93\xa1    \x95\x9f\x89\xaa  \r\n"); // 佐藤 福岡
    right.extend_from_slice(b"03  a\"b     x     \r\n");
    let l = Table::from_bytes("l.csv", left.as_bytes().to_vec(), &LoadOptions::default()).unwrap();
    let fixed = FixedWidth { widths: vec![4, 8, 6], unit: WidthUnit::Bytes, trim: true };
    let opts = LoadOptions { encoding: TextEncoding::Windows31j, fixed: Some(fixed), ..Default::default() };
    let r = Table::from_bytes("r.txt", right, &opts).unwrap();
    let c = cdr_engine::pipeline::compare_tables(
        l,
        r,
        &["id".to_string()],
        &[],
        TrimRule::None,
        &CancelToken::new(),
        &Progress::default(),
    )
    .unwrap();
    assert_eq!((c.counts.same, c.counts.changed, c.counts.added, c.counts.deleted), (2, 1, 0, 0));
    let changed = (0..c.units.len()).find(|&u| c.units[u].kind == Kind::Changed).unwrap();
    assert_eq!(c.key_values(changed), ["02"]);
}

#[test]
fn ctrl_arrows_stop_where_values_start_and_end_as_in_a_spreadsheet() {
    use cdr_engine::view::Direction::{Down, Left, Right, Up};
    let c = cmp("id,v\na,1\nb,2\nc,\nd,\ne,5\nf,6\ng,7\n", "id,v\na,1\nb,2\nc,\nd,\ne,5\nf,6\ng,7\n");
    let v = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    let cols = [0, 1];
    let go = |row, col, dir| v.edge(&c, &s, &cols, Side::Left, (row, col), dir);
    // Down column v: to the end of a run, over the empty cells, to the last row.
    assert_eq!(go(0, 1, Down), (1, 1));
    assert_eq!(go(1, 1, Down), (4, 1));
    assert_eq!(go(2, 1, Down), (4, 1));
    assert_eq!(go(4, 1, Down), (6, 1));
    assert_eq!(go(6, 1, Down), (6, 1));
    assert_eq!(go(6, 1, Up), (4, 1));
    assert_eq!(go(4, 1, Up), (1, 1));
    assert_eq!(go(1, 1, Up), (0, 1));
    assert_eq!(go(0, 0, Down), (6, 0));
    // Across a row.
    assert_eq!(go(0, 0, Right), (0, 1));
    assert_eq!(go(2, 0, Right), (2, 1), "nothing filled to the right: the last column");
    assert_eq!(go(2, 1, Left), (2, 0));

    let c = cmp("id,a,b,c,d\n1,x,,y,z\n", "id,a,b,c,d\n2,x,,y,z\n");
    let v = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    let cols = [0, 1, 2, 3, 4];
    let go = |row, col, side, dir| v.edge(&c, &s, &cols, side, (row, col), dir);
    assert_eq!(go(0, 0, Side::Left, Right), (0, 1));
    assert_eq!(go(0, 1, Side::Left, Right), (0, 3));
    assert_eq!(go(0, 3, Side::Left, Right), (0, 4));
    assert_eq!(go(0, 4, Side::Left, Left), (0, 3));
    // The left-only record has no cells on the right.
    assert_eq!(go(0, 0, Side::Right, Down), (1, 0));
    assert_eq!(go(1, 0, Side::Right, Up), (0, 0));
}

#[test]
fn ctrl_arrows_reach_far_rows() {
    use cdr_engine::view::Direction::{Down, Up};
    let mut a = String::from("id,v,w\n");
    for i in 0..10_000 {
        let v = if i == 9_000 { "" } else { "x" };
        let w = if i == 9_500 { "y" } else { "" };
        a.push_str(&format!("{i},{v},{w}\n"));
    }
    let c = cmp(&a, &a);
    let v = View::build(&c, None, Order::Aligned, Filter::default(), &CancelToken::new()).unwrap();
    let s = slots(&c, Order::Aligned);
    let cols = [0, 1, 2];
    let go = |row, col, dir| v.edge(&c, &s, &cols, Side::Left, (row, col), dir);
    assert_eq!(go(0, 1, Down), (8_999, 1));
    assert_eq!(go(8_999, 1, Down), (9_001, 1));
    assert_eq!(go(9_999, 1, Up), (9_001, 1));
    assert_eq!(go(0, 2, Down), (9_500, 2));
    assert_eq!(go(9_500, 2, Down), (9_999, 2));
    assert_eq!(go(9_999, 2, Up), (9_500, 2));
    assert_eq!(go(9_500, 2, Up), (0, 2));
}
