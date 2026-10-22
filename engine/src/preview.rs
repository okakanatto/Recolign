//! A quick look at the start of a file before comparing: suggested encoding,
//! delimiter and quoting, the header and the first records. Suggestions are
//! shown to the user and only used after they confirm them; the whole file
//! is validated when the comparison starts.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::encoding::{first_invalid_in, likeliest, TextEncoding, UTF16BE_BOM, UTF16LE_BOM, UTF8_BOM};
use crate::error::CsvErrorKind;
use crate::error::LoadError;
use crate::fixed::{self, FixedWidth};
use crate::table::{LoadOptions, Table};

/// Bytes read for a preview.
pub const PREVIEW_BYTES: usize = 1 << 20;
const PREVIEW_CHARS: usize = 200;

#[derive(Clone, Debug, Serialize)]
pub struct Suggestion {
    /// `None` when the start of the file is valid in no supported encoding.
    pub encoding: Option<TextEncoding>,
    pub delimiter: String,
    /// False when the file reads only with `"` as an ordinary character.
    pub quoted: bool,
    /// False when the first line looks like a record rather than column names.
    pub has_header: bool,
}

/// Why a column cannot be the key on its own, as far as the records read
/// show (a partial read can show no problem where the whole file has one).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeyIssue {
    /// The same value on two records, by the lines they start on.
    Duplicate { value: String, lines: [u64; 2] },
    /// A record without a value, by the line it starts on.
    Empty { line: u64 },
}

#[derive(Clone, Debug, Serialize)]
pub struct Preview {
    pub path: PathBuf,
    pub name: String,
    pub bytes: u64,
    pub modified_unix_ms: Option<i64>,
    pub suggested: Suggestion,
    /// The options this preview was made with.
    pub options: LoadOptions,
    pub bom: bool,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Only the start of the file was read.
    pub partial: bool,
    /// Why the start of the file could not be read with `options`.
    pub error: Option<String>,
    /// The first lines as they are in the file, for setting fixed widths.
    pub lines: Vec<String>,
    /// With fixed widths: the same lines cut into columns, spaces kept, to
    /// show where the widths fall.
    pub pieces: Vec<Vec<String>>,
    /// For each column (as `headers`), why it cannot be the key on its own;
    /// `None` when its values are all filled and all different.
    pub key_issues: Vec<Option<KeyIssue>>,
}

const RAW_LINES: usize = 8;

/// UTF-16 without a BOM: mostly ASCII text has a zero in every other byte.
fn utf16_without_bom(bytes: &[u8]) -> Option<TextEncoding> {
    let head = &bytes[..bytes.len().min(4096) & !1];
    if head.len() < 4 {
        return None;
    }
    // Zeros on the other side too, but far fewer, are characters such as 一
    // (U+4E00) and the full-width space (U+3000).
    let zeros = |parity: usize| head.iter().skip(parity).step_by(2).filter(|&&b| b == 0).count();
    let (even, odd, half) = (zeros(0), zeros(1), head.len() / 2);
    if odd * 10 >= half * 3 && even * 4 <= odd {
        Some(TextEncoding::Utf16le)
    } else if even * 10 >= half * 3 && odd * 4 <= even {
        Some(TextEncoding::Utf16be)
    } else {
        None
    }
}

/// Encoding that the bytes are valid in. `complete` is false when `bytes` is
/// only the start of a file, so a character cut at the end is not an error.
pub fn guess_encoding(bytes: &[u8], complete: bool) -> Option<TextEncoding> {
    if bytes.starts_with(UTF8_BOM) {
        return Some(TextEncoding::Utf8);
    }
    if bytes.starts_with(UTF16LE_BOM) {
        return Some(TextEncoding::Utf16le);
    }
    if bytes.starts_with(UTF16BE_BOM) {
        return Some(TextEncoding::Utf16be);
    }
    if let Some(e) = utf16_without_bom(bytes) {
        return Some(e);
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => return Some(TextEncoding::Utf8),
        Err(e) if !complete && e.error_len().is_none() => return Some(TextEncoding::Utf8),
        Err(_) => {}
    }
    // Japanese text in EUC-JP is often also valid Windows-31J (as half-width
    // katakana), and some Windows-31J text is valid EUC-JP: when both read,
    // take the one that reads as more ordinary Japanese.
    let valid = |e: TextEncoding| match first_invalid_in(e, bytes, |_| false) {
        Ok(None) => true,
        Ok(Some(off)) => !complete && off + 2 >= bytes.len(),
        Err(_) => false,
    };
    let candidates: Vec<TextEncoding> =
        [TextEncoding::Windows31j, TextEncoding::EucJp].into_iter().filter(|&e| valid(e)).collect();
    likeliest(&candidates, bytes)
}

/// The start of the file as text, for guessing (characters that do not
/// decode become U+FFFD; nothing here is shown as a value).
fn text_for_guessing(bytes: &[u8], encoding: TextEncoding) -> String {
    let body = bytes.strip_prefix(encoding.bom()).unwrap_or(bytes);
    encoding.codec().decode_without_bom_handling(body).0.into_owned()
}

const DELIMITER_CANDIDATES: &[u8] = b",\t;|";

/// The most frequent of `,`, TAB, `;` and `|` outside quotes on the first
/// line of `text` (`,` when none occurs).
pub fn guess_delimiter(text: &str) -> u8 {
    let mut counts = [0usize; DELIMITER_CANDIDATES.len()];
    let mut quoted = false;
    for b in text.bytes() {
        match b {
            b'"' => quoted = !quoted,
            b'\r' | b'\n' if !quoted => break,
            _ if !quoted => {
                if let Some(i) = DELIMITER_CANDIDATES.iter().position(|&d| d == b) {
                    counts[i] += 1;
                }
            }
            _ => {}
        }
    }
    let best = (0..counts.len()).max_by_key(|&i| (counts[i], std::cmp::Reverse(i))).unwrap_or(0);
    if counts[best] == 0 {
        b','
    } else {
        DELIMITER_CANDIDATES[best]
    }
}

/// The first `max` lines of `bytes` (CRLF, LF and a lone CR end a line; a
/// line break at the very end does not start another line).
fn first_lines(bytes: &[u8], max: usize) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() && out.len() < max {
        let end = memchr::memchr2(b'\r', b'\n', &bytes[i..]).map_or(bytes.len(), |o| i + o);
        out.push(&bytes[i..end]);
        i = if bytes[end..].starts_with(b"\r\n") { end + 2 } else { end + 1 };
    }
    out
}

/// Byte offset where physical line `line` (1-based) starts in a file in
/// `encoding` (CRLF, LF and a lone CR end a line).
fn line_start(bytes: &[u8], line: u64, encoding: TextEncoding) -> usize {
    if encoding.is_utf16() {
        // Two bytes a unit; a line ends with the unit CR or LF.
        let unit = |i: usize| -> Option<u16> {
            let pair = [*bytes.get(i)?, *bytes.get(i + 1)?];
            Some(if encoding == TextEncoding::Utf16le { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) })
        };
        let start = encoding.bom().len() * usize::from(bytes.starts_with(encoding.bom()));
        let (mut i, mut lines) = (start, 1);
        while lines < line {
            match unit(i) {
                None => return bytes.len(),
                Some(0x0D) if unit(i + 2) == Some(0x0A) => (i, lines) = (i + 4, lines + 1),
                Some(0x0A | 0x0D) => (i, lines) = (i + 2, lines + 1),
                Some(_) => i += 2,
            }
        }
        return i;
    }
    let mut i = 0;
    for _ in 1..line {
        match memchr::memchr2(b'\r', b'\n', &bytes[i..]) {
            Some(o) => i += o + if bytes[i + o..].starts_with(b"\r\n") { 2 } else { 1 },
            None => return bytes.len(),
        }
    }
    i
}

/// A quoted value still open at the end of a partial read and longer than
/// this is taken for a stray quote, not for a value with line breaks.
const LONGEST_QUOTED_VALUE: usize = 64 << 10;

/// Cuts a partial read after its last line break, so that no record is cut
/// in half (a quoted value with line breaks may still be).
fn cut_after_last_line(prefix: &mut Vec<u8>, encoding: TextEncoding) {
    let end = match encoding {
        TextEncoding::Utf16le | TextEncoding::Utf16be => {
            let le = encoding == TextEncoding::Utf16le;
            (0..prefix.len() / 2)
                .rev()
                .find(|&i| {
                    let unit = [prefix[2 * i], prefix[2 * i + 1]];
                    let c = if le { u16::from_le_bytes(unit) } else { u16::from_be_bytes(unit) };
                    c == 0x0A || c == 0x0D
                })
                .map(|i| 2 * i + 2)
        }
        _ => prefix.iter().rposition(|&b| b == b'\n' || b == b'\r').map(|p| p + 1),
    };
    if let Some(end) = end {
        prefix.truncate(end);
    }
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(PREVIEW_CHARS) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

/// The look of a value, for telling column names from records.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Number,
    Date,
    Text,
}

fn shape(v: &str) -> Option<Shape> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let unsigned = v.strip_prefix(['-', '+']).unwrap_or(v);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, "0"));
    if unsigned.starts_with(|c: char| c.is_ascii_digit()) && whole.split(',').all(digits) && digits(fraction) {
        return Some(Shape::Number);
    }
    let parts: Vec<&str> = v.split(['-', '/']).collect();
    if parts.len() == 3 && parts[0].len() == 4 && parts.iter().all(|p| digits(p) && p.len() <= 4) {
        return Some(Shape::Date);
    }
    Some(Shape::Text)
}

/// Whether the first line reads as column names rather than as a record.
/// A column of numbers or dates headed by text says names; a first line
/// with a number or date where the values have one, or with a value that
/// comes again among the values, says a record. Without any sign, names.
fn first_line_is_names(t: &Table) -> bool {
    let rows: Vec<Vec<String>> = (0..t.len().min(1000)).map(|r| t.values(r)).collect();
    if rows.is_empty() {
        return true;
    }
    let (mut names, mut record) = (0, 0);
    for (c, head) in t.headers.iter().enumerate() {
        let values: Vec<&str> =
            rows.iter().filter_map(|r| r.get(c)).map(|v| v.as_str()).filter(|v| !v.trim().is_empty()).collect();
        if values.is_empty() {
            continue;
        }
        let count = |s: Shape| values.iter().filter(|v| shape(v) == Some(s)).count();
        let typed = [Shape::Number, Shape::Date].into_iter().find(|&s| count(s) * 10 >= values.len() * 9);
        match (typed, shape(head)) {
            (Some(s), Some(h)) if h == s => record += 1,
            (Some(_), Some(Shape::Text)) => names += 1,
            _ => {}
        }
        if !head.trim().is_empty() && values.contains(&head.as_str()) {
            record += 1;
        }
    }
    !(record > 0 && names == 0)
}

/// For each column, a sign in the records read that it cannot be the key on
/// its own: no value (the first record without one), else a value seen twice
/// (the first). No value comes first, because it rules the column out of a
/// key of several columns too.
fn key_issues(t: &Table) -> Vec<Option<KeyIssue>> {
    let n = t.headers.len();
    let mut issues: Vec<Option<KeyIssue>> = vec![None; n];
    let mut seen: Vec<HashMap<String, u64>> = vec![HashMap::new(); n];
    for r in 0..t.len() {
        let line = t.records[r].line_start;
        for (c, v) in t.values(r).into_iter().enumerate().take(n) {
            match issues[c] {
                Some(KeyIssue::Empty { .. }) => {}
                _ if v.is_empty() => issues[c] = Some(KeyIssue::Empty { line }),
                Some(KeyIssue::Duplicate { .. }) => {}
                None => {
                    if let Some(&first) = seen[c].get(&v) {
                        issues[c] = Some(KeyIssue::Duplicate { value: cut(&v), lines: [first, line] });
                        seen[c] = HashMap::new();
                    } else {
                        seen[c].insert(v, line);
                    }
                }
            }
        }
    }
    issues
}

pub fn preview(path: &Path, options: Option<LoadOptions>, max_rows: usize) -> Result<Preview, LoadError> {
    let io = |source| LoadError::Io { path: path.to_path_buf(), source };
    let meta = std::fs::metadata(path).map_err(io)?;
    if !meta.is_file() {
        return Err(LoadError::NotAFile { path: path.to_path_buf() });
    }
    let mut prefix = Vec::with_capacity(PREVIEW_BYTES.min(meta.len() as usize));
    File::open(path).map_err(io)?.take(PREVIEW_BYTES as u64 + 1).read_to_end(&mut prefix).map_err(io)?;
    let complete = prefix.len() <= PREVIEW_BYTES;
    prefix.truncate(PREVIEW_BYTES);
    let suggested_encoding = guess_encoding(&prefix, complete);
    let encoding = options.as_ref().map(|o| o.encoding).or(suggested_encoding).unwrap_or(TextEncoding::Utf8);
    let suggested_delimiter = guess_delimiter(&text_for_guessing(&prefix, encoding));
    if !complete {
        cut_after_last_line(&mut prefix, encoding);
    }
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let chosen = options.is_some();
    let mut options = options.unwrap_or(LoadOptions {
        encoding,
        delimiter: suggested_delimiter,
        has_header: true,
        ..LoadOptions::default()
    });
    let mut table = Table::from_bytes(&name, prefix.clone(), &options);
    // The end of a partial read may fall inside a quoted value with line
    // breaks: read up to the line where that record starts instead. Once,
    // at the start of the record: cutting back one value at a time read the
    // whole prefix again for each value, minutes for a record of many
    // values with line breaks.
    let mut stray_quote = false;
    if let Err(LoadError::Csv { kind: CsvErrorKind::UnclosedQuote, line, record_line, .. }) = table {
        stray_quote = prefix.len() - line_start(&prefix, line, options.encoding) > LONGEST_QUOTED_VALUE;
        if !complete && record_line > 1 && !stray_quote {
            prefix.truncate(line_start(&prefix, record_line, options.encoding));
            table = Table::from_bytes(&name, prefix.clone(), &options);
        }
    }
    // A quote error in a file nobody set options for yet: if it reads with
    // `"` as an ordinary character, suggest that. (Not for a value cut off
    // by a partial read: that is no sign of a file without quotes.)
    let mut suggested_quoted = true;
    let mut suggested_header = true;
    let cut_off =
        !complete && !stray_quote && matches!(table, Err(LoadError::Csv { kind: CsvErrorKind::UnclosedQuote, .. }));
    if !chosen && !cut_off && matches!(table, Err(LoadError::Csv { .. })) {
        let unquoted = LoadOptions { quoted: false, ..options.clone() };
        if let Ok(t) = Table::from_bytes(&name, prefix.clone(), &unquoted) {
            (options, table, suggested_quoted) = (unquoted, Ok(t), false);
        }
    }
    // Column names nobody set: a first line that reads as a record is not taken for them.
    if !chosen && options.has_header && matches!(&table, Ok(t) if !first_line_is_names(t)) {
        let headless = LoadOptions { has_header: false, ..options.clone() };
        if let Ok(t) = Table::from_bytes(&name, prefix.clone(), &headless) {
            (options, table, suggested_header) = (headless, Ok(t), false);
        }
    }
    let mut out = Preview {
        path: path.to_path_buf(),
        name: name.clone(),
        bytes: meta.len(),
        modified_unix_ms: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64),
        suggested: Suggestion {
            encoding: suggested_encoding,
            delimiter: (suggested_delimiter as char).to_string(),
            quoted: suggested_quoted,
            has_header: suggested_header,
        },
        options: options.clone(),
        bom: !options.encoding.bom().is_empty() && prefix.starts_with(options.encoding.bom()),
        headers: Vec::new(),
        rows: Vec::new(),
        partial: !complete,
        error: None,
        lines: Vec::new(),
        pieces: Vec::new(),
        key_issues: Vec::new(),
    };
    // The first lines as text; for UTF-16, cut as UTF-8 text by characters
    // (the only unit it allows).
    let enc = options.encoding;
    let (text, text_enc) = if enc.is_utf16() {
        (text_for_guessing(&prefix, enc).into_bytes(), TextEncoding::Utf8)
    } else {
        (prefix.strip_prefix(enc.bom()).unwrap_or(&prefix).to_vec(), enc)
    };
    let decode = |b: &[u8]| cut(&text_enc.codec().decode_without_bom_handling(b).0);
    let lines = first_lines(&text, RAW_LINES);
    out.lines = lines.iter().map(|l| decode(l)).collect();
    if let Some(f) = &options.fixed {
        // Cut by the widths without checking them, so that wrong widths show
        // where they fall; what is left after the last column is one more piece.
        let layout = FixedWidth { trim: false, ..f.clone() };
        let mut spans = Vec::new();
        out.pieces = lines
            .iter()
            .map(|rec| {
                fixed::split(rec, &layout, text_enc, usize::MAX, &mut spans);
                let end = spans.last().map_or(0, |s| s.end as usize);
                let mut v: Vec<String> = spans.iter().map(|s| decode(&rec[s.start as usize..s.end as usize])).collect();
                if end < rec.len() {
                    v.push(decode(&rec[end..]));
                }
                v
            })
            .collect();
    }
    match table {
        Ok(t) => {
            // Names are not cut: they are sent back as key and column names.
            out.headers = t.headers.clone();
            out.rows = (0..t.len().min(max_rows)).map(|r| t.values(r).iter().map(|v| cut(v)).collect()).collect();
            out.key_issues = key_issues(&t);
        }
        Err(e) => out.error = Some(e.to_string()),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_guess() {
        assert_eq!(guess_encoding("id,名前\n".as_bytes(), true), Some(TextEncoding::Utf8));
        assert_eq!(guess_encoding(b"id,\x96\xbc\x91\x4f\n", true), Some(TextEncoding::Windows31j));
        // A UTF-8 character cut at the end of a partial read.
        let cut_utf8 = &"あ".as_bytes()[..2];
        assert_eq!(guess_encoding(cut_utf8, false), Some(TextEncoding::Utf8));
        assert_eq!(guess_encoding(b"\x80\xff\x00", true), None);
    }

    #[test]
    fn euc_jp_and_windows31j_are_told_apart() {
        let euc = encoding_rs::EUC_JP.encode("社員番号,氏名\n001,山田太郎\n002,佐藤花子\n").0.into_owned();
        let sjis = encoding_rs::SHIFT_JIS.encode("社員番号,氏名\n001,山田太郎\n002,佐藤花子\n").0.into_owned();
        assert_eq!(guess_encoding(&euc, true), Some(TextEncoding::EucJp));
        assert_eq!(guess_encoding(&sjis, true), Some(TextEncoding::Windows31j));
    }

    #[test]
    fn utf16_guess() {
        let le: Vec<u8> = "id,名前\n".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let be: Vec<u8> = "id,名前\n".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        assert_eq!(guess_encoding(&le, true), Some(TextEncoding::Utf16le));
        assert_eq!(guess_encoding(&be, true), Some(TextEncoding::Utf16be));
        assert_eq!(guess_encoding(b"\xff\xfei\x00d\x00", true), Some(TextEncoding::Utf16le));
        assert_eq!(text_for_guessing(&le, TextEncoding::Utf16le), "id,名前\n");
    }

    #[test]
    fn delimiter_guess() {
        assert_eq!(guess_delimiter("a,b,c\n1\t2\t3\t4\n"), b',');
        assert_eq!(guess_delimiter("a\tb\t\"x,y,z\"\n"), b'\t');
        assert_eq!(guess_delimiter("a;b;c\n"), b';');
        assert_eq!(guess_delimiter("a|b|c\n"), b'|');
        assert_eq!(guess_delimiter("single\n"), b',');
    }

    #[test]
    fn lines_end_at_cr_lf_and_crlf() {
        assert_eq!(first_lines(b"a\rb\r\nc\nd", 8), [&b"a"[..], b"b", b"c", b"d"]);
        assert_eq!(first_lines(b"a\n", 8), [&b"a"[..]]);
        assert_eq!(line_start(b"a\r\nb\rc\n", 3, TextEncoding::Utf8), 5);
        let le: Vec<u8> = "\u{feff}a\r\nb\rc\n".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(line_start(&le, 3, TextEncoding::Utf16le), 2 + 10);
    }

    #[test]
    fn half_width_katakana_files_are_told_apart() {
        let text = "id,ｶﾅ\n1,ﾔﾏﾀﾞ\n2,ｽｽﾞｷ\n";
        let euc = encoding_rs::EUC_JP.encode(text).0.into_owned();
        let sjis = encoding_rs::SHIFT_JIS.encode(text).0.into_owned();
        assert_eq!(guess_encoding(&euc, true), Some(TextEncoding::EucJp));
        assert_eq!(guess_encoding(&sjis, true), Some(TextEncoding::Windows31j));
    }

    #[test]
    fn utf16_without_bom_with_zero_bytes_in_characters() {
        // 一 is U+4E00 and the full-width space U+3000: a zero byte each.
        let le: Vec<u8> = "id,名前\n1,一郎\n2,山　太\n".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let be: Vec<u8> = "id,名前\n1,一郎\n2,山　太\n".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        assert_eq!(guess_encoding(&le, true), Some(TextEncoding::Utf16le));
        assert_eq!(guess_encoding(&be, true), Some(TextEncoding::Utf16be));
    }

    #[test]
    fn a_partial_utf16_read_is_cut_after_a_whole_line() {
        let mut le: Vec<u8> = "a\r\nb\nc".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        cut_after_last_line(&mut le, TextEncoding::Utf16le);
        assert_eq!(le.len(), 10);
    }

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cdr-preview-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn widths_too_short_still_show_where_they_cut() {
        let p = temp_file("short.dat", b"0001abcdef\n0002ghijkl\n");
        let fixed = FixedWidth { widths: vec![4, 3], unit: crate::fixed::WidthUnit::Bytes, trim: true };
        let pv = preview(&p, Some(LoadOptions { fixed: Some(fixed), ..Default::default() }), 5).unwrap();
        assert!(pv.error.is_some(), "the line is longer than the widths");
        assert_eq!(pv.pieces[0], ["0001", "abc", "def"], "the rest of the line is one more piece");
    }

    #[test]
    fn a_partial_read_inside_a_quoted_value_is_not_taken_for_a_file_without_quotes() {
        let mut src = b"memo\n".to_vec();
        while src.len() <= PREVIEW_BYTES + 100 {
            src.extend_from_slice(b"\"line one\nline two, more\"\n");
        }
        let p = temp_file("multiline.csv", &src);
        let pv = preview(&p, None, 5).unwrap();
        assert!(pv.partial && pv.suggested.quoted && pv.options.quoted, "{:?}", pv.suggested);
        assert!(pv.error.is_none(), "{:?}", pv.error);
        assert_eq!(pv.rows[0], ["line one\nline two, more"]);
    }

    #[test]
    fn a_partial_read_inside_values_with_line_breaks_in_utf16_and_in_two_fields() {
        // Excel's Unicode text: UTF-16 LE, tabs, values with line breaks in quotes.
        let mut text = String::from("id\tmemo\r\n");
        let mut n = 0;
        while text.len() * 2 <= PREVIEW_BYTES + 100 {
            text += &format!("{n}\t\"one\r\ntwo\r\nthree\"\r\n");
            n += 1;
        }
        let mut src = vec![0xFF, 0xFE];
        src.extend(text.encode_utf16().flat_map(|u| u.to_le_bytes()));
        let pv = preview(&temp_file("u16.txt", &src), None, 5).unwrap();
        assert!(pv.error.is_none() && pv.options.quoted, "{:?}", pv.error);
        assert_eq!(pv.rows[0], ["0", "one\r\ntwo\r\nthree"]);
        // Two fields with line breaks in one record.
        let mut src = b"id,a,b\n".to_vec();
        while src.len() <= PREVIEW_BYTES + 100 {
            src.extend_from_slice(b"1,\"a\nb\",\"c\nd\ne\"\n");
        }
        let pv = preview(&temp_file("two.csv", &src), None, 5).unwrap();
        assert!(pv.error.is_none(), "{:?}", pv.error);
    }

    #[test]
    fn a_partial_read_inside_a_record_of_many_values_with_line_breaks_is_cut_once() {
        // The read ends inside one record of short values with line breaks:
        // cutting back one value at a time would read the prefix again
        // about 200,000 times.
        let mut src = b"id,memo\n1,a\n2,b\n3,".to_vec();
        while src.len() <= PREVIEW_BYTES + 100 {
            src.extend_from_slice(b"\"x\ny\",");
        }
        let t0 = std::time::Instant::now();
        let pv = preview(&temp_file("many.csv", &src), None, 5).unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_secs(30), "{:?}", t0.elapsed());
        assert!(pv.error.is_none() && pv.options.quoted, "{:?}", pv.error);
        assert_eq!(pv.rows, [["1", "a"], ["2", "b"]]);
    }

    #[test]
    fn a_stray_quote_in_a_large_file_is_not_a_value_with_line_breaks() {
        let mut src = b"id\tname\n".to_vec();
        for i in 0..200_000 {
            src.extend_from_slice(format!("{i}\tname{i}\n").as_bytes());
            if i == 100 {
                src.extend_from_slice(b"x\t\"special\n");
            }
        }
        let pv = preview(&temp_file("stray.tsv", &src), None, 5).unwrap();
        assert!(!pv.suggested.quoted, "a quote that stays open for long is taken as an ordinary character");
    }

    #[test]
    fn a_first_line_that_reads_as_a_record_is_not_taken_for_column_names() {
        let headless = |name: &str, text: &str| {
            let pv = preview(&temp_file(name, text.as_bytes()), None, 5).unwrap();
            (pv.suggested.has_header, pv.options.has_header, pv.headers)
        };
        // Numbers and dates where the values have them.
        let (suggested, used, headers) = headless("h1.csv", "001,山田,2026-04-01,300000\n002,佐藤,2026-04-02,310000\n");
        assert!(!suggested && !used);
        assert_eq!(headers, ["列1", "列2", "列3", "列4"]);
        // A value of the first line that comes again.
        assert!(!headless("h2.csv", "仕入,A社\n販売,B社\n仕入,C社\n").0);
        // Names over numbers, and text only (no sign either way): names.
        assert!(headless("h3.csv", "番号,金額\n1,100\n2,200\n").0);
        assert!(headless("h4.csv", "氏名,部署\n山田,営業\n佐藤,総務\n").0);
        assert!(headless("h5.csv", "社員番号,給与\n001,300000\n").0);
        // Options set by hand are kept.
        let pv = preview(&temp_file("h6.csv", b"1,2\n3,4\n"), Some(LoadOptions::default()), 5).unwrap();
        assert!(pv.options.has_header && pv.suggested.has_header);
    }

    #[test]
    fn columns_that_cannot_be_the_key_are_told_with_their_lines() {
        let src = "id,部署,備考,名前\n1,営業,,a\n2,営業,x,b\n3,総務,y,b\n4,総務,z,\n";
        let pv = preview(&temp_file("keys.csv", src.as_bytes()), None, 5).unwrap();
        assert_eq!(
            pv.key_issues,
            [
                None,
                Some(KeyIssue::Duplicate { value: "営業".into(), lines: [2, 3] }),
                Some(KeyIssue::Empty { line: 2 }),
                // A value with no value after it: the empty one is told.
                Some(KeyIssue::Empty { line: 5 }),
            ]
        );
    }
}
