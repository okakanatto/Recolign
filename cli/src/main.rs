//! `cdr`: command-line front end of the engine, for measurements and the oracle.
//!
//!   cdr compare --left A.csv --right B.csv --key COL [--key COL ...] [options]
//!   cdr cancel-probe --left A.csv --right B.csv --key COL --delays 10,100,1000
//!
//! `compare` prints one JSON object with counts, stage timings and memory.
//! With `--canonical FILE` it also writes every unit as JSON Lines for the
//! independent oracle (tools/oracle.py). With `--probe N` it measures range
//! queries and other interactive operations N times each.

mod canonical;
mod probe;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cdr_engine::pattern::aggregate;
use cdr_engine::pipeline::{comparison_id, run, CompareRequest, SideInput};
use cdr_engine::sysmem;
use cdr_engine::{
    CancelToken, CompareError, FixedWidth, LoadOptions, Progress, Stage, TextEncoding, TrimRule, WidthUnit,
};
use serde_json::{json, Value};

const USAGE: &str = "usage:
  cdr compare --left FILE --right FILE --key NAME [--key NAME ...]
              [--encoding utf8|windows31j|utf16le|utf16be|eucjp] [--delimiter ,|tab|;|C] [--no-quotes]
              [--fixed W,W,... [--fixed-unit bytes|chars] [--keep-padding]] [--no-header]
              [--exclude NAME ...] [--trim ascii]
              [--patterns] [--canonical OUT.jsonl] [--probe N] [--output OUT.json]
  cdr cancel-probe (same input options) --delays MS[,MS...]

The reading options apply to both files; with --left- or --right- before
the name (--right-encoding, --left-fixed, ...) to one of them.";

struct Args {
    cmd: String,
    req: CompareRequest,
    patterns: bool,
    canonical: Option<PathBuf>,
    probe: usize,
    output: Option<PathBuf>,
    delays: Vec<u64>,
}

fn parse_encoding(v: &str) -> Result<TextEncoding, String> {
    match v.to_ascii_lowercase().as_str() {
        "utf8" | "utf-8" => Ok(TextEncoding::Utf8),
        "windows31j" | "windows-31j" | "cp932" | "sjis" | "shift_jis" => Ok(TextEncoding::Windows31j),
        "utf16le" | "utf-16le" | "utf16" | "utf-16" => Ok(TextEncoding::Utf16le),
        "utf16be" | "utf-16be" => Ok(TextEncoding::Utf16be),
        "eucjp" | "euc-jp" | "euc_jp" => Ok(TextEncoding::EucJp),
        _ => Err(format!(
            "文字コード「{v}」には対応していません。utf8、windows31j、utf16le、utf16be、eucjp のいずれかを指定してください。"
        )),
    }
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().ok_or(USAGE)?;
    let (mut left, mut right) = (None, None);
    // The reading options of the left and the right file.
    let mut sides = [LoadOptions::default(), LoadOptions::default()];
    let mut keys = Vec::new();
    let mut excluded = Vec::new();
    let mut trim = TrimRule::None;
    let mut a = Args {
        cmd,
        req: CompareRequest {
            left: SideInput { path: PathBuf::new(), options: LoadOptions::default() },
            right: SideInput { path: PathBuf::new(), options: LoadOptions::default() },
            keys: vec![],
            excluded: vec![],
            trim: TrimRule::None,
        },
        patterns: false,
        canonical: None,
        probe: 0,
        output: None,
        delays: vec![],
    };
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} の値がありません。\n{USAGE}"));
        // A reading option for both files, or with --left- or --right- for one.
        let (option, which) = match (flag.strip_prefix("--left-"), flag.strip_prefix("--right-")) {
            (Some(o), _) => (format!("--{o}"), 0..1),
            (_, Some(o)) => (format!("--{o}"), 1..2),
            _ => (flag.clone(), 0..2),
        };
        let sides = &mut sides[which];
        match option.as_str() {
            "--left" => left = Some(PathBuf::from(val()?)),
            "--right" => right = Some(PathBuf::from(val()?)),
            "--key" => keys.push(val()?),
            "--exclude" => excluded.push(val()?),
            "--encoding" => {
                let e = parse_encoding(&val()?)?;
                sides.iter_mut().for_each(|o| o.encoding = e);
            }
            "--delimiter" => {
                let delimiter = match val()?.as_str() {
                    "tab" | "\\t" => b'\t',
                    v if v.len() == 1 && v.is_ascii() => v.as_bytes()[0],
                    v => {
                        return Err(format!(
                            "区切り文字「{v}」には対応していません。半角の1文字か tab を指定してください。"
                        ))
                    }
                };
                sides.iter_mut().for_each(|o| o.delimiter = delimiter);
            }
            "--no-quotes" => sides.iter_mut().for_each(|o| o.quoted = false),
            "--fixed" => {
                let widths: Vec<u32> = val()?
                    .split(',')
                    .map(|s| s.trim().parse::<u32>().map_err(|_| format!("--fixed の幅「{s}」が数値ではありません。")))
                    .collect::<Result<_, _>>()?;
                let fixed = FixedWidth { widths, unit: WidthUnit::Bytes, trim: true };
                sides.iter_mut().for_each(|o| o.fixed = Some(fixed.clone()));
            }
            "--fixed-unit" => {
                let unit = match val()?.as_str() {
                    "bytes" => WidthUnit::Bytes,
                    "chars" => WidthUnit::Chars,
                    v => {
                        return Err(format!(
                            "--fixed-unit {v} には対応していません。bytes または chars を指定してください。"
                        ))
                    }
                };
                for o in sides.iter_mut() {
                    o.fixed.as_mut().ok_or(format!("{flag} の前に --fixed を指定してください。"))?.unit = unit;
                }
            }
            "--keep-padding" => {
                for o in sides.iter_mut() {
                    o.fixed.as_mut().ok_or(format!("{flag} の前に --fixed を指定してください。"))?.trim = false;
                }
            }
            "--no-header" => sides.iter_mut().for_each(|o| o.has_header = false),
            "--trim" => {
                trim = match val()?.as_str() {
                    "ascii" => TrimRule::AsciiSpaceTab,
                    "none" => TrimRule::None,
                    v => {
                        return Err(format!("--trim {v} には対応していません。ascii または none を指定してください。"))
                    }
                }
            }
            "--patterns" => a.patterns = true,
            "--canonical" => a.canonical = Some(PathBuf::from(val()?)),
            "--probe" => a.probe = val()?.parse().map_err(|_| "--probe には回数を指定してください。")?,
            "--output" => a.output = Some(PathBuf::from(val()?)),
            "--delays" => {
                a.delays = val()?
                    .split(',')
                    .map(|s| s.trim().parse::<u64>().map_err(|_| format!("--delays の値「{s}」が数値ではありません。")))
                    .collect::<Result<_, _>>()?
            }
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("不明なオプション {other}\n{USAGE}")),
        }
    }
    let [lopts, ropts] = sides;
    a.req.left = SideInput { path: left.ok_or("--left を指定してください。")?, options: lopts };
    a.req.right = SideInput { path: right.ok_or("--right を指定してください。")?, options: ropts };
    a.req.keys = keys;
    a.req.excluded = excluded;
    a.req.trim = trim;
    Ok(a)
}

pub fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e6).round() / 1e3
}

/// Samples resident memory every few milliseconds and keeps the maximum per
/// pipeline stage.
struct MemorySampler {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    peaks: Arc<Mutex<Vec<(String, u64)>>>,
}

impl MemorySampler {
    fn start(progress: Arc<Progress>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peaks: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let (s, p) = (stop.clone(), peaks.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let stage = serde_json::to_value(progress.stage())
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                let rss = sysmem::current();
                let mut v = p.lock().unwrap();
                match v.iter_mut().find(|(n, _)| *n == stage) {
                    Some((_, m)) => *m = (*m).max(rss),
                    None => v.push((stage, rss)),
                }
                drop(v);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        MemorySampler { stop, handle: Some(handle), peaks }
    }

    fn finish(mut self) -> Value {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let v = self.peaks.lock().unwrap();
        Value::Object(v.iter().map(|(k, b)| (k.clone(), json!(b))).collect())
    }
}

fn side_json(t: &cdr_engine::Table) -> Value {
    json!({
        "name": t.info.name,
        "bytes": t.info.bytes,
        "sha256": t.sha256(),
        "records": t.len(),
        "columns": t.ncols(),
        "encoding": t.info.encoding,
        "bom": t.info.bom,
        "line_endings": t.info.line_endings,
        "write_locked": t.info.write_locked,
    })
}

fn environment() -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "rayon_threads": rayon_threads(),
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        "engine_version": env!("CARGO_PKG_VERSION"),
    })
}

fn rayon_threads() -> usize {
    std::env::var("RAYON_NUM_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))
}

fn error_json(e: &CompareError) -> Value {
    let detail = match e {
        CompareError::KeyValidation(r) => serde_json::to_value(r.as_ref()).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    json!({ "ok": false, "error": { "code": e.code(), "message": e.to_string(), "detail": detail } })
}

fn emit(out: &Value, path: &Option<PathBuf>) {
    let text = serde_json::to_string_pretty(out).expect("json");
    match path {
        Some(p) => {
            std::fs::write(p, text + "\n").unwrap_or_else(|e| eprintln!("{}: 書き込めません（{e}）", p.display()))
        }
        None => println!("{text}"),
    }
}

fn cmd_compare(a: &Args) -> ExitCode {
    let progress = Arc::new(Progress::default());
    let cancel = CancelToken::new();
    let sampler = MemorySampler::start(progress.clone());
    let started = Instant::now();
    let outcome = run(&a.req, &cancel, &progress);
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            sampler.finish();
            emit(&error_json(&e), &a.output);
            return ExitCode::from(2);
        }
    };
    let mem_usable = sysmem::read();
    let patterns = if a.patterns || a.canonical.is_some() {
        progress.set_stage(Stage::Patterns);
        match aggregate(&outcome.comparison, &cancel) {
            Ok(p) => Some(p),
            Err(e) => {
                sampler.finish();
                emit(&error_json(&e), &a.output);
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };
    progress.set_stage(Stage::Done);
    // The SHA-256 of both inputs runs in the background from the moment each
    // file is read; the comparison id needs it.
    let id = comparison_id(&outcome.comparison);
    let identity_ready = started.elapsed();
    let stage_peaks = sampler.finish();
    let mem_end = sysmem::read();
    let c = &outcome.comparison;
    let hash_ms = |t: &cdr_engine::Table| t.wait_sha256().map(|(_, d)| ms(d));
    let t = &outcome.timings;
    let mut out = json!({
        "ok": true,
        "environment": environment(),
        "comparison_id": id,
        "left": side_json(&c.left),
        "right": side_json(&c.right),
        "columns": {
            "pairs": c.mapping.pairs.len(),
            "left_only": c.mapping.left_only.len(),
            "right_only": c.mapping.right_only.len(),
        },
        "counts": c.counts,
        "timings_ms": {
            "read_left": ms(t.left.read),
            "read_right": ms(t.right.read),
            "decode_check_left": ms(t.left.decode_check),
            "decode_check_right": ms(t.right.decode_check),
            "index_left": ms(t.left.index),
            "index_right": ms(t.right.index),
            "hash_left": hash_ms(&c.left),
            "hash_right": hash_ms(&c.right),
            "identity_ready": ms(identity_ready),
            "load_wall": ms(t.load),
            "key_index": ms(t.key_index),
            "compare": ms(t.compare),
            "finalize": ms(t.finalize),
            "total_to_usable": ms(t.total),
            "patterns": patterns.as_ref().map(|p| ms(p.elapsed)),
        },
        "memory": {
            "at_usable": mem_usable,
            "at_end": mem_end,
            "stage_peak_rss": stage_peaks,
            "engine_index_bytes": c.index_bytes(),
        },
    });
    if let Some(p) = &patterns {
        out["patterns"] = json!({
            "count": p.list.len(),
            "largest": p.list.first().map_or(0, |x| x.units.len()),
            "hash_collisions": p.hash_collisions,
        });
    }
    if a.probe > 0 {
        out["probe"] = probe::run(c, patterns.as_ref(), a.probe);
    }
    if let Some(path) = &a.canonical {
        let t = Instant::now();
        if let Err(e) = canonical::write(path, c, patterns.as_ref()) {
            eprintln!("{}: 書き込めません（{e}）", path.display());
            return ExitCode::from(1);
        }
        out["canonical"] = json!({ "path": path, "write_ms": ms(t.elapsed()) });
    }
    emit(&out, &a.output);
    ExitCode::SUCCESS
}

/// Starts a comparison, cancels it after each delay and measures how long the
/// worker takes to stop. A run that finished before the cancel is reported
/// as such, not as a successful cancellation.
fn cmd_cancel_probe(a: &Args) -> ExitCode {
    let mut results = Vec::new();
    for &delay in &a.delays {
        let progress = Arc::new(Progress::default());
        let cancel = CancelToken::new();
        let req = a.req.clone();
        let (p2, c2) = (progress.clone(), cancel.clone());
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            let r = run(&req, &c2, &p2);
            let done = Instant::now();
            let r = r.and_then(|o| aggregate(&o.comparison, &c2).map(|_| ()));
            (r, done, Instant::now())
        });
        std::thread::sleep(Duration::from_millis(delay));
        let stage = progress.stage();
        let cancelled_at = Instant::now();
        cancel.cancel();
        let (r, _, finished) = worker.join().expect("worker");
        let outcome = match &r {
            Err(CompareError::Cancelled) => "cancelled",
            Ok(()) => "completed_before_cancel",
            Err(_) => "failed",
        };
        let stop_ms = if finished > cancelled_at { ms(finished - cancelled_at) } else { 0.0 };
        results.push(json!({
            "delay_ms": delay,
            "stage_at_cancel": stage,
            "outcome": outcome,
            "stop_ms": stop_ms,
            "elapsed_ms": ms(finished - started),
        }));
    }
    emit(&json!({ "ok": true, "environment": environment(), "cancel_probe": results }), &a.output);
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(64);
        }
    };
    match args.cmd.as_str() {
        "compare" => cmd_compare(&args),
        "cancel-probe" => cmd_cancel_probe(&args),
        other => {
            eprintln!("不明なコマンド {other}\n{USAGE}");
            ExitCode::from(64)
        }
    }
}
