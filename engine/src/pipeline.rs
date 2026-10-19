//! End-to-end comparison: load both files, map columns, resolve keys,
//! validate and match. Pattern aggregation is a separate step so the
//! side-by-side view can open before it finishes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cancel::{CancelToken, Progress, Stage};
use crate::columns::ColumnMapping;
use crate::compare::{CompareSpec, Comparison, TrimRule};
use crate::error::{CompareError, LoadError, Side};
use crate::source::to_hex;
use crate::table::{LoadOptions, LoadTimings, SourceInfo, Table};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SideInput {
    pub path: PathBuf,
    pub options: LoadOptions,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompareRequest {
    pub left: SideInput,
    pub right: SideInput,
    /// Key column names (columns are paired by name).
    pub keys: Vec<String>,
    #[serde(default)]
    pub excluded: Vec<String>,
    #[serde(default)]
    pub trim: TrimRule,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PipelineTimings {
    pub left: LoadTimings,
    pub right: LoadTimings,
    /// Wall time for loading both files in parallel.
    pub load: Duration,
    pub key_index: Duration,
    pub compare: Duration,
    pub finalize: Duration,
    /// From the start of reading to a usable comparison. Excludes pattern
    /// aggregation and the SHA-256 of the inputs (both run afterwards).
    pub total: Duration,
}

pub struct Outcome {
    pub comparison: Comparison,
    pub timings: PipelineTimings,
}

fn resolve(
    cmp_names: &[String],
    left: &Table,
    right: &Table,
    mapping: &ColumnMapping,
    what: &str,
) -> Result<Vec<u32>, CompareError> {
    cmp_names
        .iter()
        .map(|name| {
            if let Some(k) = mapping.pairs.iter().position(|p| left.headers[p.left as usize] == *name) {
                return Ok(k as u32);
            }
            let in_left = left.headers.iter().any(|h| h == name);
            let in_right = right.headers.iter().any(|h| h == name);
            let message = match (in_left, in_right) {
                (true, false) => {
                    format!("{what}「{name}」は左のファイルにしかありません。両方にある列を指定してください。")
                }
                (false, true) => {
                    format!("{what}「{name}」は右のファイルにしかありません。両方にある列を指定してください。")
                }
                _ => format!("{what}「{name}」という列がありません。列名を確認してください。"),
            };
            Err(CompareError::KeyColumn { message })
        })
        .collect()
}

/// How a file was read, as part of the comparison id. Options added later are
/// written only when not at their default, so that review states saved
/// before them keep their comparison id.
fn options_text(info: &SourceInfo) -> String {
    let mut s = format!("{:?}|{}|{}", info.encoding, info.delimiter, info.has_header);
    if !info.quoted {
        s += "|unquoted";
    }
    if let Some(f) = &info.fixed {
        s += &format!("|fixed {:?} {:?} trim={}", f.widths, f.unit, f.trim);
    }
    s
}

/// Identifies the inputs and the comparison rules. Review states belong to
/// (comparison id, key), never to a display row number. Waits for the
/// background SHA-256 of both files; `None` if hashing was cancelled.
pub fn comparison_id(cmp: &Comparison) -> Option<String> {
    let (left_sha, _) = cmp.left.wait_sha256()?;
    let (right_sha, _) = cmp.right.wait_sha256()?;
    let mut h = Sha256::new();
    let opts = |t: &Table| options_text(&t.info);
    h.update(format!("cdr-engine {}\n", env!("CARGO_PKG_VERSION")));
    h.update(format!("left {left_sha} {}\n", opts(&cmp.left)));
    h.update(format!("right {right_sha} {}\n", opts(&cmp.right)));
    for p in &cmp.mapping.pairs {
        h.update(format!("pair {} {}\n", p.left, p.right));
    }
    h.update(format!("keys {:?}\nexcluded {:?}\ntrim {:?}\n", cmp.spec.keys, cmp.spec.excluded, cmp.spec.trim));
    Some(to_hex(&h.finalize()))
}

/// Maps columns by name, resolves key and excluded column names, validates
/// keys and compares.
pub fn compare_tables(
    left: Table,
    right: Table,
    keys: &[String],
    excluded: &[String],
    trim: TrimRule,
    cancel: &CancelToken,
    progress: &Progress,
) -> Result<Comparison, CompareError> {
    let mapping = ColumnMapping::by_name(&left.headers, &right.headers)?;
    let keys = resolve(keys, &left, &right, &mapping, "突合キー")?;
    // The order in which columns were set aside means nothing: kept in column
    // order, so that the comparison (and its review states) is the same.
    let mut excluded = resolve(excluded, &left, &right, &mapping, "比較対象外の列")?;
    excluded.sort_unstable();
    excluded.dedup();
    if let Some(k) = excluded.iter().find(|k| keys.contains(k)) {
        let name = &left.headers[mapping.pairs[*k as usize].left as usize];
        return Err(CompareError::KeyColumn {
            message: format!("突合キー「{name}」は比較対象外にできません。")
        });
    }
    Comparison::run(left, right, mapping, CompareSpec { keys, excluded, trim }, cancel, progress)
}

pub fn run(req: &CompareRequest, cancel: &CancelToken, progress: &Progress) -> Result<Outcome, CompareError> {
    let t0 = Instant::now();
    progress.set_stage(Stage::Reading);
    let (l, r) = std::thread::scope(|s| {
        let lh = s.spawn(|| Table::load(&req.left.path, &req.left.options, cancel, &progress.left));
        let rh = s.spawn(|| Table::load(&req.right.path, &req.right.options, cancel, &progress.right));
        (lh.join().expect("left loader"), rh.join().expect("right loader"))
    });
    let load = t0.elapsed();
    let wrap = |side: Side, e: LoadError| match e {
        LoadError::Cancelled => CompareError::Cancelled,
        error => CompareError::Load { side, error },
    };
    let (left, lt) = l.map_err(|e| wrap(Side::Left, e))?;
    let (right, rt) = r.map_err(|e| wrap(Side::Right, e))?;
    if cancel.is_cancelled() {
        return Err(CompareError::Cancelled);
    }
    let comparison = compare_tables(left, right, &req.keys, &req.excluded, req.trim, cancel, progress)?;
    let total = t0.elapsed();
    progress.set_stage(Stage::Done);
    let ct = comparison.timings;
    Ok(Outcome {
        timings: PipelineTimings {
            left: lt,
            right: rt,
            load,
            key_index: ct.key_index,
            compare: ct.compare,
            finalize: ct.finalize,
            total,
        },
        comparison,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::TextEncoding;
    use crate::fixed::{FixedWidth, WidthUnit};

    #[test]
    fn the_comparison_id_of_default_options_did_not_change() {
        // Review states saved by earlier versions are found by this text.
        let t = |opts: LoadOptions| Table::from_bytes("t.csv", b"id\n1\n".to_vec(), &opts).unwrap();
        assert_eq!(options_text(&t(LoadOptions::default()).info), "Utf8|,|true");
        let tsv = LoadOptions { encoding: TextEncoding::Windows31j, delimiter: b'\t', ..Default::default() };
        assert_eq!(options_text(&t(tsv).info), "Windows31j|\t|true");
        let unquoted = LoadOptions { quoted: false, ..Default::default() };
        assert_eq!(options_text(&t(unquoted).info), "Utf8|,|true|unquoted");
        let fixed = FixedWidth { widths: vec![2], unit: WidthUnit::Chars, trim: true };
        let fixed = LoadOptions { fixed: Some(fixed), ..Default::default() };
        assert_eq!(options_text(&t(fixed.clone()).info), "Utf8||true|fixed [2] Chars trim=true");
        // The quote setting plays no part in fixed width, nor in its id.
        assert_eq!(
            options_text(&t(LoadOptions { quoted: false, ..fixed }).info),
            "Utf8||true|fixed [2] Chars trim=true"
        );
    }

    #[test]
    fn the_order_of_columns_set_aside_does_not_change_the_comparison() {
        let run = |excluded: &[&str]| {
            let t = || Table::from_bytes("t.csv", b"id,a,b\n1,x,y\n".to_vec(), &LoadOptions::default()).unwrap();
            let excluded: Vec<String> = excluded.iter().map(|s| s.to_string()).collect();
            let keys = ["id".to_string()];
            let (cancel, progress) = (CancelToken::default(), Progress::default());
            let cmp = compare_tables(t(), t(), &keys, &excluded, TrimRule::None, &cancel, &progress).unwrap();
            (cmp.spec.excluded.clone(), comparison_id(&cmp).unwrap())
        };
        assert_eq!(run(&["b", "a"]), run(&["a", "b"]));
        assert_eq!(run(&["b", "a", "b"]).0, vec![1, 2]);
    }
}
