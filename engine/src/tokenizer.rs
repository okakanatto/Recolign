//! Strict CSV tokenizer working on the original file bytes.
//!
//! Pass 1 (`index`) validates the whole file and records where each logical
//! record starts and ends (byte range and physical lines). Field values are
//! not stored; `split` re-tokenizes a validated record on demand. This keeps
//! memory proportional to the file size plus a small per-record index.
//!
//! The delimiter is any ASCII character but `"`, CR and LF (`table.rs`
//! checks it). With `quoted` off, `"` is an ordinary character and no value
//! is enclosed: for tab-separated files from databases, which do not quote.
//!
//! Rules (RFC 4180 with explicit decisions where RFC 4180 is silent):
//! - Record terminators are CRLF, LF and a lone CR. Inside quotes they are
//!   part of the value and are kept as is.
//! - A field is quoted only if its first byte is `"`. Inside quotes `""` is
//!   one `"`. After the closing quote only a delimiter, a terminator or EOF
//!   may follow.
//! - `"` inside an unquoted field is an error (not taken literally).
//! - Every record must have the same number of fields as the first record.
//!   A blank line is a record with one empty field.
//! - A terminator at the very end of the file does not start another record.

use memchr::{memchr, memchr3};
use serde::Serialize;

use crate::error::CsvErrorKind;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordPos {
    /// Offset of the first byte of the record in the original file.
    pub start: u64,
    /// Offset just past the last byte of the record, terminator excluded.
    pub end: u64,
    /// 1-based physical line where the record starts.
    pub line_start: u64,
    /// 1-based physical line where the record ends. Larger than
    /// `line_start` when a quoted value contains line breaks.
    pub line_end: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LineEndings {
    pub crlf: u64,
    pub lf: u64,
    pub cr: u64,
    /// The last record has no terminator.
    pub unterminated_last: bool,
}

#[derive(Debug)]
pub struct Indexed {
    /// All records including the header record, if any.
    pub records: Vec<RecordPos>,
    pub field_count: usize,
    pub line_endings: LineEndings,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IndexError {
    Cancelled,
    /// `record_line` is the line where the record with the error starts.
    Csv {
        kind: CsvErrorKind,
        line: u64,
        byte_offset: u64,
        record_line: u64,
    },
    /// `record_index` is 0-based and counts the header record.
    FieldCount {
        record_index: u64,
        line: u64,
        found: usize,
        expected: usize,
        blank: bool,
    },
    RecordTooLarge {
        line: u64,
    },
    /// The index of the records could not grow.
    OutOfMemory,
}

const CHECK_INTERVAL: usize = 4 << 20;

/// Pass 1. `on_progress(offset)` is called every few MiB and returns `true`
/// to cancel.
pub fn index(
    bytes: &[u8],
    start: usize,
    delimiter: u8,
    quoted: bool,
    mut on_progress: impl FnMut(usize) -> bool,
) -> Result<Indexed, IndexError> {
    let n = bytes.len();
    let mut stop = [false; 256];
    stop[delimiter as usize] = true;
    stop[b'"' as usize] = quoted;
    stop[b'\r' as usize] = true;
    stop[b'\n' as usize] = true;

    // Rough capacity guess from the first record length avoids most regrowth.
    let mut records: Vec<RecordPos> = Vec::new();
    let mut le = LineEndings::default();
    let mut expected: Option<usize> = None;

    let mut i = start;
    let mut line: u64 = 1;
    let mut rec_start = start;
    let mut rec_line: u64 = 1;
    let mut fields: usize = 0;
    let mut next_check = start + CHECK_INTERVAL;

    // Ends the current record at `end` (exclusive) and consumes the terminator
    // at `end` if there is one. Returns the next position.
    macro_rules! end_record {
        ($end:expr) => {{
            let end: usize = $end;
            fields += 1;
            let blank = end == rec_start;
            if end - rec_start > u32::MAX as usize {
                return Err(IndexError::RecordTooLarge { line: rec_line });
            }
            match expected {
                None => {
                    expected = Some(fields);
                    if records.is_empty() && end > rec_start {
                        let guess = (n - start) / (end - rec_start + 2) + 16;
                        // Only a guess: when it cannot be had, grow as needed.
                        records.try_reserve(guess.min(1 << 26)).ok();
                    }
                }
                Some(e) if e != fields => {
                    return Err(IndexError::FieldCount {
                        record_index: records.len() as u64,
                        line: rec_line,
                        found: fields,
                        expected: e,
                        blank,
                    });
                }
                _ => {}
            }
            // A record can be one byte of the file and is 32 in the index:
            // a failed allocation here would end the process without a word.
            if records.len() == records.capacity() && records.try_reserve(1).is_err() {
                return Err(IndexError::OutOfMemory);
            }
            records.push(RecordPos { start: rec_start as u64, end: end as u64, line_start: rec_line, line_end: line });
            fields = 0;
            let next = if end >= n {
                le.unterminated_last = true;
                n
            } else if bytes[end] == b'\r' {
                if end + 1 < n && bytes[end + 1] == b'\n' {
                    le.crlf += 1;
                    end + 2
                } else {
                    le.cr += 1;
                    end + 1
                }
            } else {
                le.lf += 1;
                end + 1
            };
            line += 1;
            rec_start = next;
            rec_line = line;
            next
        }};
    }

    while i < n {
        if i >= next_check {
            if on_progress(i) {
                return Err(IndexError::Cancelled);
            }
            next_check = i + CHECK_INTERVAL;
        }
        // At the start of a field.
        if quoted && bytes[i] == b'"' {
            let quote_line = line;
            let quote_pos = i;
            i += 1;
            loop {
                let Some(off) = memchr3(b'"', b'\r', b'\n', &bytes[i..]) else {
                    return Err(IndexError::Csv {
                        kind: CsvErrorKind::UnclosedQuote,
                        line: quote_line,
                        byte_offset: quote_pos as u64,
                        record_line: rec_line,
                    });
                };
                let j = i + off;
                match bytes[j] {
                    b'"' => {
                        if j + 1 < n && bytes[j + 1] == b'"' {
                            i = j + 2;
                        } else {
                            i = j + 1;
                            break;
                        }
                    }
                    b'\r' => {
                        line += 1;
                        i = if j + 1 < n && bytes[j + 1] == b'\n' { j + 2 } else { j + 1 };
                    }
                    _ => {
                        line += 1;
                        i = j + 1;
                    }
                }
            }
            if i >= n {
                i = end_record!(n);
                continue;
            }
            let c = bytes[i];
            if c == delimiter {
                fields += 1;
                i += 1;
                if i == n {
                    i = end_record!(n);
                }
            } else if c == b'\r' || c == b'\n' {
                i = end_record!(i);
            } else {
                return Err(IndexError::Csv {
                    kind: CsvErrorKind::TextAfterClosingQuote,
                    line,
                    byte_offset: i as u64,
                    record_line: rec_line,
                });
            }
        } else {
            let j = match bytes[i..].iter().position(|&c| stop[c as usize]) {
                Some(off) => i + off,
                None => {
                    i = end_record!(n);
                    continue;
                }
            };
            let c = bytes[j];
            if c == delimiter {
                fields += 1;
                i = j + 1;
                if i == n {
                    i = end_record!(n);
                }
            } else if c == b'"' {
                return Err(IndexError::Csv {
                    kind: CsvErrorKind::QuoteInUnquotedField,
                    line,
                    byte_offset: j as u64,
                    record_line: rec_line,
                });
            } else {
                i = end_record!(j);
            }
        }
    }
    let _ = i;
    Ok(Indexed { records, field_count: expected.unwrap_or(0), line_endings: le })
}

/// Value range of one field relative to the start of its record. For quoted
/// fields the range excludes the surrounding quotes; `escaped` means the
/// range still contains `""` pairs that stand for one `"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FieldSpan {
    pub start: u32,
    pub end: u32,
    pub escaped: bool,
}

/// Splits a record that `index` already validated. Stops after `limit` fields.
pub fn split(rec: &[u8], delimiter: u8, quoted: bool, limit: usize, out: &mut Vec<FieldSpan>) {
    out.clear();
    let n = rec.len();
    let mut i = 0usize;
    while out.len() < limit {
        if quoted && i < n && rec[i] == b'"' {
            let s = i + 1;
            let mut j = s;
            let mut escaped = false;
            loop {
                let q = j + memchr(b'"', &rec[j..]).expect("validated record has a closing quote");
                if q + 1 < n && rec[q + 1] == b'"' {
                    escaped = true;
                    j = q + 2;
                } else {
                    out.push(FieldSpan { start: s as u32, end: q as u32, escaped });
                    i = q + 1;
                    break;
                }
            }
        } else {
            let e = memchr(delimiter, &rec[i..]).map_or(n, |o| i + o);
            out.push(FieldSpan { start: i as u32, end: e as u32, escaped: false });
            i = e;
        }
        if i >= n {
            return;
        }
        // rec[i] is the delimiter.
        i += 1;
        if i == n {
            if out.len() < limit {
                out.push(FieldSpan { start: n as u32, end: n as u32, escaped: false });
            }
            return;
        }
    }
}

/// Replaces each `""` with `"`.
pub fn unescape(raw: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        v.push(raw[i]);
        i += if raw[i] == b'"' { 2 } else { 1 };
    }
    v
}

/// 1-based physical line containing `offset` (CRLF, LF and CR each end a line).
pub fn line_of_offset(bytes: &[u8], offset: usize) -> u64 {
    let mut line = 1u64;
    let mut i = 0;
    let end = offset.min(bytes.len());
    while i < end {
        match bytes[i] {
            b'\n' => line += 1,
            b'\r' => {
                if i + 1 < end && bytes[i + 1] == b'\n' {
                    i += 1;
                }
                line += 1;
            }
            _ => {}
        }
        i += 1;
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(src: &str) -> Vec<Vec<String>> {
        let b = src.as_bytes();
        let idx = index(b, 0, b',', true, |_| false).unwrap();
        let mut spans = Vec::new();
        idx.records
            .iter()
            .map(|r| {
                let rec = &b[r.start as usize..r.end as usize];
                split(rec, b',', true, usize::MAX, &mut spans);
                spans
                    .iter()
                    .map(|s| {
                        let raw = &rec[s.start as usize..s.end as usize];
                        let v = if s.escaped { unescape(raw) } else { raw.to_vec() };
                        String::from_utf8(v).unwrap()
                    })
                    .collect()
            })
            .collect()
    }

    fn err(src: &str) -> IndexError {
        index(src.as_bytes(), 0, b',', true, |_| false).unwrap_err()
    }

    #[test]
    fn basic_and_terminators() {
        assert_eq!(values("a,b\r\nc,d\r\n"), vec![vec!["a", "b"], vec!["c", "d"]]);
        assert_eq!(values("a,b\nc,d"), vec![vec!["a", "b"], vec!["c", "d"]]);
        assert_eq!(values("a,b\rc,d\r"), vec![vec!["a", "b"], vec!["c", "d"]]);
        let idx = index(b"a,b\r\nc,d\nx,y\r1,2", 0, b',', true, |_| false).unwrap();
        assert_eq!(idx.line_endings, LineEndings { crlf: 1, lf: 1, cr: 1, unterminated_last: true });
    }

    #[test]
    fn quoted_values_keep_content() {
        assert_eq!(values("a,\"x,\r\n\"\"y\"\"\"\r\n"), vec![vec!["a", "x,\r\n\"y\""]]);
        assert_eq!(values("\"\",b\n"), vec![vec!["", "b"]]);
        assert_eq!(values("a,\"\"\n"), vec![vec!["a", ""]]);
        assert_eq!(values("a,\"x\""), vec![vec!["a", "x"]]);
    }

    #[test]
    fn empty_fields_and_trailing_delimiter() {
        assert_eq!(values("a,,c\n,,\n"), vec![vec!["a", "", "c"], vec!["", "", ""]]);
        assert_eq!(values("a,b,\nc,d,"), vec![vec!["a", "b", ""], vec!["c", "d", ""]]);
        assert_eq!(values("a,\"b\",\n"), vec![vec!["a", "b", ""]]);
    }

    #[test]
    fn positions_with_multiline_record() {
        let src = "h1,h2\r\na,\"x\r\ny\"\r\nb,z\r\n";
        let idx = index(src.as_bytes(), 0, b',', true, |_| false).unwrap();
        let r = &idx.records;
        assert_eq!((r[0].line_start, r[0].line_end), (1, 1));
        assert_eq!((r[1].line_start, r[1].line_end), (2, 3));
        assert_eq!((r[2].line_start, r[2].line_end), (4, 4));
        assert_eq!(&src[r[1].start as usize..r[1].end as usize], "a,\"x\r\ny\"");
        assert_eq!(&src[r[2].start as usize..r[2].end as usize], "b,z");
    }

    #[test]
    fn lone_cr_inside_quotes_counts_as_line() {
        let idx = index(b"a,\"x\ry\"\nb,c\n", 0, b',', true, |_| false).unwrap();
        assert_eq!(idx.records[1].line_start, 3);
    }

    #[test]
    fn strict_errors() {
        assert_eq!(
            err("a,\"x\n"),
            IndexError::Csv { kind: CsvErrorKind::UnclosedQuote, line: 1, byte_offset: 2, record_line: 1 }
        );
        assert_eq!(
            err("a,x\"q\n"),
            IndexError::Csv { kind: CsvErrorKind::QuoteInUnquotedField, line: 1, byte_offset: 3, record_line: 1 }
        );
        assert_eq!(
            err("a,\"x\" y\n"),
            IndexError::Csv { kind: CsvErrorKind::TextAfterClosingQuote, line: 1, byte_offset: 5, record_line: 1 }
        );
        assert_eq!(
            err("a,b\nc\n"),
            IndexError::FieldCount { record_index: 1, line: 2, found: 1, expected: 2, blank: false }
        );
        assert_eq!(
            err("a,b\n\nc,d\n"),
            IndexError::FieldCount { record_index: 1, line: 2, found: 1, expected: 2, blank: true }
        );
        assert_eq!(
            err("a,b\nc,d\n\n"),
            IndexError::FieldCount { record_index: 2, line: 3, found: 1, expected: 2, blank: true }
        );
        assert_eq!(
            err("a,b\nc,d,e\n"),
            IndexError::FieldCount { record_index: 1, line: 2, found: 3, expected: 2, blank: false }
        );
    }

    #[test]
    fn error_line_counts_quoted_newlines() {
        assert_eq!(
            err("h,v\na,\"1\n2\"\nb,x\"\n"),
            IndexError::Csv { kind: CsvErrorKind::QuoteInUnquotedField, line: 4, byte_offset: 15, record_line: 4 }
        );
    }

    #[test]
    fn single_column_blank_line_is_an_empty_value() {
        assert_eq!(values("id\n\nb\n"), vec![vec!["id"], vec![""], vec!["b"]]);
    }

    #[test]
    fn tab_and_semicolon() {
        let b = b"a\t\"x\ty\"\r\n";
        let idx = index(b, 0, b'\t', true, |_| false).unwrap();
        let mut spans = Vec::new();
        split(&b[..idx.records[0].end as usize], b'\t', true, usize::MAX, &mut spans);
        assert_eq!(spans.len(), 2);
        assert_eq!(&b[spans[1].start as usize..spans[1].end as usize], b"x\ty");
        assert!(index(b"a;b\nc;d\n", 0, b';', true, |_| false).is_ok());
    }

    #[test]
    fn without_quotes_a_quote_is_an_ordinary_character() {
        let b = b"id\tv\n1\t5\" screen\n";
        assert!(matches!(index(b, 0, b'\t', true, |_| false), Err(IndexError::Csv { .. })));
        let idx = index(b, 0, b'\t', false, |_| false).unwrap();
        let mut spans = Vec::new();
        let row = |r: usize, spans: &mut Vec<FieldSpan>| {
            let p = idx.records[r];
            let rec = &b[p.start as usize..p.end as usize];
            split(rec, b'\t', false, usize::MAX, spans);
            spans
                .iter()
                .map(|s| String::from_utf8(rec[s.start as usize..s.end as usize].to_vec()).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(row(1, &mut spans), ["1", "5\" screen"]);
        // The tab inside the quotes separates: there are three fields here.
        assert!(matches!(
            index(b"a\tb\n\"x\ty\"\tz\n", 0, b'\t', false, |_| false),
            Err(IndexError::FieldCount { found: 3, expected: 2, .. })
        ));
    }

    #[test]
    fn any_ascii_delimiter() {
        let b = b"id|v\n1|a,b\n";
        let idx = index(b, 0, b'|', true, |_| false).unwrap();
        assert_eq!(idx.field_count, 2);
    }

    #[test]
    fn split_limit() {
        let mut spans = Vec::new();
        split(b"a,b,c", b',', true, 2, &mut spans);
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn line_of_offset_counts_all_terminators() {
        assert_eq!(line_of_offset(b"a\r\nb\nc\rd", 7), 4);
        assert_eq!(line_of_offset(b"a\r\nb", 3), 2);
    }
}
