//! Writing rows of a view to a CSV or Excel file.

use std::collections::HashMap;
use std::path::PathBuf;

use cdr_engine::export::{export_csv, export_xlsx, kind_label, ExportError, ExportFormat, ExportRequest, Extra};
use cdr_engine::view::View;
use cdr_engine::{CancelToken, Table};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{bad, parse, Reply};
use crate::review::{Entry, Status};
use crate::state::{order_key, Ready};
use crate::Service;

#[derive(Deserialize)]
struct ExportReq {
    view: u64,
    path: PathBuf,
    /// Lines for the conditions sheet of an Excel export, from the screen
    /// (how the view was filtered and sorted, the time of the export).
    #[serde(default)]
    info: Vec<(String, String)>,
    /// Add the review state and memo columns.
    #[serde(default)]
    review: bool,
    #[serde(flatten)]
    export: ExportRequest,
}

/// The conditions sheet of an Excel export: the files, the comparison
/// rules and counts, then the lines the screen sent.
fn export_info(r: &Ready, view: &View, req: &ExportRequest, extra: &[(String, String)]) -> Vec<(String, String)> {
    let c = &r.cmp;
    let side = |t: &Table| {
        let enc = t.info.encoding.label();
        (
            t.info.path.as_ref().map_or_else(|| t.info.name.clone(), |p| p.display().to_string()),
            format!(
                "{}件、{}列、{enc}{}、{}、SHA-256 {}",
                t.len(),
                t.ncols(),
                if t.info.bom { "（BOMあり）" } else { "" },
                t.info.layout_label(),
                t.sha256().unwrap_or_else(|| "計算中".into())
            ),
        )
    };
    let name = |k: u32| c.left.headers[c.mapping.pairs[k as usize].left as usize].clone();
    let (lp, ld) = side(&c.left);
    let (rp, rd) = side(&c.right);
    let k = c.counts;
    let start = req.start.min(view.len());
    let end = req.end.unwrap_or(usize::MAX).min(view.len()).max(start);
    let mut v = vec![
        ("左のファイル".to_string(), lp),
        ("".to_string(), ld),
        ("右のファイル".to_string(), rp),
        ("".to_string(), rd),
        ("キー".to_string(), c.spec.keys.iter().map(|&k| name(k)).collect::<Vec<_>>().join(" + ")),
        (
            "比較しない列".to_string(),
            if c.spec.excluded.is_empty() {
                "なし".to_string()
            } else {
                c.spec.excluded.iter().map(|&k| name(k)).collect::<Vec<_>>().join("、")
            },
        ),
        (
            "値の前後の空白".to_string(),
            match c.spec.trim {
                cdr_engine::TrimRule::None => "違いとして扱う".to_string(),
                cdr_engine::TrimRule::AsciiSpaceTab => {
                    "半角スペースとタブの違いを無視する（キーの列を除く）".to_string()
                }
            },
        ),
        (
            "件数".to_string(),
            format!(
                "{} {}、{} {}、{} {}、{} {}",
                kind_label(cdr_engine::Kind::Changed),
                k.changed,
                kind_label(cdr_engine::Kind::Deleted),
                k.deleted,
                kind_label(cdr_engine::Kind::Added),
                k.added,
                kind_label(cdr_engine::Kind::Same),
                k.same
            ),
        ),
        ("書き出した行数".to_string(), (end - start).to_string()),
    ];
    if let Some(id) = r.id.read().unwrap().clone() {
        v.push(("比較ID".to_string(), id));
    }
    v.extend(extra.iter().cloned());
    v
}

impl Service {
    /// Writes rows of a view to a CSV or Excel file. The file is written
    /// under a temporary name and renamed when complete; the compared files
    /// are never written.
    pub(crate) fn export(&self, args: &Value) -> Reply {
        let r = self.ready()?;
        let q: ExportReq = parse(args)?;
        let view = self.view_by_id(q.view)?;
        let shown = q.path.display().to_string();
        if let Ok(target) = std::fs::canonicalize(&q.path) {
            for t in [&r.cmp.left, &r.cmp.right] {
                if t.info.path.as_ref().and_then(|p| std::fs::canonicalize(p).ok()).as_ref() == Some(&target) {
                    return Err(bad(format!(
                        "比較しているファイル（{shown}）には書き出せません。別の名前を指定してください。"
                    )));
                }
            }
        }
        let name = q.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = q.path.with_file_name(format!("{name}.part"));
        let slots = &r.slots[order_key(view.order)];
        let cancel = CancelToken::new();
        // A copy of the review states, so the export does not hold the lock.
        let states: HashMap<u32, Entry> = if q.review {
            r.reviews.lock().unwrap().entries().map(|(&u, e)| (u, e.clone())).collect()
        } else {
            HashMap::new()
        };
        let diff = |u: u32| r.cmp.units[u as usize].kind != cdr_engine::Kind::Same;
        let review_values = |u: u32| -> Vec<String> {
            let e = states.get(&u);
            let status =
                if diff(u) { e.map_or(Status::Unchecked, |e| e.status).label().to_string() } else { String::new() };
            vec![status, e.map(|e| e.memo.clone()).unwrap_or_default()]
        };
        let extra = Extra { names: vec!["確認状況".into(), "メモ".into()], values: &review_values };
        let extra = q.review.then_some(&extra);
        let written = match q.export.format {
            ExportFormat::Csv => std::fs::File::create(&tmp).map_err(ExportError::Io).and_then(|file| {
                let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
                let n = export_csv(&view, &r.cmp, slots, &q.export, extra, &mut w, &cancel)?;
                w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
                Ok(n)
            }),
            ExportFormat::Xlsx => {
                let mut info = export_info(&r, &view, &q.export, &q.info);
                if q.review {
                    let s = self.review_summary(&r);
                    info.push((
                        "確認状況".to_string(),
                        format!(
                            "確認済み {}、要調査 {}、未確認 {}（書き出した時点）",
                            s["checked"], s["investigate"], s["unchecked"]
                        ),
                    ));
                }
                export_xlsx(&view, &r.cmp, slots, &q.export, extra, &info, &tmp, &cancel)
            }
        };
        let rows = match written {
            Ok(n) => n,
            Err(e) => {
                std::fs::remove_file(&tmp).ok();
                return Err(bad(match e {
                    ExportError::Io(e) => format!("「{shown}」に書き出せませんでした（{e}）。保存先を変えてください。"),
                    e => e.to_string(),
                }));
            }
        };
        if let Err(e) = std::fs::rename(&tmp, &q.path) {
            std::fs::remove_file(&tmp).ok();
            return Err(bad(format!(
                "「{shown}」を置き換えられませんでした（{e}）。ほかのアプリで開いている場合は閉じてから書き出してください。"
            )));
        }
        Ok(json!({ "rows": rows, "path": q.path }))
    }
}
