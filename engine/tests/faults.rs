//! Failures while reading (AC-13): each one stops the comparison with a
//! message that names the file, and never yields a result.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cdr_engine::error::{CompareError, LoadError, Side};
use cdr_engine::pipeline::{run, CompareRequest, SideInput};
use cdr_engine::{CancelToken, LoadOptions, Progress, TrimRule};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cdr-faults-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&d).ok();
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn request(left: &Path, right: &Path) -> CompareRequest {
    let side = |p: &Path| SideInput { path: p.to_path_buf(), options: LoadOptions::default() };
    CompareRequest {
        left: side(left),
        right: side(right),
        keys: vec!["id".into()],
        excluded: vec![],
        trim: TrimRule::None,
    }
}

fn compare(left: &Path, right: &Path) -> Result<(), CompareError> {
    run(&request(left, right), &CancelToken::new(), &Progress::default()).map(|_| ())
}

#[test]
fn a_missing_file_is_named_with_what_to_check() {
    let d = dir("missing");
    let ok = d.join("ok.csv");
    std::fs::write(&ok, "id,v\n1,a\n").unwrap();
    let missing = d.join("ない.csv");
    let err = compare(&ok, &missing).unwrap_err();
    assert!(matches!(&err, CompareError::Load { side: Side::Right, error: LoadError::Io { .. } }), "{err:?}");
    let msg = err.to_string();
    assert!(msg.starts_with("右のファイル") && msg.contains("ない.csv") && msg.contains("パスとアクセス権"), "{msg}");
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn a_folder_instead_of_a_file_is_reported() {
    let d = dir("folder");
    let ok = d.join("ok.csv");
    std::fs::write(&ok, "id,v\n1,a\n").unwrap();
    let err = compare(&d, &ok).unwrap_err();
    assert!(matches!(&err, CompareError::Load { side: Side::Left, .. }), "{err:?}");
    std::fs::remove_dir_all(&d).ok();
}

/// Another program appends to the file while it is being read: the read is
/// rejected as changed, not compared with half of the new content.
#[test]
fn a_file_that_changes_while_read_is_not_compared() {
    let d = dir("changing");
    let ok = d.join("ok.csv");
    std::fs::write(&ok, "id,v\n1,a\n").unwrap();
    let growing = d.join("growing.csv");
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&growing).unwrap());
        writeln!(f, "id,v").unwrap();
        for i in 0..1_500_000 {
            writeln!(f, "{i},value-{i:08}").unwrap();
        }
    }
    // Opened before the comparison starts, as by a program that keeps the
    // file open (on Windows the reader then has to allow writing).
    let mut f = std::fs::OpenOptions::new().append(true).open(&growing).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::SeqCst) {
                writeln!(f, "x{i},appended").unwrap();
                f.flush().unwrap();
                i += 1;
                std::thread::yield_now();
            }
        })
    };
    let result = compare(&growing, &ok);
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    let err = result.unwrap_err();
    assert!(matches!(&err, CompareError::Load { side: Side::Left, error: LoadError::Changed { .. } }), "{err:?}");
    assert!(err.to_string().contains("読込中にファイルが変更されました"), "{err}");
    std::fs::remove_dir_all(&d).ok();
}

/// The input files are only read: their bytes and times are the same after
/// a comparison (AC-11).
#[test]
fn comparing_leaves_the_files_untouched() {
    let d = dir("untouched");
    let (l, r) = (d.join("l.csv"), d.join("r.csv"));
    std::fs::write(&l, "id,v\n1,a\n2,b\n").unwrap();
    std::fs::write(&r, "id,v\n1,a\n2,c\n").unwrap();
    let meta = |p: &Path| {
        let m = std::fs::metadata(p).unwrap();
        (m.len(), m.modified().unwrap(), std::fs::read(p).unwrap())
    };
    let (bl, br) = (meta(&l), meta(&r));
    compare(&l, &r).unwrap();
    assert_eq!((meta(&l), meta(&r)), (bl, br));
    std::fs::remove_dir_all(&d).ok();
}
