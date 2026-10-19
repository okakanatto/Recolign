//! Text encodings. The whole file is validated once, without replacement
//! characters.
//!
//! UTF-8, Windows-31J and EUC-JP files are parsed on their original bytes:
//! `"`, CR, LF and the delimiters below 0x40 (TAB, `,`, `;`, …) never occur
//! inside a multi-byte sequence of these encodings, and in UTF-8 and EUC-JP
//! no ASCII byte does. So record positions refer to the original bytes.
//! UTF-16 files, and Windows-31J files with a delimiter from 0x40 up (`|`
//! can be the second byte of a Windows-31J character), are converted to
//! UTF-8 once after validation and parsed as UTF-8 (`table.rs`).

use std::borrow::Cow;

use encoding_rs::{DecoderResult, Encoding, EUC_JP, SHIFT_JIS, UTF_16BE, UTF_16LE, UTF_8};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextEncoding {
    /// UTF-8. A leading BOM (EF BB BF) is accepted and recorded.
    Utf8,
    /// Windows-31J (CP932), decoded with the WHATWG Shift_JIS decoder of
    /// encoding_rs. Bytes that decoder rejects are reported as errors.
    Windows31j,
    /// UTF-16 little endian, as Excel saves "Unicode text". A leading BOM
    /// (FF FE) is accepted and recorded.
    Utf16le,
    /// UTF-16 big endian. A leading BOM (FE FF) is accepted and recorded.
    Utf16be,
    /// EUC-JP, decoded with the WHATWG EUC-JP decoder of encoding_rs.
    EucJp,
}

impl TextEncoding {
    pub fn label(self) -> &'static str {
        match self {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Windows31j => "Shift_JIS（Windows-31J）",
            TextEncoding::Utf16le => "UTF-16 LE",
            TextEncoding::Utf16be => "UTF-16 BE",
            TextEncoding::EucJp => "EUC-JP",
        }
    }

    pub(crate) fn codec(self) -> &'static Encoding {
        match self {
            TextEncoding::Utf8 => UTF_8,
            TextEncoding::Windows31j => SHIFT_JIS,
            TextEncoding::Utf16le => UTF_16LE,
            TextEncoding::Utf16be => UTF_16BE,
            TextEncoding::EucJp => EUC_JP,
        }
    }

    pub fn is_utf16(self) -> bool {
        matches!(self, TextEncoding::Utf16le | TextEncoding::Utf16be)
    }

    /// The byte order mark this encoding may start with.
    pub fn bom(self) -> &'static [u8] {
        match self {
            TextEncoding::Utf8 => UTF8_BOM,
            TextEncoding::Windows31j | TextEncoding::EucJp => b"",
            TextEncoding::Utf16le => UTF16LE_BOM,
            TextEncoding::Utf16be => UTF16BE_BOM,
        }
    }
}

pub const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
pub const UTF16LE_BOM: &[u8] = b"\xFF\xFE";
pub const UTF16BE_BOM: &[u8] = b"\xFE\xFF";

/// Returned when a long-running check was cancelled.
#[derive(Debug)]
pub struct Cancelled;

/// Byte offset of the first malformed sequence of `bytes` in `encoding`,
/// if any. `check_cancel(offset)` is called every few hundred KiB.
pub fn first_invalid_in(
    encoding: TextEncoding,
    bytes: &[u8],
    check_cancel: impl FnMut(usize) -> bool,
) -> Result<Option<usize>, Cancelled> {
    first_invalid(encoding.codec(), bytes, check_cancel)
}

fn first_invalid(
    encoding: &'static Encoding,
    bytes: &[u8],
    mut check_cancel: impl FnMut(usize) -> bool,
) -> Result<Option<usize>, Cancelled> {
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let mut out = vec![0u8; 256 * 1024];
    let mut pos = 0usize;
    loop {
        let (result, read, _) = decoder.decode_to_utf8_without_replacement(&bytes[pos..], &mut out, true);
        pos += read;
        match result {
            DecoderResult::InputEmpty => return Ok(None),
            DecoderResult::OutputFull => {
                if check_cancel(pos) {
                    return Err(Cancelled);
                }
            }
            DecoderResult::Malformed(bad, extra) => {
                return Ok(Some(pos - bad as usize - extra as usize));
            }
        }
    }
}

/// Decodes a field or record slice of an already validated file.
pub fn decode(encoding: TextEncoding, raw: &[u8]) -> Cow<'_, str> {
    encoding
        .codec()
        .decode_without_bom_handling_and_without_replacement(raw)
        .expect("slice of a validated file is decodable")
}

/// A UTF-16 file (BOM excluded) converted to UTF-8, or the byte offset of
/// the first malformed sequence (an unpaired surrogate or an odd last byte).
pub fn utf16_to_utf8(
    bytes: &[u8],
    big_endian: bool,
    check_cancel: impl FnMut(usize) -> bool,
) -> Result<Result<String, usize>, Cancelled> {
    let encoding = if big_endian { UTF_16BE } else { UTF_16LE };
    if let Some(off) = first_invalid(encoding, bytes, check_cancel)? {
        return Ok(Err(off));
    }
    let text = encoding.decode_without_bom_handling_and_without_replacement(bytes).expect("validated UTF-16");
    Ok(Ok(text.into_owned()))
}

/// Short hint for a decoding failure in `chosen`: the encoding the file
/// does read in, if one of the others does.
pub fn failure_hint(chosen: TextEncoding, bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(UTF16LE_BOM) || bytes.starts_with(UTF16BE_BOM) {
        return Some("先頭にUTF-16のBOMがあります。文字コードにUTF-16を指定してください。");
    }
    // Only a hint: the first few MiB are enough (a character cut at the end
    // of them does not count against an encoding).
    let head = &bytes[..bytes.len().min(4 << 20)];
    let reads = |e: TextEncoding| match first_invalid_in(e, head, |_| false) {
        Ok(None) => true,
        Ok(Some(off)) => head.len() < bytes.len() && off + 3 >= head.len(),
        Err(_) => false,
    };
    let others: Vec<TextEncoding> = [TextEncoding::Utf8, TextEncoding::Windows31j, TextEncoding::EucJp]
        .into_iter()
        .filter(|&e| e != chosen && reads(e))
        .collect();
    // Text that is valid UTF-8 is almost surely UTF-8.
    let best = if others.contains(&TextEncoding::Utf8) { Some(TextEncoding::Utf8) } else { likeliest(&others, head) };
    best.map(|e| match e {
        TextEncoding::Utf8 => "UTF-8としては読める可能性があります。",
        TextEncoding::Windows31j => "Shift_JIS（Windows-31J）としては読める可能性があります。",
        _ => "EUC-JPとしては読める可能性があります。",
    })
}

/// Of Japanese encodings the bytes are all valid in, the one they read best
/// in. EUC-JP text is often also valid Windows-31J, and the other way round;
/// in the wrong one it tends to become rare kanji, half-width katakana or
/// private-use characters.
///
/// - Kana (hiragana, katakana) in a reading is the surest sign: the reading
///   with more kana and kanji, and fewer half-width katakana and
///   private-use characters, wins.
/// - Without any kana, a reading wins outright only on a strong sign of
///   half-width katakana data (bank and fixed-width files, whose wrong
///   reading is kanji): the EUC-JP reading has half-width katakana (EUC-JP
///   marks each with 0x8E), or a reading is half-width katakana alone that
///   reads as kana (sound marks and small kana only where kana can carry
///   them). Kanji-only text in EUC-JP read as Windows-31J also becomes
///   half-width katakana, but rarely in such a well-formed way.
/// - Otherwise, more kanji and fewer half-width katakana wins.
pub(crate) fn likeliest(candidates: &[TextEncoding], bytes: &[u8]) -> Option<TextEncoding> {
    struct Reading {
        encoding: TextEncoding,
        kana: i64,
        kanji: i64,
        half_kana: i64,
        private: i64,
        plausible_half_kana: bool,
    }
    let read = |e: TextEncoding| {
        let text = e.codec().decode_without_bom_handling(bytes.strip_prefix(e.bom()).unwrap_or(bytes)).0;
        let mut r = Reading { encoding: e, kana: 0, kanji: 0, half_kana: 0, private: 0, plausible_half_kana: true };
        let mut prev = 0u32;
        for ch in text.chars() {
            let c = ch as u32;
            match c {
                0x3040..=0x30FF => r.kana += 1,
                0x4E00..=0x9FFF | 0xFF01..=0xFF5E => r.kanji += 1,
                0xFF61..=0xFF9F => r.half_kana += 1,
                0xE000..=0xF8FF => r.private += 1,
                _ => {}
            }
            let fits = match c {
                // ﾞ after ｳ, ｶ–ﾄ, ﾊ–ﾎ, ﾜ, ｦ; ﾟ after ﾊ–ﾎ.
                0xFF9E => matches!(prev, 0xFF73 | 0xFF76..=0xFF84 | 0xFF8A..=0xFF8E | 0xFF9C | 0xFF66),
                0xFF9F => matches!(prev, 0xFF8A..=0xFF8E),
                // Small kana and ｰ after a kana.
                0xFF67..=0xFF70 => matches!(prev, 0xFF71..=0xFF9F),
                // ｡｢｣､･ are rare in data.
                0xFF61..=0xFF65 => false,
                _ => true,
            };
            r.plausible_half_kana &= fits;
            prev = c;
        }
        r
    };
    let readings: Vec<Reading> = candidates.iter().map(|&e| read(e)).collect();
    let any_kana = readings.iter().any(|r| r.kana > 0);
    readings
        .into_iter()
        .max_by_key(|r| {
            let rest = r.kanji - r.half_kana - 5 * r.private;
            if any_kana {
                (0, r.kana + rest)
            } else {
                let euc_kana = r.encoding == TextEncoding::EucJp && r.half_kana > 0;
                let kana_alone = r.half_kana > 0 && r.kanji == 0 && r.private == 0 && r.plausible_half_kana;
                (i64::from(euc_kana || kana_alone), rest)
            }
        })
        .map(|r| r.encoding)
}
