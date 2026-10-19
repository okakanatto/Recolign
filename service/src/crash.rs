//! What happens when the program itself has a bug (a panic).
//!
//! The panic is written to a log file in the user's profile (time, version,
//! thread, place in the code and the call stack; never values from the
//! compared files), and the command, the comparison or the background save
//! that hit it reports an internal error with the log's location instead of
//! hanging or ending the app.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The version and the commit it was built from, e.g. `0.1.0 (1a2b3c4d5)`.
pub fn version() -> String {
    format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("CDR_COMMIT"))
}

static LOG: OnceLock<PathBuf> = OnceLock::new();

/// The crash log, once `install` has been called.
pub fn log_file() -> Option<&'static Path> {
    LOG.get().map(PathBuf::as_path)
}

/// Writes every panic to `dir/crash.log` (kept under 1 MB: the older half
/// moves to `crash.old.log`), then lets the default handling continue.
pub fn install(dir: PathBuf) {
    let file = dir.join("crash.log");
    if LOG.set(file.clone()).is_err() {
        return;
    }
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // The panic's message is left out: it can quote a value (a string
        // cut in the middle of a character shows the string).
        let place = info.location().map_or_else(String::new, |l| format!("{}:{}", l.file(), l.line()));
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let trace = std::backtrace::Backtrace::force_capture();
        let entry = format!("---- {} UTC, Recolign {}\nthread {thread} at {place}\n{trace}\n", utc_now(), version());
        append(&file, entry.as_bytes());
        default(info);
    }));
}

fn append(file: &Path, data: &[u8]) {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if std::fs::metadata(file).is_ok_and(|m| m.len() > 1 << 20) {
        std::fs::rename(file, file.with_file_name("crash.old.log")).ok();
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
        f.write_all(data).ok();
    }
}

/// Writes an error the screen caught (a bug in the page's code) to the
/// same log. Long texts are cut. The page sends the kind of error and where
/// it happened, not its message.
pub fn log_screen_error(text: &str) {
    if let Some(file) = log_file() {
        let text: String = text.chars().take(4000).collect();
        append(file, format!("---- {} UTC, Recolign {}, screen\n{text}\n", utc_now(), version()).as_bytes());
    }
}

/// The message shown when a bug stopped `what`.
pub fn message(what: &str) -> String {
    let log = log_file().map_or_else(String::new, |p| format!("記録は {} にあります。", p.display()));
    format!(
        "{what}の途中で、プログラムの不具合による内部エラーが起きました。アプリを開き直してから、もう一度操作してください。{log}続けて起きるときは、この記録を添えて開発者に知らせてください。"
    )
}

/// The current time as `YYYY-MM-DD HH:MM:SS` (UTC).
fn utc_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    civil(secs as i64)
}

/// Seconds since 1970 as a UTC date and time, without a date library
/// (H. Hinnant's days-to-civil algorithm).
fn civil(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::civil;

    #[test]
    fn times_are_utc_calendar_times() {
        assert_eq!(civil(0), "1970-01-01 00:00:00");
        assert_eq!(civil(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(civil(1_791_936_061), "2026-10-14 00:01:01");
    }
}
