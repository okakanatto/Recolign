//! Read-only snapshot of an input file.
//!
//! The whole file is read into memory once. The file's size and modification
//! time are compared before and after reading; any difference is an error, so
//! a comparison never mixes two versions of a file. On Windows the file is
//! opened without write sharing first, so other programs cannot write while
//! it is being read.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::cancel::{CancelToken, SideProgress};
use crate::error::LoadError;

pub struct Snapshot {
    pub bytes: Vec<u8>,
    pub modified: Option<SystemTime>,
    /// False when the file had to be opened with write sharing allowed.
    pub write_locked: bool,
}

const READ_CHUNK: u64 = 8 << 20;

#[cfg(windows)]
fn open_read_only(path: &Path) -> std::io::Result<(File, bool)> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(path) {
        Ok(f) => Ok((f, true)),
        // Another program holds the file open for writing (for example a
        // spreadsheet application). Read it anyway; the size/time check below
        // still detects changes made while reading.
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)
            .map(|f| (f, false)),
        Err(e) => Err(e),
    }
}

#[cfg(not(windows))]
fn open_read_only(path: &Path) -> std::io::Result<(File, bool)> {
    File::open(path).map(|f| (f, false))
}

pub fn read_snapshot(path: &Path, cancel: &CancelToken, progress: &SideProgress) -> Result<Snapshot, LoadError> {
    let io = |source| LoadError::Io { path: path.to_path_buf(), source };
    let before = std::fs::metadata(path).map_err(io)?;
    if !before.is_file() {
        return Err(LoadError::NotAFile { path: path.to_path_buf() });
    }
    let (file, write_locked) = open_read_only(path).map_err(io)?;
    let opened = file.metadata().map_err(io)?;
    if !opened.is_file() {
        return Err(LoadError::NotAFile { path: path.to_path_buf() });
    }
    let len = opened.len();
    progress.bytes_total.store(len, Ordering::Relaxed);
    let capacity = usize::try_from(len.saturating_add(1))
        .map_err(|_| LoadError::TooLarge { name: path.display().to_string(), what: "ファイルサイズ" })?;
    let mut bytes: Vec<u8> = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| LoadError::TooLarge {
            name: path.display().to_string(), what: "ファイルサイズ（メモリ不足）"
        })?;
    // One byte more than the size, to notice a file that grew, and no more:
    // the buffer never grows past what was reserved.
    let mut file = file.take(len.saturating_add(1));
    loop {
        if cancel.is_cancelled() {
            return Err(LoadError::Cancelled);
        }
        let n = (&mut file).take(READ_CHUNK).read_to_end(&mut bytes).map_err(io)?;
        progress.bytes_read.store(bytes.len() as u64, Ordering::Relaxed);
        if n == 0 {
            break;
        }
    }
    let after_handle = file.get_ref().metadata().map_err(io)?;
    let after_path = std::fs::metadata(path).map_err(io)?;
    let modified = before.modified().ok();
    if bytes.len() as u64 != len
        || after_handle.len() != len
        || after_path.len() != len
        || after_path.modified().ok() != modified
        || after_handle.modified().ok() != modified
    {
        return Err(LoadError::Changed { path: path.to_path_buf() });
    }
    Ok(Snapshot { bytes, modified, write_locked })
}

/// SHA-256 of a snapshot, computed on its own thread so that the comparison
/// can be shown before the hash is known. The hash identifies the input for
/// saved review states; the data on screen always comes from the same
/// in-memory bytes, so showing it earlier does not change what is shown.
#[derive(Clone)]
pub struct SnapshotHash {
    inner: Arc<HashInner>,
}

struct HashInner {
    state: Mutex<HashState>,
    done: Condvar,
    abandon: AtomicBool,
}

#[derive(Clone)]
enum HashState {
    Running,
    Done { hex: String, elapsed: Duration },
    Stopped,
}

impl SnapshotHash {
    pub fn spawn<T: AsRef<[u8]> + Send + Sync + 'static>(data: Arc<T>, cancel: CancelToken) -> Self {
        let inner = Arc::new(HashInner {
            state: Mutex::new(HashState::Running),
            done: Condvar::new(),
            abandon: AtomicBool::new(false),
        });
        let worker = inner.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            let mut h = Sha256::new();
            let mut stopped = false;
            for chunk in (*data).as_ref().chunks(4 << 20) {
                if cancel.is_cancelled() || worker.abandon.load(Ordering::Relaxed) {
                    stopped = true;
                    break;
                }
                h.update(chunk);
            }
            let state = if stopped {
                HashState::Stopped
            } else {
                HashState::Done { hex: to_hex(&h.finalize()), elapsed: t.elapsed() }
            };
            *worker.state.lock().unwrap() = state;
            worker.done.notify_all();
        });
        SnapshotHash { inner }
    }

    /// The hash if it is already known.
    pub fn get(&self) -> Option<String> {
        match &*self.inner.state.lock().unwrap() {
            HashState::Done { hex, .. } => Some(hex.clone()),
            _ => None,
        }
    }

    /// Blocks until the hash is known. `None` if it was cancelled.
    pub fn wait(&self) -> Option<(String, Duration)> {
        let mut st = self.inner.state.lock().unwrap();
        loop {
            match &*st {
                HashState::Running => st = self.inner.done.wait(st).unwrap(),
                HashState::Done { hex, elapsed } => return Some((hex.clone(), *elapsed)),
                HashState::Stopped => return None,
            }
        }
    }

    /// Stops the computation when the table is dropped.
    pub(crate) fn abandon(&self) {
        self.inner.abandon.store(true, Ordering::Relaxed);
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}
