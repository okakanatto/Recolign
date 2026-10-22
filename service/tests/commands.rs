//! The JSON commands the desktop app and the harness share, run end to end
//! on the sample files.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cdr_service::Service;
use serde_json::{json, Value};

fn sample(name: &str) -> String {
    format!("{}/../samples/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn ok(s: &Arc<Service>, cmd: &str, args: Value) -> Value {
    s.handle(cmd, &args).unwrap_or_else(|e| panic!("{cmd}: {}", e.body))
}

fn wait_ready(s: &Arc<Service>) -> Value {
    let t = Instant::now();
    loop {
        let st = s.status();
        match st["phase"]["state"].as_str().unwrap() {
            "ready" => return st["phase"]["summary"].clone(),
            "failed" => panic!("failed: {}", st["phase"]["error"]),
            _ => {}
        }
        assert!(t.elapsed() < Duration::from_secs(20), "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn request(keys: &[&str]) -> Value {
    let side =
        |f: &str| json!({ "path": sample(f), "options": { "encoding": "utf8", "delimiter": 44, "has_header": true } });
    json!({ "left": side("before.csv"), "right": side("after.csv"), "keys": keys })
}

#[test]
fn preview_compare_view_and_copy() {
    let s = Arc::new(Service::new());
    let p = ok(&s, "preview", json!({ "path": sample("before.csv") }));
    assert_eq!(p["suggested"]["encoding"], "utf8");
    assert_eq!(p["options"]["delimiter"], 44);
    assert_eq!(p["headers"][0], "社員番号");
    assert!(p["bom"].as_bool().unwrap());

    ok(&s, "compare", request(&["社員番号"]));
    let sum = wait_ready(&s);
    let c = &sum["counts"];
    assert_eq!((c["changed"].as_u64(), c["added"].as_u64(), c["deleted"].as_u64()), (Some(12), Some(3), Some(3)));
    assert_eq!(sum["left"]["has_header"], true);
    let changed: u64 = sum["pair_changed"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).sum();
    assert!(changed >= 12);

    let v = ok(&s, "view", json!({ "order": "aligned", "filter": { "kinds": ["changed", "added", "deleted"] } }));
    assert_eq!(v["total"], 18);
    let id = v["view"].as_u64().unwrap();
    let rows = ok(&s, "rows", json!({ "view": id, "start": 0, "count": 5, "col_start": 0, "col_count": 3 }));
    assert_eq!(rows["rows"].as_array().unwrap().len(), 5);
    let first = &rows["rows"][0];
    let unit = first["unit"].as_u64().unwrap();
    assert_eq!(ok(&s, "locate", json!({ "view": id, "unit": unit }))["row"], 0);
    let d = ok(&s, "detail", json!({ "unit": unit }));
    assert_eq!(d["unit"], unit);

    let key = d["key"].clone();
    assert_eq!(ok(&s, "find", json!({ "key": key }))["unit"], unit);

    // Copy two rows of the key column, with the column name.
    let cp = ok(&s, "copy", json!({ "view": id, "start": 0, "end": 2, "slots": [0], "side": "left", "header": true }));
    let text = cp["text"].as_str().unwrap();
    assert!(text.starts_with("社員番号\r\n"), "{text:?}");
    assert_eq!(cp["rows"], 2);

    // Export the differences to a CSV file; the inputs are never overwritten.
    let dir = std::env::temp_dir().join(format!("cdr-export-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("diff.csv");
    let ex = ok(&s, "export", json!({ "view": id, "path": out, "slots": [0, 2], "side": "both", "kind_column": true }));
    assert_eq!(ex["rows"], 18);
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("\u{feff}種類,左:社員番号,左:部署,右:社員番号,右:部署\r\n"), "{text:?}");
    assert_eq!(text.lines().count(), 19);
    assert!(!dir.join("diff.csv.part").exists());
    // Windows-31J, and Excel with a conditions sheet.
    let sj = dir.join("diff_sjis.csv");
    ok(&s, "export", json!({ "view": id, "path": sj, "slots": [0, 2], "side": "left", "encoding": "windows31j" }));
    let bytes = std::fs::read(&sj).unwrap();
    let (t, _, bad) = encoding_rs::SHIFT_JIS.decode(&bytes);
    assert!(!bad && t.starts_with("社員番号,部署\r\n"), "{t:?}");
    let xl = dir.join("diff.xlsx");
    let ex = ok(
        &s,
        "export",
        json!({ "view": id, "path": xl, "slots": [0, 2], "side": "both", "format": "xlsx",
        "kind_column": true, "changed_columns": true, "info": [["表示", "差分のみ"]] }),
    );
    assert_eq!(ex["rows"], 18);
    assert!(std::fs::read(&xl).unwrap().starts_with(b"PK"));
    let e = s
        .handle("export", &json!({ "view": id, "path": sample("before.csv"), "slots": [0], "side": "left" }))
        .unwrap_err();
    assert!(e.body["error"].as_str().unwrap().contains("書き出せません"));
    std::fs::remove_dir_all(&dir).ok();

    // Text search: the next matching row and the matching cells.
    let all = ok(&s, "view", json!({ "order": "aligned" }));
    let aid = all["view"].as_u64().unwrap();
    let f = ok(&s, "find_text", json!({ "view": aid, "text": "営業企画課", "count": true }));
    assert_eq!(f["count"], 5 + 1);
    let row = f["hit"]["row"].as_u64().unwrap();
    assert!(!f["hit"]["right"].as_array().unwrap().is_empty());
    let g = ok(&s, "find_text", json!({ "view": aid, "text": "営業企画課", "from": row }));
    assert!(g["hit"]["row"].as_u64().unwrap() > row);
    assert!(ok(&s, "find_text", json!({ "view": aid, "text": "存在しない値" }))["hit"].is_null());

    // Sorted by the number of changed columns, most first.
    let sv = ok(&s, "view", json!({ "order": "aligned", "sort": { "by": "changed_count", "desc": true } }));
    assert_eq!(sv["total"], sum["counts"]["units"]);

    // Unknown view ids and commands are refused with a message.
    let e =
        s.handle("rows", &json!({ "view": 9999, "start": 0, "count": 1, "col_start": 0, "col_count": 1 })).unwrap_err();
    assert_eq!(e.status, 409);
    assert!(s.handle("nope", &json!({})).is_err());
}

#[test]
fn key_problems_are_reported_and_reset_goes_back_to_idle() {
    let s = Arc::new(Service::new());
    ok(&s, "compare", request(&["部署"]));
    let t = Instant::now();
    let err = loop {
        let st = s.status();
        if st["phase"]["state"] == "failed" {
            break st["phase"]["error"].clone();
        }
        assert!(st["phase"]["state"] != "ready", "duplicate keys must stop the comparison");
        assert!(t.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(err["code"], "key_validation");
    assert!(err["detail"]["left"]["duplicate_groups_total"].as_u64().unwrap() > 0);
    ok(&s, "reset", json!({}));
    assert_eq!(s.status()["phase"]["state"], "idle");
}

fn wait_review_ready(s: &Arc<Service>) -> Value {
    let t = Instant::now();
    loop {
        let r = s.handle("review_summary", &json!({})).unwrap();
        if r["ready"] == true {
            return r;
        }
        assert!(t.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn review_states_undo_and_resume_by_comparison() {
    let dir = std::env::temp_dir().join(format!("cdr-review-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let s = Arc::new(Service::with_data_dir(dir.clone()));
    ok(&s, "compare", request(&["社員番号"]));
    wait_ready(&s);
    let sum = wait_review_ready(&s);
    assert_eq!(
        (sum["total"].as_u64(), sum["unchecked"].as_u64(), sum["restored"].as_u64()),
        (Some(18), Some(18), Some(0))
    );

    // A bulk change is previewed first: the records are fixed then.
    let v = ok(&s, "view", json!({ "order": "aligned", "filter": { "kinds": ["changed", "added", "deleted"] } }));
    let id = v["view"].as_u64().unwrap();
    let t = ok(&s, "review_target", json!({ "view": id, "start": 0, "end": 3 }));
    assert_eq!(t["count"], 3);
    let set = ok(&s, "review_set", json!({ "token": t["token"], "status": "checked" }));
    assert_eq!((set["checked"].as_u64(), set["changed"].as_u64()), (Some(3), Some(3)));
    assert!(
        s.handle("review_set", &json!({ "token": t["token"], "status": "checked" })).is_err(),
        "a token is used once"
    );
    // A note on one record, then undo and redo.
    let rows = ok(&s, "rows", json!({ "view": id, "start": 3, "count": 1, "col_start": 0, "col_count": 1 }));
    let u = rows["rows"][0]["unit"].as_u64().unwrap();
    ok(&s, "review_set", json!({ "units": [u], "status": "investigate", "memo": "仕入先に確認" }));
    let d = ok(&s, "detail", json!({ "unit": u }));
    assert_eq!(
        (d["review"]["status"].as_str(), d["review"]["memo"].as_str()),
        (Some("investigate"), Some("仕入先に確認"))
    );
    let rows = ok(&s, "rows", json!({ "view": id, "start": 0, "count": 4, "col_start": 0, "col_count": 1 }));
    assert_eq!(rows["rows"][0]["review"], 1);
    assert_eq!(rows["rows"][3]["review"], 2);
    assert_eq!(rows["rows"][3]["memo"], true);
    let un = ok(&s, "review_undo", json!({}));
    assert_eq!((un["investigate"].as_u64(), un["done"]["count"].as_u64()), (Some(0), Some(1)));
    let re = ok(&s, "review_redo", json!({}));
    assert_eq!(re["investigate"], 1);
    // Next unchecked difference skips the reviewed ones.
    let n = ok(&s, "next_review", json!({ "view": id, "from": null, "forward": true, "status": ["unchecked"] }));
    assert_eq!(n["row"], 4);
    // Views by review state.
    let iv = ok(&s, "view", json!({ "order": "aligned", "review": ["investigate"] }));
    assert_eq!(iv["total"], 1);
    // Same records cannot be marked.
    let all = ok(&s, "view", json!({ "order": "aligned" }));
    let first = ok(&s, "rows", json!({ "view": all["view"], "start": 0, "count": 1, "col_start": 0, "col_count": 1 }));
    assert_eq!(first["rows"][0]["kind"], "same");
    assert!(s.handle("review_set", &json!({ "units": [first["rows"][0]["unit"]], "status": "checked" })).is_err());
    s.flush().unwrap();

    // The same files and conditions in a new session get the states back.
    let s2 = Arc::new(Service::with_data_dir(dir.clone()));
    ok(&s2, "compare", request(&["社員番号"]));
    wait_ready(&s2);
    let sum = wait_review_ready(&s2);
    assert_eq!(
        (sum["restored"].as_u64(), sum["checked"].as_u64(), sum["investigate"].as_u64()),
        (Some(4), Some(3), Some(1))
    );
    assert!(sum["stale_saved_at"].is_null());
    // Different conditions (an excluded column) are a different comparison.
    let mut req = request(&["社員番号"]);
    req["excluded"] = json!(["備考"]);
    ok(&s2, "compare", req);
    wait_ready(&s2);
    let sum = wait_review_ready(&s2);
    assert_eq!(sum["restored"], 0);
    assert!(sum["stale_saved_at"].as_i64().is_some(), "the user is told the earlier states were not used");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn groups_by_each_rule() {
    let s = Arc::new(Service::new());
    ok(&s, "compare", request(&["社員番号"]));
    wait_ready(&s);
    // Full patterns need the background aggregation.
    let t = Instant::now();
    let full = loop {
        if let Ok(v) = s.handle("groups", &json!({ "rule": "full" })) {
            break v;
        }
        assert!(t.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(full["items"][0]["count"], 5);
    assert_eq!(full["items"][0]["changes"][0]["after"], "営業企画課");
    let per = ok(&s, "groups", json!({ "rule": "per_column" }));
    let first = &per["items"][0];
    assert_eq!((first["count"].as_u64(), first["with_other"].as_u64()), (Some(6), Some(1)));
    assert_eq!(first["changes"][0]["name"], "部署");
    let cols = ok(&s, "groups", json!({ "rule": "columns" }));
    assert!(cols["total"].as_u64().unwrap() >= 2);
    // A view of one group.
    let v = ok(&s, "view", json!({ "order": "aligned", "group": { "rule": "full", "id": 0 } }));
    assert_eq!(v["total"], 5);
}

/// A saved file that cannot be read is kept aside and reported, and a
/// write that fails is reported until a later write succeeds.
#[test]
fn damaged_saves_and_failed_writes_are_reported() {
    let dir = std::env::temp_dir().join(format!("cdr-review-fail-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let s = Arc::new(Service::with_data_dir(dir.clone()));
    ok(&s, "compare", request(&["社員番号"]));
    wait_ready(&s);
    wait_review_ready(&s);
    let v = ok(&s, "view", json!({ "order": "aligned", "filter": { "kinds": ["changed"] } }));
    let rows = ok(&s, "rows", json!({ "view": v["view"], "start": 0, "count": 1, "col_start": 0, "col_count": 1 }));
    let u = rows["rows"][0]["unit"].clone();
    ok(&s, "review_set", json!({ "units": [u], "status": "checked" }));
    s.flush().unwrap();
    s.reset(); // closes the comparison, so the next session may save it
    let saved: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json") && !p.ends_with("index.json"))
        .collect();
    assert_eq!(saved.len(), 1);

    // Damage the file: the next session starts empty, says so, and keeps the file.
    std::fs::write(&saved[0], b"{\"version\":1,\"entries\":[").unwrap();
    let s2 = Arc::new(Service::with_data_dir(dir.clone()));
    ok(&s2, "compare", request(&["社員番号"]));
    wait_ready(&s2);
    let sum = wait_review_ready(&s2);
    assert_eq!(sum["checked"], 0);
    assert!(sum["load_problem"].as_str().unwrap().contains("壊れている"), "{sum}");
    let kept: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.contains(".unreadable-"))
        .collect();
    assert_eq!(kept.len(), 1);
    // Clearing the saved states needs the comparison closed, and leaves other files alone.
    assert!(s2.handle("review_store", &json!({ "action": "clear" })).is_err());
    let info = ok(&s2, "review_store", json!({ "action": "info" }));
    assert_eq!(info["comparisons"], 0, "only the set-aside file and the index remain");
    std::fs::write(dir.join("other.txt"), b"x").unwrap();
    s2.reset();
    let cleared = ok(&s2, "review_store", json!({ "action": "clear" }));
    assert_eq!(cleared["deleted"], 2);
    assert!(dir.join("other.txt").exists());
    // Clearing the states of the open comparison is one undoable change.
    ok(&s2, "compare", request(&["社員番号"]));
    wait_ready(&s2);
    wait_review_ready(&s2);
    ok(&s2, "review_set", json!({ "units": [u], "status": "investigate", "memo": "x" }));
    let t = ok(&s2, "review_target", json!({ "reviewed": true }));
    assert_eq!((t["count"].as_u64(), t["already"]["memos"].as_u64()), (Some(1), Some(1)));
    let r = ok(&s2, "review_set", json!({ "token": t["token"], "status": "unchecked", "memo": "" }));
    assert_eq!((r["investigate"].as_u64(), r["memos"].as_u64()), (Some(0), Some(0)));
    s2.flush().unwrap();
    assert_eq!(ok(&s2, "review_store", json!({ "action": "info" }))["comparisons"], 0, "an empty set is not kept");
    s2.reset();

    // A file from a newer version is not read and not overwritten.
    std::fs::write(&saved[0], b"{\"version\":99}").unwrap();
    let s3 = Arc::new(Service::with_data_dir(dir.clone()));
    ok(&s3, "compare", request(&["社員番号"]));
    wait_ready(&s3);
    let sum = wait_review_ready(&s3);
    assert!(sum["load_problem"].as_str().unwrap().contains("新しい版"), "{sum}");

    // The folder cannot be written: the error is reported and kept until a write succeeds.
    let blocked = dir.join("blocked");
    std::fs::write(&blocked, b"not a folder").unwrap();
    let s4 = Arc::new(Service::with_data_dir(blocked.clone()));
    ok(&s4, "compare", request(&["社員番号"]));
    wait_ready(&s4);
    wait_review_ready(&s4);
    let r = ok(&s4, "review_set", json!({ "units": [u], "status": "investigate" }));
    assert!(r["save_error"].is_null());
    assert!(s4.flush().is_err());
    let sum = ok(&s4, "review_summary", json!({}));
    assert!(sum["save_error"].as_str().unwrap().contains("確認状況を保存できません"), "{sum}");
    assert_eq!(sum["unsaved"], true);
    assert_eq!(sum["investigate"], 1, "the change stays in memory");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn memory_check_estimates_from_file_sizes() {
    let s = Arc::new(Service::new());
    let m = ok(&s, "memory_check", json!({ "left": sample("before.csv"), "right": sample("after.csv") }));
    let input =
        std::fs::metadata(sample("before.csv")).unwrap().len() + std::fs::metadata(sample("after.csv")).unwrap().len();
    assert_eq!(m["input_bytes"].as_u64(), Some(input));
    assert!(m["needed_bytes"].as_u64().unwrap() > input);
    // Linux reports the available memory; small files fit.
    assert!(m["available_bytes"].as_u64().is_some());
    assert_eq!(m["enough"], true);

    // Short lines: the records count more than the bytes.
    let dir = std::env::temp_dir().join(format!("cdr-memory-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let short = dir.join("short.csv");
    let text: String = std::iter::once("id\n".to_string()).chain((0..200_000).map(|i| format!("{i}\n"))).collect();
    std::fs::write(&short, &text).unwrap();
    let m = ok(&s, "memory_check", json!({ "left": short, "right": short }));
    let needed = m["needed_bytes"].as_u64().unwrap();
    assert!(needed >= 2 * 200_000 * 90 + (64 << 20), "{m}");
    assert!(needed <= 2 * 200_000 * 100 + 2 * text.len() as u64 * 2 + (64 << 20), "{m}");

    // A long first line, then many empty lines: the lines are counted
    // over the whole file, not only in its first megabytes.
    let late = dir.join("late.csv");
    let mut bytes = vec![b'x'; 5 << 20];
    bytes.extend(std::iter::repeat_n(b'\n', 4 << 20));
    std::fs::write(&late, &bytes).unwrap();
    let m = ok(&s, "memory_check", json!({ "left": late, "right": short }));
    assert!(m["needed_bytes"].as_u64().unwrap() >= (4 << 20) * 96 / 2, "{m}");

    // A folder is not read, and no estimate comes from it.
    let m = ok(&s, "memory_check", json!({ "left": dir, "right": short }));
    assert_eq!(m["input_bytes"].as_u64(), Some(text.len() as u64));
    let e = s.handle("preview", &json!({ "path": dir })).unwrap_err();
    assert!(e.body.to_string().contains("ファイルではないため読めません"), "{}", e.body);
    std::fs::remove_dir_all(&dir).ok();
}

/// When the memory left runs low during a comparison, it is stopped with a
/// reason instead of the process ending, and the next comparison works.
#[test]
fn low_memory_stops_the_comparison_with_a_reason() {
    let s = Arc::new(Service::new());
    s.set_memory_guard(u64::MAX, 0);
    ok(&s, "compare", request(&["社員番号"]));
    let t = Instant::now();
    let err = loop {
        let st = s.status();
        if st["phase"]["state"] == "failed" {
            break st["phase"]["error"].clone();
        }
        assert!(st["phase"]["state"] != "ready", "the comparison should have been stopped");
        assert!(t.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(err["code"], "out_of_memory");
    assert!(err["message"].as_str().unwrap().contains("ほかのアプリを閉じて"), "{err}");
    s.set_memory_guard(256 << 20, 64 << 20);
    ok(&s, "compare", request(&["社員番号"]));
    wait_ready(&s);
}

/// A file with the same name but different content is a different
/// comparison: the saved states are not used, and the user is told (AC-10).
#[test]
fn same_name_different_content_does_not_reuse_states() {
    let dir = std::env::temp_dir().join(format!("cdr-review-content-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let files = dir.join("files");
    std::fs::create_dir_all(&files).unwrap();
    let (l, r) = (files.join("before.csv"), files.join("after.csv"));
    std::fs::copy(sample("before.csv"), &l).unwrap();
    std::fs::copy(sample("after.csv"), &r).unwrap();
    let side = |p: &std::path::Path| json!({ "path": p, "options": { "encoding": "utf8", "delimiter": 44, "has_header": true } });
    let req = json!({ "left": side(&l), "right": side(&r), "keys": ["社員番号"] });
    let s = Arc::new(Service::with_data_dir(dir.join("reviews")));
    ok(&s, "compare", req.clone());
    wait_ready(&s);
    wait_review_ready(&s);
    let v = ok(&s, "view", json!({ "order": "aligned", "filter": { "kinds": ["changed"] } }));
    let rows = ok(&s, "rows", json!({ "view": v["view"], "start": 0, "count": 1, "col_start": 0, "col_count": 1 }));
    ok(&s, "review_set", json!({ "units": [rows["rows"][0]["unit"]], "status": "checked" }));
    s.flush().unwrap();
    // Same path and name, one value changed in the right file.
    let text = std::fs::read_to_string(&r).unwrap().replacen("営業企画課", "営業企画部", 1);
    std::fs::write(&r, text).unwrap();
    let s2 = Arc::new(Service::with_data_dir(dir.join("reviews")));
    ok(&s2, "compare", req);
    wait_ready(&s2);
    let sum = wait_review_ready(&s2);
    assert_eq!((sum["restored"].as_u64(), sum["checked"].as_u64()), (Some(0), Some(0)));
    assert!(sum["stale_saved_at"].as_i64().is_some(), "the user is told the earlier states were not used");
    std::fs::remove_dir_all(&dir).ok();
}

/// A bulk change applies to the records fixed when it was shown, even if
/// the view changed in the meantime; changes sent at the same time from
/// several threads are all kept (AC-09).
#[test]
fn bulk_changes_keep_their_records_and_concurrent_changes_are_kept() {
    let s = Arc::new(Service::new());
    ok(&s, "compare", request(&["社員番号"]));
    wait_ready(&s);
    wait_review_ready(&s);
    let changed = ok(&s, "view", json!({ "order": "aligned", "filter": { "kinds": ["changed"] } }));
    let rows =
        ok(&s, "rows", json!({ "view": changed["view"], "start": 0, "count": 3, "col_start": 0, "col_count": 1 }));
    let fixed: Vec<u64> = rows["rows"].as_array().unwrap().iter().map(|r| r["unit"].as_u64().unwrap()).collect();
    let t = ok(&s, "review_target", json!({ "view": changed["view"], "start": 0, "end": 3 }));
    // Meanwhile the user switches to another view (only deleted records).
    let deleted = ok(&s, "view", json!({ "order": "aligned", "filter": { "kinds": ["deleted"] } }));
    let r = ok(&s, "review_set", json!({ "token": t["token"], "status": "investigate" }));
    assert_eq!(r["investigate"], 3);
    for u in &fixed {
        assert_eq!(ok(&s, "detail", json!({ "unit": u }))["review"]["status"], "investigate");
    }
    let drows =
        ok(&s, "rows", json!({ "view": deleted["view"], "start": 0, "count": 10, "col_start": 0, "col_count": 1 }));
    // (Rows without a state carry no "review" field.)
    assert!(
        drows["rows"].as_array().unwrap().iter().all(|r| r["review"].is_null()),
        "the other view's records are untouched"
    );

    // Eight threads mark different records at the same time.
    let all = ok(&s, "view", json!({ "order": "aligned", "filter": { "kinds": ["changed", "added", "deleted"] } }));
    let rows = ok(&s, "rows", json!({ "view": all["view"], "start": 0, "count": 18, "col_start": 0, "col_count": 1 }));
    let units: Vec<u64> = rows["rows"].as_array().unwrap().iter().map(|r| r["unit"].as_u64().unwrap()).collect();
    let threads: Vec<_> = units
        .chunks(3)
        .map(|chunk| {
            let (s, chunk) = (s.clone(), chunk.to_vec());
            std::thread::spawn(move || {
                for u in chunk {
                    s.handle("review_set", &json!({ "units": [u], "status": "checked" })).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let sum = ok(&s, "review_summary", json!({}));
    assert_eq!((sum["checked"].as_u64(), sum["investigate"].as_u64()), (Some(18), Some(0)));
    // Each was one step: undoing them all returns to the 3 under investigation.
    for _ in 0..18 {
        ok(&s, "review_undo", json!({}));
    }
    assert_eq!(ok(&s, "review_summary", json!({}))["investigate"], 3);
}

/// An export that fails partway (a character Shift_JIS cannot hold, on the
/// last row) leaves no partial file and keeps the file that was already at
/// the destination; a destination folder that does not exist is reported.
#[test]
fn failed_exports_leave_the_destination_as_it_was() {
    let dir = std::env::temp_dir().join(format!("cdr-export-fail-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let (l, r) = (dir.join("l.csv"), dir.join("r.csv"));
    let mut a = String::from("id,v\n");
    let mut b = String::from("id,v\n");
    for i in 0..5000 {
        a.push_str(&format!("{i},a{i}\n"));
        b.push_str(&format!("{i},{}\n", if i == 4999 { "𠮷野家".to_string() } else { format!("b{i}") }));
    }
    std::fs::write(&l, a).unwrap();
    std::fs::write(&r, b).unwrap();
    let side = |p: &std::path::Path| json!({ "path": p, "options": { "encoding": "utf8", "delimiter": 44, "has_header": true } });
    let s = Arc::new(Service::new());
    ok(&s, "compare", json!({ "left": side(&l), "right": side(&r), "keys": ["id"] }));
    wait_ready(&s);
    let v = ok(&s, "view", json!({ "order": "aligned" }));
    let out = dir.join("out.csv");
    std::fs::write(&out, "the earlier export\n").unwrap();
    let err = s
        .handle(
            "export",
            &json!({ "view": v["view"], "path": out, "slots": [0, 1], "side": "right", "encoding": "windows31j" }),
        )
        .unwrap_err();
    let msg = err.body["error"].as_str().unwrap().to_string();
    assert!(msg.contains("𠮷"), "the character is named: {msg}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "the earlier export\n", "the earlier file is kept");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty(), "no partial file is left");
    let err = s
        .handle(
            "export",
            &json!({ "view": v["view"], "path": dir.join("ない").join("out.csv"), "slots": [0, 1], "side": "right" }),
        )
        .unwrap_err();
    assert!(err.body["error"].as_str().unwrap().contains("書き出せませんでした"), "{}", err.body);
    std::fs::remove_dir_all(&dir).ok();
}

/// The temporary file of an export is always a new file: a compared file
/// (or any other file) with the name `<destination>.part` is not written.
#[test]
fn an_export_never_writes_a_file_that_has_its_temporary_name() {
    let dir = std::env::temp_dir().join(format!("cdr-export-part-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let (l, r) = (dir.join("out.csv.part"), dir.join("r.csv"));
    std::fs::write(&l, "id,v\n1,a\n2,b\n").unwrap();
    std::fs::write(&r, "id,v\n1,a\n2,c\n").unwrap();
    std::fs::write(dir.join("out.csv.1.part"), "another file\n").unwrap();
    let side = |p: &std::path::Path| json!({ "path": p, "options": { "encoding": "utf8", "delimiter": 44, "has_header": true } });
    let s = Arc::new(Service::new());
    ok(&s, "compare", json!({ "left": side(&l), "right": side(&r), "keys": ["id"] }));
    wait_ready(&s);
    let v = ok(&s, "view", json!({ "order": "aligned" }));
    for format in ["csv", "xlsx"] {
        let out = dir.join("out.csv");
        ok(&s, "export", json!({ "view": v["view"], "path": out, "slots": [0, 1], "side": "both", "format": format }));
        assert!(std::fs::metadata(&out).unwrap().len() > 0, "{format}");
        assert_eq!(std::fs::read_to_string(&l).unwrap(), "id,v\n1,a\n2,b\n", "{format}: the compared file is kept");
        assert_eq!(std::fs::read_to_string(dir.join("out.csv.1.part")).unwrap(), "another file\n", "{format}");
        assert!(!dir.join("out.csv.2.part").exists(), "{format}: the temporary file is renamed");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A saved file that exists but cannot be read is left alone: nothing is
/// written over it, and the reason is shown as the save error.
#[test]
fn an_unreadable_saved_file_is_never_overwritten() {
    let dir = std::env::temp_dir().join(format!("cdr-review-unreadable-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let s = Arc::new(Service::with_data_dir(dir.clone()));
    ok(&s, "compare", request(&["社員番号"]));
    let id = wait_ready(&s)["comparison_id"].clone();
    let id = match id.as_str() {
        Some(i) => i.to_string(),
        None => {
            // The ID is known once both files are hashed.
            let t = Instant::now();
            loop {
                if let Some(i) = s.status()["phase"]["summary"]["comparison_id"].as_str() {
                    break i.to_string();
                }
                assert!(t.elapsed() < Duration::from_secs(20));
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };
    s.reset();
    // Something the app cannot read where the saved file belongs (a folder stands in for a locked file).
    let path = dir.join(format!("{id}.json"));
    std::fs::create_dir_all(path.join("x")).unwrap();
    let s2 = Arc::new(Service::with_data_dir(dir.clone()));
    ok(&s2, "compare", request(&["社員番号"]));
    wait_ready(&s2);
    let sum = wait_review_ready(&s2);
    assert!(sum["load_problem"].as_str().unwrap().contains("読めませんでした"), "{sum}");
    let v = ok(&s2, "view", json!({ "order": "aligned", "filter": { "kinds": ["changed"] } }));
    let rows = ok(&s2, "rows", json!({ "view": v["view"], "start": 0, "count": 1, "col_start": 0, "col_count": 1 }));
    ok(&s2, "review_set", json!({ "units": [rows["rows"][0]["unit"]], "status": "checked" }));
    let err = s2.flush().unwrap_err();
    assert!(err.contains("上書きしない"), "{err}");
    assert!(path.join("x").is_dir(), "left as it was");
    std::fs::remove_dir_all(&dir).ok();
}

/// The same comparison open twice (two windows): the first one saves, the
/// second says it does not, so neither writes over the other.
#[test]
fn a_second_window_with_the_same_comparison_does_not_save() {
    let dir = std::env::temp_dir().join(format!("cdr-review-twice-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let a = Arc::new(Service::with_data_dir(dir.clone()));
    let b = Arc::new(Service::with_data_dir(dir.clone()));
    for s in [&a, &b] {
        ok(s, "compare", request(&["社員番号"]));
        wait_ready(s);
        wait_review_ready(s);
    }
    let mark = |s: &Arc<Service>, status: &str| {
        let v = ok(s, "view", json!({ "order": "aligned", "filter": { "kinds": ["changed"] } }));
        let rows = ok(s, "rows", json!({ "view": v["view"], "start": 0, "count": 1, "col_start": 0, "col_count": 1 }));
        ok(s, "review_set", json!({ "units": [rows["rows"][0]["unit"]], "status": status }));
    };
    mark(&a, "checked");
    mark(&b, "investigate");
    a.flush().unwrap();
    let err = b.flush().unwrap_err();
    assert!(err.contains("別のウィンドウ"), "{err}");
    // Clearing from a third place skips the comparison held open by `a`.
    let c = Arc::new(Service::with_data_dir(dir.clone()));
    let cleared = ok(&c, "review_store", json!({ "action": "clear" }));
    assert_eq!(cleared["in_use"], 1, "{cleared}");
    // `a` closes: its states are what the file holds.
    a.reset();
    b.reset();
    let d = Arc::new(Service::with_data_dir(dir.clone()));
    ok(&d, "compare", request(&["社員番号"]));
    wait_ready(&d);
    let sum = wait_review_ready(&d);
    assert_eq!((sum["checked"].as_u64(), sum["investigate"].as_u64()), (Some(1), Some(0)));
    std::fs::remove_dir_all(&dir).ok();
}
