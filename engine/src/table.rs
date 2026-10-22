//! A loaded, validated table file: its text plus a per-record index.
//!
//! Two layouts: delimited text (CSV, TSV, any one ASCII delimiter, with or
//! without quotes; `tokenizer.rs`) and fixed-width text (`fixed.rs`).

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cancel::{CancelToken, SideProgress};
use crate::encoding::{self, TextEncoding, UTF16BE_BOM, UTF16LE_BOM, UTF8_BOM};
use crate::error::LoadError;
use crate::fixed::{self, FixedError, FixedWidth, WidthUnit};
use crate::source::{self, Snapshot, SnapshotHash};
use crate::tokenizer::{self, FieldSpan, IndexError, LineEndings, RecordPos};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoadOptions {
    pub encoding: TextEncoding,
    /// Any ASCII character but `"`, CR and LF; usually `,`, TAB, `;` or `|`.
    /// Not used for fixed-width text.
    pub delimiter: u8,
    pub has_header: bool,
    /// Values may be enclosed in `"` (CSV rules). Off: `"` is an ordinary
    /// character.
    #[serde(default = "yes")]
    pub quoted: bool,
    /// Fixed-width columns instead of a delimiter.
    #[serde(default)]
    pub fixed: Option<FixedWidth>,
}

fn yes() -> bool {
    true
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions { encoding: TextEncoding::Utf8, delimiter: b',', has_header: true, quoted: true, fixed: None }
    }
}

impl LoadOptions {
    /// Why these options cannot be used, if they cannot.
    fn problem(&self) -> Option<String> {
        match &self.fixed {
            None => {
                let d = self.delimiter;
                if !d.is_ascii() || matches!(d, b'"' | b'\r' | b'\n' | 0) {
                    return Some("区切り文字には、「\"」と改行以外の半角の1文字を指定してください。".into());
                }
            }
            Some(f) => {
                if f.widths.is_empty() || f.widths.contains(&0) {
                    return Some("固定長の列の幅を、1以上の数で1つ以上指定してください。".into());
                }
                if f.widths.iter().map(|&w| w as u64).sum::<u64>() > u32::MAX as u64 {
                    return Some("固定長の列の幅の合計が大きすぎます。".into());
                }
                if f.unit == WidthUnit::Bytes && self.encoding.is_utf16() {
                    return Some("UTF-16のファイルでは、列の幅を文字数で指定してください。".into());
                }
            }
        }
        None
    }

    /// The file is parsed as UTF-8 after conversion: UTF-16, and Windows-31J
    /// with a delimiter that can be the second byte of a character.
    fn converts_to_utf8(&self) -> bool {
        match self.encoding {
            TextEncoding::Utf8 => false,
            TextEncoding::Utf16le | TextEncoding::Utf16be => true,
            TextEncoding::Windows31j => self.fixed.is_none() && self.delimiter >= 0x40,
            TextEncoding::EucJp => false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceInfo {
    pub path: Option<PathBuf>,
    pub name: String,
    pub bytes: u64,
    pub modified_unix_ms: Option<i64>,
    pub encoding: TextEncoding,
    /// The delimiter; empty for fixed-width text.
    pub delimiter: String,
    pub quoted: bool,
    pub fixed: Option<FixedWidth>,
    pub has_header: bool,
    pub bom: bool,
    pub line_endings: LineEndings,
    pub write_locked: bool,
}

impl SourceInfo {
    /// How the file was cut into values, in words (for the conditions an
    /// export records).
    pub fn layout_label(&self) -> String {
        match &self.fixed {
            Some(f) => {
                let widths = f.widths.iter().map(|w| w.to_string()).collect::<Vec<_>>().join(", ");
                let trim = if f.trim { "、前後の空白を除く" } else { "" };
                format!("固定長（{}単位の幅 {widths}{trim}）", f.unit.label())
            }
            None => {
                let d = match self.delimiter.as_str() {
                    "," => "カンマ".to_string(),
                    "\t" => "タブ".to_string(),
                    ";" => "セミコロン".to_string(),
                    "|" => "縦線（|）".to_string(),
                    " " => "空白".to_string(),
                    other => format!("「{other}」"),
                };
                format!("区切り {d}{}", if self.quoted { "" } else { "、引用符なし" })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct LoadTimings {
    pub read: Duration,
    pub decode_check: Duration,
    pub index: Duration,
}

/// The text that records and values point into: UTF-8 (the file's own
/// bytes, or the file converted), or the original Windows-31J or EUC-JP
/// bytes.
enum Content {
    Utf8(String),
    Legacy(TextEncoding, Vec<u8>),
}

/// How a record is cut into fields.
enum Layout {
    Delimited { delimiter: u8, quoted: bool },
    Fixed(FixedWidth),
}

impl Content {
    fn bytes(&self) -> &[u8] {
        match self {
            Content::Utf8(s) => s.as_bytes(),
            Content::Legacy(_, b) => b,
        }
    }
}

impl AsRef<[u8]> for Content {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

/// The most columns a file may have, as in an Excel sheet. More are
/// usually a delimiter that does not fit the file.
pub const MAX_COLUMNS: usize = 16_384;

pub struct Table {
    pub info: SourceInfo,
    content: Arc<Content>,
    hash: SnapshotHash,
    layout: Layout,
    pub headers: Vec<String>,
    pub header_pos: Option<RecordPos>,
    /// Data records only (header excluded), in file order.
    pub records: Vec<RecordPos>,
}

fn unix_ms(t: SystemTime) -> Option<i64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as i64)
}

impl Drop for Table {
    fn drop(&mut self) {
        self.hash.abandon();
    }
}

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Table").field("name", &self.info.name).field("records", &self.records.len()).finish()
    }
}

impl Table {
    pub fn load(
        path: &Path,
        opts: &LoadOptions,
        cancel: &CancelToken,
        progress: &SideProgress,
    ) -> Result<(Table, LoadTimings), LoadError> {
        let t = Instant::now();
        let snap = source::read_snapshot(path, cancel, progress)?;
        let read = t.elapsed();
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let (table, mut timings) = Self::build(name, Some(path.to_path_buf()), snap, opts, cancel, progress)?;
        timings.read = read;
        Ok((table, timings))
    }

    pub fn from_bytes(name: &str, bytes: Vec<u8>, opts: &LoadOptions) -> Result<Table, LoadError> {
        let snap = Snapshot { bytes, modified: None, write_locked: false };
        Self::build(name.to_string(), None, snap, opts, &CancelToken::new(), &SideProgress::default()).map(|(t, _)| t)
    }

    fn build(
        name: String,
        path: Option<PathBuf>,
        snap: Snapshot,
        opts: &LoadOptions,
        cancel: &CancelToken,
        progress: &SideProgress,
    ) -> Result<(Table, LoadTimings), LoadError> {
        if let Some(message) = opts.problem() {
            return Err(LoadError::Options { name, message });
        }
        let mut timings = LoadTimings::default();
        let bytes = snap.bytes;
        let file_len = bytes.len() as u64;
        let bom = opts.encoding.bom();
        let has_bom = !bom.is_empty() && bytes.starts_with(bom);
        if !has_bom {
            for (other, mark) in [
                (TextEncoding::Utf8, UTF8_BOM),
                (TextEncoding::Utf16le, UTF16LE_BOM),
                (TextEncoding::Utf16be, UTF16BE_BOM),
            ] {
                if other != opts.encoding && bytes.starts_with(mark) {
                    return Err(LoadError::BomMismatch { name, bom: other, chosen: opts.encoding });
                }
            }
        }
        let bom_len = if has_bom { bom.len() } else { 0 };
        if bytes.len() == bom_len {
            return Err(LoadError::Empty { name });
        }

        // Validate the encoding and settle the text that is parsed. The
        // SHA-256 is always of the file's own bytes, computed meanwhile.
        let t = Instant::now();
        let check = |pos: usize| {
            progress.bytes_indexed.store(pos as u64 / 2, Ordering::Relaxed);
            cancel.is_cancelled()
        };
        let decode_error = |name: String, text: &[u8], off: usize, hint| LoadError::Decode {
            name,
            encoding: opts.encoding,
            line: tokenizer::line_of_offset(text, off),
            byte_offset: off as u64,
            hint,
        };
        let (content, hash, start) = match opts.encoding {
            TextEncoding::Utf8 => match String::from_utf8(bytes) {
                Ok(s) => {
                    let content = Arc::new(Content::Utf8(s));
                    let hash = SnapshotHash::spawn(content.clone(), cancel.clone());
                    (content, hash, bom_len)
                }
                Err(e) => {
                    let off = e.utf8_error().valid_up_to();
                    let b = e.into_bytes();
                    return Err(decode_error(name, &b, off, encoding::failure_hint(TextEncoding::Utf8, &b)));
                }
            },
            TextEncoding::Windows31j | TextEncoding::EucJp => {
                // Hashing runs while the bytes are validated: the hash thread
                // shares the bytes it hashes (the text itself, or the file
                // before conversion), so nothing is copied.
                let (shared, hash) = if opts.converts_to_utf8() {
                    let original = Arc::new(bytes);
                    let hash = SnapshotHash::spawn(original.clone(), cancel.clone());
                    (Err(original), hash)
                } else {
                    let content = Arc::new(Content::Legacy(opts.encoding, bytes));
                    let hash = SnapshotHash::spawn(content.clone(), cancel.clone());
                    (Ok(content), hash)
                };
                let b: &[u8] = match &shared {
                    Ok(content) => content.bytes(),
                    Err(original) => original,
                };
                match encoding::first_invalid_in(opts.encoding, b, check) {
                    Ok(None) => {}
                    Ok(Some(off)) => {
                        hash.abandon();
                        let hint = encoding::failure_hint(opts.encoding, b);
                        return Err(decode_error(name, b, off, hint));
                    }
                    Err(_) => {
                        hash.abandon();
                        return Err(LoadError::Cancelled);
                    }
                }
                let content = match shared {
                    Ok(content) => content,
                    Err(original) => Arc::new(Content::Utf8(encoding::decode(opts.encoding, &original).into_owned())),
                };
                (content, hash, 0)
            }
            TextEncoding::Utf16le | TextEncoding::Utf16be => {
                let original = Arc::new(bytes);
                let hash = SnapshotHash::spawn(original.clone(), cancel.clone());
                let body = &original[bom_len..];
                match encoding::utf16_to_utf8(body, opts.encoding == TextEncoding::Utf16be, check) {
                    Ok(Ok(text)) => (Arc::new(Content::Utf8(text)), hash, 0),
                    Ok(Err(off)) => {
                        hash.abandon();
                        // The line: count the line breaks in the part that was read.
                        let before = opts.encoding.codec().decode_without_bom_handling(&body[..off]).0;
                        let line = tokenizer::line_of_offset(before.as_bytes(), before.len());
                        return Err(LoadError::Decode {
                            name,
                            encoding: opts.encoding,
                            line,
                            byte_offset: (bom_len + off) as u64,
                            hint: None,
                        });
                    }
                    Err(_) => {
                        hash.abandon();
                        return Err(LoadError::Cancelled);
                    }
                }
            }
        };
        timings.decode_check = t.elapsed();

        let text_encoding = match *content {
            Content::Utf8(_) => TextEncoding::Utf8,
            Content::Legacy(e, _) => e,
        };
        let layout = match &opts.fixed {
            Some(f) => Layout::Fixed(f.clone()),
            None => Layout::Delimited { delimiter: opts.delimiter, quoted: opts.quoted },
        };
        let b = content.bytes();
        let total = b.len() as u64;
        let t = Instant::now();
        let on_progress = |pos: usize| {
            progress.bytes_indexed.store(pos as u64, Ordering::Relaxed);
            cancel.is_cancelled()
        };
        // A position in the text parsed, as a position in the file: for a
        // converted file, the bytes the text before it takes in the file.
        let file_offset = |off: u64| -> u64 {
            let before = match std::str::from_utf8(&b[..off as usize]) {
                Ok(before) if opts.converts_to_utf8() => before,
                _ => return off,
            };
            if opts.encoding.is_utf16() {
                (bom_len + 2 * before.encode_utf16().count()) as u64
            } else {
                // Windows-31J: one byte for U+0000–U+0080 and half-width katakana, two for the rest.
                before
                    .chars()
                    .map(|c| if c <= '\u{80}' || ('\u{FF61}'..='\u{FF9F}').contains(&c) { 1 } else { 2 })
                    .sum()
            }
        };
        let indexed = match &layout {
            Layout::Delimited { delimiter, quoted } => tokenizer::index(b, start, *delimiter, *quoted, on_progress)
                .map_err(|e| match e {
                    IndexError::Cancelled => LoadError::Cancelled,
                    IndexError::Csv { kind, line, byte_offset, record_line } => LoadError::Csv {
                        name: name.clone(),
                        kind,
                        line,
                        byte_offset: file_offset(byte_offset),
                        record_line,
                    },
                    IndexError::FieldCount { record_index, line, found, expected, blank } => LoadError::FieldCount {
                        name: name.clone(),
                        line,
                        record: record_index + if opts.has_header { 0 } else { 1 },
                        found,
                        expected,
                        blank,
                    },
                    IndexError::RecordTooLarge { .. } => {
                        LoadError::TooLarge { name: name.clone(), what: "1レコードの長さ" }
                    }
                    IndexError::OutOfMemory => {
                        LoadError::TooLarge { name: name.clone(), what: "レコードの数（メモリ不足）" }
                    }
                }),
            Layout::Fixed(f) => fixed::index(b, start, f, text_encoding, on_progress).map_err(|e| match e {
                FixedError::Cancelled => LoadError::Cancelled,
                FixedError::TooLong { line, length, total } => {
                    LoadError::FixedTooLong { name: name.clone(), line, length, total, unit: f.unit }
                }
                FixedError::SplitsCharacter { line, column } => {
                    LoadError::FixedSplitsCharacter { name: name.clone(), line, column }
                }
                FixedError::LineTooLarge { .. } => LoadError::TooLarge { name: name.clone(), what: "1行の長さ" },
                FixedError::OutOfMemory => {
                    LoadError::TooLarge { name: name.clone(), what: "行の数（メモリ不足）" }
                }
            }),
        };
        timings.index = t.elapsed();
        progress.bytes_indexed.store(total, Ordering::Relaxed);
        let mut indexed = match indexed {
            Ok(i) => i,
            Err(e) => {
                hash.abandon();
                return Err(e);
            }
        };

        if indexed.field_count > MAX_COLUMNS {
            hash.abandon();
            return Err(LoadError::TooManyColumns { name, found: indexed.field_count });
        }
        let header_pos =
            if opts.has_header && !indexed.records.is_empty() { Some(indexed.records.remove(0)) } else { None };
        let records = indexed.records;
        progress.records.store(records.len() as u64, Ordering::Relaxed);
        let ncols = indexed.field_count;
        let mut table = Table {
            info: SourceInfo {
                path,
                name,
                bytes: file_len,
                modified_unix_ms: snap.modified.and_then(unix_ms),
                encoding: opts.encoding,
                delimiter: if opts.fixed.is_some() { String::new() } else { (opts.delimiter as char).to_string() },
                // Quotes play no part in fixed width (and are not part of its comparison id).
                quoted: opts.quoted || opts.fixed.is_some(),
                fixed: opts.fixed.clone(),
                has_header: opts.has_header,
                bom: has_bom,
                line_endings: indexed.line_endings,
                write_locked: snap.write_locked,
            },
            content,
            hash,
            layout,
            headers: Vec::new(),
            header_pos,
            records,
        };
        table.headers = match header_pos {
            Some(pos) => {
                let mut spans = Vec::new();
                table.split_pos(pos, usize::MAX, &mut spans);
                spans.iter().map(|&s| table.value_at(pos, s).into_owned()).collect()
            }
            None => (1..=ncols).map(|i| format!("列{i}")).collect(),
        };
        Ok((table, timings))
    }

    /// SHA-256 of the file bytes, if the background computation finished.
    pub fn sha256(&self) -> Option<String> {
        self.hash.get()
    }

    /// Waits for the SHA-256 of the file bytes. `None` if it was cancelled.
    pub fn wait_sha256(&self) -> Option<(String, Duration)> {
        self.hash.wait()
    }

    pub fn ncols(&self) -> usize {
        self.headers.len()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn encoding(&self) -> TextEncoding {
        self.info.encoding
    }

    /// The encoding of the text the records point into (UTF-16 files and
    /// some Windows-31J files are converted to UTF-8 when loaded).
    pub fn text_encoding(&self) -> TextEncoding {
        match *self.content {
            Content::Utf8(_) => TextEncoding::Utf8,
            Content::Legacy(e, _) => e,
        }
    }

    pub fn record_bytes(&self, r: usize) -> &[u8] {
        let p = self.records[r];
        &self.content.bytes()[p.start as usize..p.end as usize]
    }

    fn split_pos(&self, p: RecordPos, limit: usize, out: &mut Vec<FieldSpan>) {
        let rec = &self.content.bytes()[p.start as usize..p.end as usize];
        match &self.layout {
            Layout::Delimited { delimiter, quoted } => tokenizer::split(rec, *delimiter, *quoted, limit, out),
            Layout::Fixed(f) => fixed::split(rec, f, self.text_encoding(), limit, out),
        }
    }

    /// Splits data record `r` into at most `limit` field spans.
    pub fn split(&self, r: usize, limit: usize, out: &mut Vec<FieldSpan>) {
        self.split_pos(self.records[r], limit, out);
    }

    /// Undecoded bytes of a field, still in CSV escaped form if `span.escaped`.
    pub fn field_raw(&self, r: usize, span: FieldSpan) -> &[u8] {
        let base = self.records[r].start as usize;
        &self.content.bytes()[base + span.start as usize..base + span.end as usize]
    }

    fn value_at(&self, p: RecordPos, span: FieldSpan) -> Cow<'_, str> {
        let a = p.start as usize + span.start as usize;
        let b = p.start as usize + span.end as usize;
        match &*self.content {
            Content::Utf8(s) => {
                let v = &s[a..b];
                if span.escaped {
                    Cow::Owned(v.replace("\"\"", "\""))
                } else {
                    Cow::Borrowed(v)
                }
            }
            Content::Legacy(e, bytes) => {
                let raw = &bytes[a..b];
                if span.escaped {
                    Cow::Owned(encoding::decode(*e, &tokenizer::unescape(raw)).into_owned())
                } else {
                    encoding::decode(*e, raw)
                }
            }
        }
    }

    /// Decoded cell value of data record `r`.
    pub fn value(&self, r: usize, span: FieldSpan) -> Cow<'_, str> {
        self.value_at(self.records[r], span)
    }

    /// All cell values of data record `r`.
    pub fn values(&self, r: usize) -> Vec<String> {
        let mut spans = Vec::new();
        self.split(r, usize::MAX, &mut spans);
        spans.iter().map(|&s| self.value(r, s).into_owned()).collect()
    }

    /// Original text of data record `r` (quotes, delimiters and padding as in
    /// the file), decoded for display. Terminator excluded.
    pub fn raw_text(&self, r: usize) -> Cow<'_, str> {
        let p = self.records[r];
        let (a, b) = (p.start as usize, p.end as usize);
        match &*self.content {
            Content::Utf8(s) => Cow::Borrowed(&s[a..b]),
            Content::Legacy(e, bytes) => encoding::decode(*e, &bytes[a..b]),
        }
    }

    /// Data record containing physical line `line`, if any.
    pub fn record_at_line(&self, line: u64) -> Option<usize> {
        let i = self.records.partition_point(|p| p.line_start <= line);
        if i == 0 {
            return None;
        }
        let p = self.records[i - 1];
        (line <= p.line_end).then_some(i - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf8(src: &[u8]) -> Result<Table, LoadError> {
        Table::from_bytes("t.csv", src.to_vec(), &LoadOptions::default())
    }

    fn sjis(src: &[u8]) -> Result<Table, LoadError> {
        Table::from_bytes(
            "t.csv",
            src.to_vec(),
            &LoadOptions { encoding: TextEncoding::Windows31j, ..Default::default() },
        )
    }

    #[test]
    fn header_bom_and_positions() {
        let t = utf8(b"\xEF\xBB\xBFid,v\r\n001,\"a\"\"b\"\r\n").unwrap();
        assert_eq!(t.headers, vec!["id", "v"]);
        assert!(t.info.bom);
        assert_eq!(t.values(0), vec!["001", "a\"b"]);
        assert_eq!(t.records[0].start, 3 + 6);
        assert_eq!(t.raw_text(0), "001,\"a\"\"b\"");
        assert_eq!(t.records[0].line_start, 2);
    }

    #[test]
    fn header_only_is_zero_records() {
        let t = utf8(b"id,v\r\n").unwrap();
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn empty_file_errors() {
        assert!(matches!(utf8(b""), Err(LoadError::Empty { .. })));
        assert!(matches!(utf8(b"\xEF\xBB\xBF"), Err(LoadError::Empty { .. })));
    }

    #[test]
    fn invalid_utf8_is_reported_with_line() {
        // "id,v\n" + "a,\x82\xA0\n" : valid Windows-31J (hiragana), invalid UTF-8.
        let e = utf8(b"id,v\na,\x82\xa0\n").unwrap_err();
        match e {
            LoadError::Decode { line, byte_offset, hint, .. } => {
                assert_eq!((line, byte_offset), (2, 7));
                assert!(hint.unwrap().contains("Windows-31J"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn windows31j_values_and_raw_positions() {
        // 社員番号,氏名 / 001,髙﨑①〜
        let mut src = Vec::new();
        src.extend_from_slice(b"\x8e\xd0\x88\xf5\x94\xd4\x8d\x86,\x8e\x81\x96\xbc\r\n");
        src.extend_from_slice(b"001,\xfb\xfc\xfa\xb1\x87\x40\x81\x60\r\n");
        let t = sjis(&src).unwrap();
        assert_eq!(t.headers, vec!["社員番号", "氏名"]);
        assert_eq!(t.values(0), vec!["001", "\u{9AD9}\u{FA11}\u{2460}\u{FF5E}"]);
        assert_eq!(t.records[0].start, 15);
        assert_eq!(t.records[0].end, 15 + 12);
    }

    #[test]
    fn windows31j_rejects_bom_and_invalid_bytes() {
        assert!(matches!(sjis(b"\xEF\xBB\xBFid\n"), Err(LoadError::BomMismatch { .. })));
        // Lead byte followed by a delimiter is malformed.
        match sjis(b"id,v\na,\x81,\n").unwrap_err() {
            LoadError::Decode { line, byte_offset, .. } => assert_eq!((line, byte_offset), (2, 7)),
            other => panic!("{other:?}"),
        }
        // 0xA0 is not mapped by the WHATWG decoder (Windows maps it to U+F8F0).
        assert!(matches!(sjis(b"id\n\xa0\n"), Err(LoadError::Decode { .. })));
    }

    #[test]
    fn no_header_mode_names_columns_by_position() {
        let t = Table::from_bytes(
            "t.csv",
            b"a,b\nc,d\n".to_vec(),
            &LoadOptions { has_header: false, ..Default::default() },
        )
        .unwrap();
        assert_eq!(t.headers, vec!["列1", "列2"]);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn field_count_error_reports_data_record_number() {
        match utf8(b"id,v\na,b\nc\n").unwrap_err() {
            LoadError::FieldCount { line, record, found, expected, .. } => {
                assert_eq!((line, record, found, expected), (3, 2, 1, 2))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn record_at_line_handles_multiline_records() {
        let t = utf8(b"id,v\na,\"x\ny\"\nb,z\n").unwrap();
        assert_eq!(t.record_at_line(1), None);
        assert_eq!(t.record_at_line(2), Some(0));
        assert_eq!(t.record_at_line(3), Some(0));
        assert_eq!(t.record_at_line(4), Some(1));
        assert_eq!(t.record_at_line(5), None);
    }

    fn with(src: &[u8], opts: LoadOptions) -> Result<Table, LoadError> {
        Table::from_bytes("t.txt", src.to_vec(), &opts)
    }

    fn utf16(text: &str, big_endian: bool, bom: bool) -> Vec<u8> {
        let mut v: Vec<u8> = if bom { (if big_endian { UTF16BE_BOM } else { UTF16LE_BOM }).to_vec() } else { vec![] };
        for u in text.encode_utf16() {
            v.extend_from_slice(&if big_endian { u.to_be_bytes() } else { u.to_le_bytes() });
        }
        v
    }

    #[test]
    fn excel_unicode_text_is_utf16le_with_tabs() {
        let src = utf16("社員番号\t氏名\r\n001\t\"髙橋\"\"一\"\r\n", false, true);
        let t = with(&src, LoadOptions { encoding: TextEncoding::Utf16le, delimiter: b'\t', ..Default::default() })
            .unwrap();
        assert!(t.info.bom);
        assert_eq!(t.headers, ["社員番号", "氏名"]);
        assert_eq!(t.values(0), ["001", "髙橋\"一"]);
        assert_eq!(t.raw_text(0), "001\t\"髙橋\"\"一\"");
        assert_eq!(t.records[0].line_start, 2);
        // The SHA-256 is of the file's own bytes, not of the converted text.
        use sha2::{Digest, Sha256};
        let want: String = Sha256::digest(&src).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(t.wait_sha256().unwrap().0, want);
    }

    #[test]
    fn utf16be_without_bom_and_errors() {
        let t = with(
            &utf16("id,v\n1,a\n", true, false),
            LoadOptions { encoding: TextEncoding::Utf16be, ..Default::default() },
        )
        .unwrap();
        assert!(!t.info.bom);
        assert_eq!(t.values(0), ["1", "a"]);
        // An unpaired surrogate on line 2.
        let mut bad = utf16("id\n", false, false);
        bad.extend_from_slice(&[0x00, 0xD8, b'x', 0]);
        match with(&bad, LoadOptions { encoding: TextEncoding::Utf16le, ..Default::default() }).unwrap_err() {
            LoadError::Decode { line, byte_offset, .. } => assert_eq!((line, byte_offset), (2, 6)),
            other => panic!("{other:?}"),
        }
        // A UTF-16 file read as UTF-8 says which encoding to choose.
        let e = utf8(&utf16("id\n", false, true)).unwrap_err();
        assert!(e.to_string().contains("UTF-16 LEを指定してください"), "{e}");
    }

    #[test]
    fn windows31j_with_a_pipe_does_not_split_inside_characters() {
        // ポ is 0x83 0x7C: its second byte is `|`.
        let src = b"id|\x83\x7c\x83\x43\x83\x93\x83\x67\nA|\x83\x7c\n";
        let t = with(src, LoadOptions { encoding: TextEncoding::Windows31j, delimiter: b'|', ..Default::default() })
            .unwrap();
        assert_eq!(t.headers, ["id", "ポイント"]);
        assert_eq!(t.values(0), ["A", "ポ"]);
        assert_eq!(t.text_encoding(), TextEncoding::Utf8);
        assert_eq!(t.info.layout_label(), "区切り 縦線（|）");
    }

    #[test]
    fn unquoted_tab_separated_text() {
        let opts = LoadOptions { delimiter: b'\t', quoted: false, ..Default::default() };
        let t = with(b"id\tsize\n1\t5\" screen\n2\t\"a\"\n", opts).unwrap();
        assert_eq!(t.values(0), ["1", "5\" screen"]);
        assert_eq!(t.values(1), ["2", "\"a\""]);
        assert_eq!(t.info.layout_label(), "区切り タブ、引用符なし");
        // With quotes on, the same file stops with a hint to turn them off.
        let e = with(b"id\tsize\n1\t5\" screen\n", LoadOptions { delimiter: b'\t', ..Default::default() }).unwrap_err();
        assert!(e.to_string().contains("オフにしてください"), "{e}");
    }

    #[test]
    fn fixed_width_in_windows31j_bytes() {
        // 番号(4) 氏名(8) 所在(4), padded with spaces; the header too.
        let mut src = Vec::new();
        src.extend_from_slice(b"\x94\xd4\x8d\x86\x8e\x81\x96\xbc    \x8f\x8a\x8d\xdd\r\n"); // 番号 氏名     所在
        src.extend_from_slice(b"0001\x8eR\x93c    \x93\x8c\x8b\x9e\r\n"); // 0001 山田     東京
        src.extend_from_slice(b"0002\x8d\xb2\x93\xa1\x88\xea  \x91\xe5\x8d\xe3\r\n"); // 0002 佐藤一   大阪
        let fixed = FixedWidth { widths: vec![4, 8, 4], unit: WidthUnit::Bytes, trim: true };
        let opts = LoadOptions { encoding: TextEncoding::Windows31j, fixed: Some(fixed.clone()), ..Default::default() };
        let t = with(&src, opts).unwrap();
        assert_eq!(t.headers, ["番号", "氏名", "所在"]);
        assert_eq!(t.values(0), ["0001", "山田", "東京"]);
        assert_eq!(t.values(1), ["0002", "佐藤一", "大阪"]);
        assert_eq!(t.raw_text(0), "0001山田    東京");
        assert_eq!(t.info.delimiter, "");
        assert_eq!(t.info.layout_label(), "固定長（バイト単位の幅 4, 8, 4、前後の空白を除く）");
        // Without trimming the padding is part of the value.
        let kept = FixedWidth { trim: false, ..fixed };
        let t = with(&src, LoadOptions { encoding: TextEncoding::Windows31j, fixed: Some(kept), ..Default::default() })
            .unwrap();
        assert_eq!(t.values(0)[1], "山田    ");
    }

    #[test]
    fn fixed_width_errors_name_the_line() {
        let opts = |widths: Vec<u32>, unit| LoadOptions {
            fixed: Some(FixedWidth { widths, unit, trim: true }),
            ..Default::default()
        };
        let src = "id名前\n01山田\n02佐藤一\n".as_bytes();
        let t = with(src, opts(vec![2, 3], WidthUnit::Chars)).unwrap();
        assert_eq!(t.values(1), ["02", "佐藤一"]);
        let e = with(src, opts(vec![2, 2], WidthUnit::Chars)).unwrap_err();
        assert!(matches!(e, LoadError::FixedTooLong { line: 3, length: 5, total: 4, .. }), "{e:?}");
        assert!(e.to_string().contains("3行目の長さ（5文字）が、列の幅の合計（4文字）を超えています"), "{e}");
        let e = with(src, opts(vec![3, 9], WidthUnit::Bytes)).unwrap_err();
        assert!(matches!(e, LoadError::FixedSplitsCharacter { line: 1, column: 1, .. }), "{e:?}");
    }

    #[test]
    fn options_that_cannot_work_are_explained() {
        for opts in [
            LoadOptions { delimiter: b'"', ..Default::default() },
            LoadOptions { delimiter: b'\n', ..Default::default() },
            LoadOptions { delimiter: 0xA5, ..Default::default() },
            LoadOptions {
                fixed: Some(FixedWidth { widths: vec![], unit: WidthUnit::Bytes, trim: true }),
                ..Default::default()
            },
            LoadOptions {
                fixed: Some(FixedWidth { widths: vec![2, 0], unit: WidthUnit::Bytes, trim: true }),
                ..Default::default()
            },
            LoadOptions {
                encoding: TextEncoding::Utf16le,
                fixed: Some(FixedWidth { widths: vec![2], unit: WidthUnit::Bytes, trim: true }),
                ..Default::default()
            },
        ] {
            assert!(matches!(with(b"a\n", opts.clone()), Err(LoadError::Options { .. })), "{opts:?}");
        }
    }

    #[test]
    fn options_saved_before_quotes_and_widths_still_read() {
        let o: LoadOptions = serde_json::from_str(r#"{"encoding":"utf8","delimiter":44,"has_header":true}"#).unwrap();
        assert_eq!(o, LoadOptions::default());
    }

    #[test]
    fn euc_jp_delimited_and_fixed_width() {
        let euc = |t: &str| encoding_rs::EUC_JP.encode(t).0.into_owned();
        let opts = LoadOptions { encoding: TextEncoding::EucJp, ..Default::default() };
        let t = with(&euc("社員番号,氏名\n001,山田\n"), opts.clone()).unwrap();
        assert_eq!(t.headers, ["社員番号", "氏名"]);
        assert_eq!(t.text_encoding(), TextEncoding::EucJp);
        let t = with(&euc("id|名前\n1|ポイント\n"), LoadOptions { delimiter: b'|', ..opts.clone() }).unwrap();
        assert_eq!(t.values(0), ["1", "ポイント"]);
        // Fixed width in EUC-JP bytes: a full-width character is 2 bytes.
        let fixed = FixedWidth { widths: vec![3, 6], unit: WidthUnit::Bytes, trim: true };
        let t = with(&euc("No 名前　\n001山田　\n"), LoadOptions { fixed: Some(fixed), has_header: false, ..opts })
            .unwrap();
        assert_eq!(t.values(1), ["001", "山田"]);
        // An EUC-JP file read as UTF-8 names EUC-JP.
        let e = utf8(&euc("id\nやまだ\n")).unwrap_err();
        assert!(e.to_string().contains("EUC-JP"), "{e}");
    }

    #[test]
    fn error_positions_are_positions_in_the_file() {
        // UTF-16 LE with BOM: the `"` of `a"b` is byte 18 of the file.
        let src = utf16("id,v\n1,a\"b\n", false, true);
        match with(&src, LoadOptions { encoding: TextEncoding::Utf16le, ..Default::default() }).unwrap_err() {
            LoadError::Csv { line, byte_offset, .. } => assert_eq!((line, byte_offset), (2, 18)),
            other => panic!("{other:?}"),
        }
        // Shift_JIS with `|` (read as converted text): the `"` is byte 11.
        let src = b"id|v\n\x83\x7c\x83\x43|a\"b\n";
        let opts = LoadOptions { encoding: TextEncoding::Windows31j, delimiter: b'|', ..Default::default() };
        match with(src, opts).unwrap_err() {
            LoadError::Csv { line, byte_offset, .. } => assert_eq!((line, byte_offset), (2, 11)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn text_after_a_closing_quote_suggests_turning_quotes_off() {
        let e = with(b"id\tmemo\n1\t\"Hello\" world\n", LoadOptions { delimiter: b'\t', ..Default::default() })
            .unwrap_err();
        assert!(e.to_string().contains("「引用符（\"）を使う」をオフ"), "{e}");
    }

    #[test]
    fn quotes_play_no_part_in_fixed_width() {
        let fixed = FixedWidth { widths: vec![2, 2], unit: WidthUnit::Bytes, trim: true };
        let t = with(b"ab\"c\n", LoadOptions { quoted: false, fixed: Some(fixed), ..Default::default() }).unwrap();
        assert!(t.info.quoted, "a fixed-width file is read the same whatever the quote setting");
    }
}
