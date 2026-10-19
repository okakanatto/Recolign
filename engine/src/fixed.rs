//! Fixed-width text: every line is one record, and each column takes a set
//! number of bytes or characters, left to right. There are no delimiters
//! and no quotes.
//!
//! - Widths in bytes count the bytes of the file's encoding (in Windows-31J
//!   and EUC-JP a full-width character is 2 bytes, as in most fixed-width
//!   files made in Japan).
//! - `enc` below is the encoding of the text being cut: UTF-8 (the file's,
//!   or a UTF-16 file converted), Windows-31J or EUC-JP. A column that would end inside a character is an error, with
//!   its line: the widths do not fit the file.
//! - A line shorter than the widths leaves the last columns empty (tools
//!   often drop trailing spaces). A line longer than the widths is an error,
//!   so that nothing at its end is silently left out.
//! - With `trim`, spaces around each value (half-width and the full-width
//!   space U+3000) are padding and not part of the value. The original line
//!   is still shown as it is.
//!
//! Lines end with CRLF, LF or a lone CR, as in `tokenizer.rs`.

use memchr::memchr2;
use serde::{Deserialize, Serialize};

use crate::encoding::TextEncoding;
use crate::tokenizer::{FieldSpan, Indexed, LineEndings, RecordPos};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidthUnit {
    Bytes,
    Chars,
}

impl WidthUnit {
    pub fn label(self) -> &'static str {
        match self {
            WidthUnit::Bytes => "バイト",
            WidthUnit::Chars => "文字",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FixedWidth {
    /// Width of each column, left to right.
    pub widths: Vec<u32>,
    pub unit: WidthUnit,
    /// Spaces around each value are padding.
    #[serde(default = "yes")]
    pub trim: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, PartialEq, Eq)]
pub enum FixedError {
    Cancelled,
    /// `length` and `total` are in the layout's unit.
    TooLong {
        line: u64,
        length: u64,
        total: u64,
    },
    /// Column `column` (1-based) would end inside a character.
    SplitsCharacter {
        line: u64,
        column: usize,
    },
    /// A line longer than the positions of the index can hold.
    LineTooLarge {
        line: u64,
    },
}

/// Length of the character starting at `rec[i]` in a validated file.
#[inline]
fn char_len(rec: &[u8], i: usize, enc: TextEncoding) -> usize {
    let c = rec[i];
    let len = match enc {
        TextEncoding::Windows31j => match c {
            0x81..=0x9F | 0xE0..=0xFC => 2,
            _ => 1,
        },
        TextEncoding::EucJp => match c {
            0x8F => 3,
            0x8E | 0xA1..=0xFE => 2,
            _ => 1,
        },
        _ => match c {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            _ => 4,
        },
    };
    len.min(rec.len() - i)
}

/// A half-width space or the full-width space U+3000.
fn is_space(ch: &[u8], enc: TextEncoding) -> bool {
    let full: &[u8] = match enc {
        TextEncoding::Windows31j => b"\x81\x40",
        TextEncoding::EucJp => b"\xA1\xA1",
        _ => "\u{3000}".as_bytes(),
    };
    ch == b" " || ch == full
}

/// `rec[a..b]` without the spaces at both ends.
fn trimmed(rec: &[u8], a: usize, b: usize, enc: TextEncoding) -> (usize, usize) {
    let (mut first, mut last) = (None, a);
    let mut i = a;
    while i < b {
        let l = char_len(rec, i, enc).min(b - i);
        if !is_space(&rec[i..i + l], enc) {
            first.get_or_insert(i);
            last = i + l;
        }
        i += l;
    }
    first.map_or((a, a), |f| (f, last))
}

/// End of the column that starts at `pos` and is `width` units wide.
#[inline]
fn column_end(rec: &[u8], pos: usize, width: u32, unit: WidthUnit, enc: TextEncoding) -> usize {
    match unit {
        WidthUnit::Bytes => (pos + width as usize).min(rec.len()),
        WidthUnit::Chars => {
            let mut e = pos;
            for _ in 0..width {
                if e >= rec.len() {
                    break;
                }
                e += char_len(rec, e, enc);
            }
            e
        }
    }
}

/// Splits one line that `index` already checked. Stops after `limit` fields.
pub fn split(rec: &[u8], layout: &FixedWidth, enc: TextEncoding, limit: usize, out: &mut Vec<FieldSpan>) {
    out.clear();
    let mut pos = 0;
    for &w in layout.widths.iter().take(limit) {
        let end = column_end(rec, pos, w, layout.unit, enc);
        let (s, e) = if layout.trim { trimmed(rec, pos, end, enc) } else { (pos, end) };
        out.push(FieldSpan { start: s as u32, end: e as u32, escaped: false });
        pos = end;
    }
}

/// Checks one line against the layout.
fn check_line(rec: &[u8], layout: &FixedWidth, enc: TextEncoding, line: u64) -> Result<(), FixedError> {
    let total: u64 = layout.widths.iter().map(|&w| w as u64).sum();
    match layout.unit {
        WidthUnit::Chars => {
            let length = if enc == TextEncoding::Utf8 {
                // Every byte but a continuation byte starts a character.
                rec.iter().filter(|&&b| (b as i8) >= -0x40).count() as u64
            } else {
                let (mut i, mut n) = (0, 0u64);
                while i < rec.len() {
                    i += char_len(rec, i, enc);
                    n += 1;
                }
                n
            };
            if length > total {
                return Err(FixedError::TooLong { line, length, total });
            }
        }
        WidthUnit::Bytes => {
            if rec.len() as u64 > total {
                return Err(FixedError::TooLong { line, length: rec.len() as u64, total });
            }
            // Every column boundary inside the line must be a character start.
            let mut boundary = 0usize;
            let mut i = 0usize;
            for (k, &w) in layout.widths.iter().enumerate() {
                boundary += w as usize;
                if boundary >= rec.len() {
                    break;
                }
                if enc == TextEncoding::Utf8 {
                    if (0x80..0xC0).contains(&rec[boundary]) {
                        return Err(FixedError::SplitsCharacter { line, column: k + 1 });
                    }
                } else {
                    while i < boundary {
                        i += char_len(rec, i, enc);
                    }
                    if i != boundary {
                        return Err(FixedError::SplitsCharacter { line, column: k + 1 });
                    }
                }
            }
        }
    }
    Ok(())
}

const CHECK_INTERVAL: usize = 4 << 20;

/// Finds the lines from `start` and checks each against the layout.
/// `on_progress(offset)` is called every few MiB and returns `true` to cancel.
pub fn index(
    bytes: &[u8],
    start: usize,
    layout: &FixedWidth,
    enc: TextEncoding,
    mut on_progress: impl FnMut(usize) -> bool,
) -> Result<Indexed, FixedError> {
    let n = bytes.len();
    let mut records = Vec::new();
    let mut le = LineEndings::default();
    let mut i = start;
    let mut line = 1u64;
    let mut next_check = start + CHECK_INTERVAL;
    while i < n {
        if i >= next_check {
            if on_progress(i) {
                return Err(FixedError::Cancelled);
            }
            next_check = i + CHECK_INTERVAL;
        }
        let end = memchr2(b'\r', b'\n', &bytes[i..]).map_or(n, |o| i + o);
        if end - i > u32::MAX as usize {
            return Err(FixedError::LineTooLarge { line });
        }
        check_line(&bytes[i..end], layout, enc, line)?;
        records.push(RecordPos { start: i as u64, end: end as u64, line_start: line, line_end: line });
        i = if end >= n {
            le.unterminated_last = true;
            n
        } else if bytes[end] == b'\r' && bytes.get(end + 1) == Some(&b'\n') {
            le.crlf += 1;
            end + 2
        } else if bytes[end] == b'\r' {
            le.cr += 1;
            end + 1
        } else {
            le.lf += 1;
            end + 1
        };
        line += 1;
    }
    Ok(Indexed { records, field_count: layout.widths.len(), line_endings: le })
}

#[cfg(test)]
mod tests {
    use super::*;
    use TextEncoding::{EucJp, Utf8, Windows31j};

    fn layout(widths: &[u32], unit: WidthUnit, trim: bool) -> FixedWidth {
        FixedWidth { widths: widths.to_vec(), unit, trim }
    }

    fn fields(rec: &[u8], l: &FixedWidth, enc: TextEncoding) -> Vec<Vec<u8>> {
        let mut spans = Vec::new();
        split(rec, l, enc, usize::MAX, &mut spans);
        spans.iter().map(|s| rec[s.start as usize..s.end as usize].to_vec()).collect()
    }

    #[test]
    fn bytes_in_windows31j() {
        // "001" "山田" (4 bytes) padded to 6, "東京" (4 bytes).
        let rec = b"001\x8eR\x93c  \x93\x8c\x8b\x9e";
        let l = layout(&[3, 6, 4], WidthUnit::Bytes, true);
        assert_eq!(check_line(rec, &l, Windows31j, 1), Ok(()));
        assert_eq!(fields(rec, &l, Windows31j), [&b"001"[..], b"\x8eR\x93c", b"\x93\x8c\x8b\x9e"]);
        // A first column of 4 bytes would end inside 山.
        let bad = layout(&[4, 5, 4], WidthUnit::Bytes, true);
        assert_eq!(check_line(rec, &bad, Windows31j, 7), Err(FixedError::SplitsCharacter { line: 7, column: 1 }));
    }

    #[test]
    fn a_second_byte_that_looks_like_ascii_is_not_a_boundary() {
        // ポ is 0x83 0x7C: its second byte is `|`.
        let rec = b"\x83\x7cA";
        assert_eq!(
            check_line(rec, &layout(&[1, 2], WidthUnit::Bytes, false), Windows31j, 1),
            Err(FixedError::SplitsCharacter { line: 1, column: 1 })
        );
        assert_eq!(fields(rec, &layout(&[2, 1], WidthUnit::Bytes, false), Windows31j), [&b"\x83\x7c"[..], b"A"]);
    }

    #[test]
    fn half_width_katakana_in_euc_jp_is_two_bytes() {
        // ｱ is 0x8E 0xB1 in EUC-JP, one byte in Windows-31J.
        let rec = b"\x8e\xb1A";
        let bad = layout(&[1, 2], WidthUnit::Bytes, false);
        assert_eq!(check_line(rec, &bad, EucJp, 1), Err(FixedError::SplitsCharacter { line: 1, column: 1 }));
        assert_eq!(fields(rec, &layout(&[1, 1], WidthUnit::Chars, false), EucJp), [&b"\x8e\xb1"[..], b"A"]);
    }

    #[test]
    fn characters_in_utf8() {
        let rec = "001山田　 東京".as_bytes();
        let l = layout(&[3, 4, 2], WidthUnit::Chars, true);
        assert_eq!(check_line(rec, &l, Utf8, 1), Ok(()));
        assert_eq!(fields(rec, &l, Utf8), ["001".as_bytes(), "山田".as_bytes(), "東京".as_bytes()]);
        let kept = layout(&[3, 4, 2], WidthUnit::Chars, false);
        assert_eq!(fields(rec, &kept, Utf8)[1], "山田　 ".as_bytes());
        // UTF-8 bytes: a boundary inside 山 (3 bytes).
        assert_eq!(
            check_line(rec, &layout(&[4, 20], WidthUnit::Bytes, true), Utf8, 2),
            Err(FixedError::SplitsCharacter { line: 2, column: 1 })
        );
    }

    #[test]
    fn short_lines_leave_columns_empty_and_long_lines_are_errors() {
        let l = layout(&[2, 3, 3], WidthUnit::Bytes, true);
        assert_eq!(fields(b"ab c", &l, Utf8), [&b"ab"[..], b"c", b""]);
        assert_eq!(check_line(b"ab", &l, Utf8, 1), Ok(()));
        assert_eq!(check_line(b"abcdefghi", &l, Utf8, 4), Err(FixedError::TooLong { line: 4, length: 9, total: 8 }));
    }

    #[test]
    fn lines_and_terminators() {
        let l = layout(&[1, 1], WidthUnit::Chars, false);
        let idx = index(b"ab\r\ncd\ref\ngh", 0, &l, Utf8, |_| false).unwrap();
        assert_eq!(idx.records.len(), 4);
        assert_eq!((idx.records[2].start, idx.records[2].end, idx.records[2].line_start), (7, 9, 3));
        assert_eq!((idx.line_endings.crlf, idx.line_endings.cr, idx.line_endings.lf), (1, 1, 1));
        assert!(idx.line_endings.unterminated_last);
        assert_eq!(
            index(b"ab\nabc\n", 0, &l, Utf8, |_| false).unwrap_err(),
            FixedError::TooLong { line: 2, length: 3, total: 2 }
        );
    }
}
