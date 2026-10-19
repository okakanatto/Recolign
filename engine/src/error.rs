//! Errors. Each message states what happened, where, and how to fix it.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::encoding::TextEncoding;
use crate::fixed::WidthUnit;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn label(self) -> &'static str {
        match self {
            Side::Left => "左",
            Side::Right => "右",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CsvErrorKind {
    UnclosedQuote,
    QuoteInUnquotedField,
    TextAfterClosingQuote,
}

#[derive(Debug)]
pub enum LoadError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Size or modification time differed before and after reading.
    Changed {
        path: PathBuf,
    },
    /// A folder, device or pipe rather than a file.
    NotAFile {
        path: PathBuf,
    },
    Cancelled,
    Empty {
        name: String,
    },
    /// The file starts with the byte order mark of `bom`, not of `chosen`.
    BomMismatch {
        name: String,
        bom: TextEncoding,
        chosen: TextEncoding,
    },
    /// The reading options cannot be used (`message` says why and what to do).
    Options {
        name: String,
        message: String,
    },
    Decode {
        name: String,
        encoding: TextEncoding,
        line: u64,
        byte_offset: u64,
        hint: Option<&'static str>,
    },
    Csv {
        name: String,
        kind: CsvErrorKind,
        line: u64,
        byte_offset: u64,
    },
    FieldCount {
        name: String,
        line: u64,
        record: u64,
        found: usize,
        expected: usize,
        blank: bool,
    },
    TooLarge {
        name: String,
        what: &'static str,
    },
    /// More columns than [`crate::table::MAX_COLUMNS`].
    TooManyColumns {
        name: String,
        found: usize,
    },
    /// A fixed-width line is longer than the widths (in `unit`).
    FixedTooLong {
        name: String,
        line: u64,
        length: u64,
        total: u64,
        unit: WidthUnit,
    },
    /// Fixed-width column `column` (1-based) would end inside a character.
    FixedSplitsCharacter {
        name: String,
        line: u64,
        column: usize,
    },
}

impl LoadError {
    pub fn code(&self) -> &'static str {
        match self {
            LoadError::Io { .. } => "io",
            LoadError::Changed { .. } => "source_changed",
            LoadError::NotAFile { .. } => "not_a_file",
            LoadError::Cancelled => "cancelled",
            LoadError::Empty { .. } => "empty_file",
            LoadError::BomMismatch { .. } => "bom_mismatch",
            LoadError::Options { .. } => "options",
            LoadError::Decode { .. } => "decode",
            LoadError::Csv { kind, .. } => match kind {
                CsvErrorKind::UnclosedQuote => "unclosed_quote",
                CsvErrorKind::QuoteInUnquotedField => "quote_in_unquoted_field",
                CsvErrorKind::TextAfterClosingQuote => "text_after_closing_quote",
            },
            LoadError::FieldCount { blank: true, .. } => "blank_line",
            LoadError::FieldCount { .. } => "field_count",
            LoadError::TooLarge { .. } => "too_large",
            LoadError::TooManyColumns { .. } => "too_many_columns",
            LoadError::FixedTooLong { .. } => "fixed_too_long",
            LoadError::FixedSplitsCharacter { .. } => "fixed_splits_character",
        }
    }
}

/// Added to quote errors: the file may simply not use quotes.
const NO_QUOTES: &str = "引用符を使わないファイルなら、読み込みの設定で「引用符（\"）を使う」をオフにしてください。";

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io { path, source } => write!(
                f,
                "{}: ファイルを読めません（{}）。パスとアクセス権を確認してください。",
                path.display(),
                source
            ),
            LoadError::Changed { path } => write!(
                f,
                "{}: 読込中にファイルが変更されました。更新が終わってから、もう一度読み込んでください。",
                path.display()
            ),
            LoadError::NotAFile { path } => write!(
                f,
                "{}: ファイルではないため読めません。フォルダーではなく、CSVなどのファイルを選んでください。",
                path.display()
            ),
            LoadError::Cancelled => write!(f, "読込を取り消しました。"),
            LoadError::Empty { name } => write!(
                f,
                "{name}: ファイルが空です。見出し行のあるCSVを指定してください。"
            ),
            LoadError::BomMismatch { name, bom, chosen } => write!(
                f,
                "{name}: 先頭に{}のBOMがありますが、文字コードに{}が指定されています。{}を指定してください。",
                bom.label(),
                chosen.label(),
                bom.label()
            ),
            LoadError::Options { name, message } => write!(f, "{name}: {message}"),
            LoadError::Decode { name, encoding, line, byte_offset, hint } => {
                write!(
                    f,
                    "{name}: {line}行目（バイト位置 {byte_offset}）を{}として読めません。文字コードの指定を確認してください。",
                    encoding.label()
                )?;
                if let Some(hint) = hint {
                    write!(f, "{hint}")?;
                }
                Ok(())
            }
            LoadError::Csv { name, kind, line, byte_offset } => match kind {
                CsvErrorKind::UnclosedQuote => write!(
                    f,
                    "{name}: {line}行目で始まる引用符「\"」が、ファイル末尾まで閉じていません。出力元のCSVを確認してください。{NO_QUOTES}"
                ),
                CsvErrorKind::QuoteInUnquotedField => write!(
                    f,
                    "{name}: {line}行目（バイト位置 {byte_offset}）で、引用符で囲まれていない値の途中に「\"」があります。値に「\"」を含める場合は、値全体を「\"」で囲み「\"\"」と書く必要があります。{NO_QUOTES}"
                ),
                CsvErrorKind::TextAfterClosingQuote => write!(
                    f,
                    "{name}: {line}行目（バイト位置 {byte_offset}）で、閉じ引用符の後に区切り文字・改行以外の文字があります。出力元のCSVを確認してください。{NO_QUOTES}"
                ),
            },
            LoadError::FieldCount { name, line, record, found, expected, blank } => {
                if *blank {
                    write!(
                        f,
                        "{name}: {line}行目が空行です。見出しが{expected}列のため、列数不一致として中止しました。空行を削除してください。"
                    )
                } else {
                    write!(
                        f,
                        "{name}: {line}行目（データ{record}件目）の列数が{found}です。見出しは{expected}列です。区切り文字の指定と、値の中の区切り文字が引用符で囲まれているかを確認してください。"
                    )
                }
            }
            LoadError::TooLarge { name, what } => {
                write!(f, "{name}: {what}が処理できる上限を超えています。")
            }
            LoadError::TooManyColumns { name, found } => write!(
                f,
                "{name}: 列が{found}列あります。読めるのは{}列までです。読み込みの設定の「区切り」が、このファイルの区切り文字と合っているかを確かめてください。",
                crate::table::MAX_COLUMNS
            ),
            LoadError::FixedTooLong { name, line, length, total, unit } => write!(
                f,
                "{name}: {line}行目の長さ（{length}{u}）が、列の幅の合計（{total}{u}）を超えています。列の幅と単位を確かめてください。",
                u = unit.label()
            ),
            LoadError::FixedSplitsCharacter { name, line, column } => write!(
                f,
                "{name}: {line}行目で、{column}列目の終わりが文字の途中になります。列の幅を数え直すか、単位を「文字」にしてください。"
            ),
        }
    }
}

impl std::error::Error for LoadError {}

/// One record position for error listings.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RecordRef {
    /// 1-based data record number (header excluded).
    pub record: u64,
    pub line_start: u64,
    pub line_end: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct DuplicateGroup {
    pub key: Vec<String>,
    pub records: Vec<RecordRef>,
}

/// Key validation results for one side. Totals are exact; the lists are capped.
#[derive(Clone, Debug, Default, Serialize)]
pub struct KeyProblems {
    pub duplicate_groups_total: u64,
    pub duplicate_records_total: u64,
    pub duplicate_groups: Vec<DuplicateGroup>,
    pub empty_total: u64,
    pub empty: Vec<RecordRef>,
}

impl KeyProblems {
    pub fn is_empty(&self) -> bool {
        self.duplicate_groups_total == 0 && self.empty_total == 0
    }
}

#[derive(Debug)]
pub enum CompareError {
    Cancelled,
    Load {
        side: Side,
        error: LoadError,
    },
    ColumnMapping {
        problems: Vec<String>,
    },
    NoKey,
    KeyColumn {
        message: String,
    },
    TooManyUnits,
    KeyValidation(Box<KeyValidationReport>),
    /// Stopped because the memory left for the process ran low.
    OutOfMemory {
        available: u64,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyValidationReport {
    pub left_name: String,
    pub right_name: String,
    pub left: KeyProblems,
    pub right: KeyProblems,
}

impl CompareError {
    pub fn code(&self) -> &'static str {
        match self {
            CompareError::Cancelled => "cancelled",
            CompareError::Load { error, .. } => error.code(),
            CompareError::ColumnMapping { .. } => "column_mapping",
            CompareError::NoKey => "no_key",
            CompareError::KeyColumn { .. } => "key_column",
            CompareError::TooManyUnits => "too_many_units",
            CompareError::KeyValidation(_) => "key_validation",
            CompareError::OutOfMemory { .. } => "out_of_memory",
        }
    }
}

const LISTED_PROBLEMS: usize = 5;

fn write_problems(f: &mut fmt::Formatter<'_>, side: &str, name: &str, p: &KeyProblems) -> fmt::Result {
    if p.is_empty() {
        return Ok(());
    }
    write!(f, "\n{side}（{name}）:")?;
    if p.duplicate_groups_total > 0 {
        write!(f, " 重複キー {}種・{}レコード", p.duplicate_groups_total, p.duplicate_records_total)?;
    }
    if p.empty_total > 0 {
        write!(f, " 空のキー {}レコード", p.empty_total)?;
    }
    for g in p.duplicate_groups.iter().take(LISTED_PROBLEMS) {
        let lines: Vec<String> = g.records.iter().map(|r| format!("{}行目", r.line_start)).collect();
        write!(f, "\n  重複キー [{}]: {}", g.key.join(", "), lines.join(" / "))?;
    }
    for r in p.empty.iter().take(LISTED_PROBLEMS) {
        write!(f, "\n  空のキー: {}行目", r.line_start)?;
    }
    Ok(())
}

impl fmt::Display for CompareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompareError::Cancelled => {
                write!(f, "比較を取り消しました。結果は保存されていません。")
            }
            CompareError::Load { side, error } => {
                write!(f, "{}のファイル: {}", side.label(), error)
            }
            CompareError::ColumnMapping { problems } => {
                write!(f, "列名が空の列か、同じ列名の列があるため、左右の列を対応させられません。")?;
                for p in problems {
                    write!(f, "\n  {p}")?;
                }
                write!(
                    f,
                    "\n列名を1つずつ違う名前にしてください。1行目が列名でないファイルは、読み込みの設定で「1行目は列名」を外すと、列名を「列1」「列2」…とします。"
                )
            }
            CompareError::NoKey => write!(f, "キーにする列を1つ以上選んでください。"),
            CompareError::KeyColumn { message } => write!(f, "{message}"),
            CompareError::TooManyUnits => {
                write!(f, "レコード数が処理できる上限（約42億）を超えています。")
            }
            CompareError::OutOfMemory { available } => write!(
                f,
                "メモリが足りなくなったため、比較を中止しました（空きが約{} MBまで減りました）。\n\
                 ほかのアプリを閉じてから、もう一度比較してください。比較するファイルと保存した確認状況は変わっていません。",
                available / (1 << 20)
            ),
            CompareError::KeyValidation(report) => {
                let KeyValidationReport {
                    left_name,
                    right_name,
                    left,
                    right,
                } = report.as_ref();
                write!(f, "キーの値が同じレコードか、キーが空のレコードがあるため、比較を中止しました。")?;
                write_problems(f, "左", left_name, left)?;
                write_problems(f, "右", right_name, right)?;
                write!(
                    f,
                    "\nキーには、1件ずつ値が違い、空にならない列を選んでください。1つの列で区別できないときは、キーの列を増やして組み合わせます（例：社員番号と適用日）。同じキーのレコードどうしを自動で対応させることはしません。"
                )
            }
        }
    }
}

impl std::error::Error for CompareError {}
