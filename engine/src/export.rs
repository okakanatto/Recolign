//! Writing rows of a view to a file: CSV (UTF-8 with or without BOM, or
//! Windows-31J) or an Excel workbook with the differences marked.
//!
//! Values are written exactly as read. In Excel every value is a text cell,
//! so leading zeros, dates and values starting with `=` stay as they are.

use std::fmt;
use std::io::Write;

use rust_xlsxwriter::{Color, Format, FormatBorder, IgnoreError, Workbook, XlsxError};
use serde::Deserialize;

use crate::cancel::CancelToken;
use crate::compare::{Comparison, Kind, IGNORED_BIT, NONE};
use crate::error::Side;
use crate::table::Table;
use crate::tokenizer::FieldSpan;
use crate::view::{CopySide, Order, Slot, View};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[default]
    Csv,
    Xlsx,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportEncoding {
    /// UTF-8 starting with a BOM, which spreadsheet applications use to
    /// recognise UTF-8.
    #[default]
    Utf8Bom,
    Utf8,
    Windows31j,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExportRequest {
    /// First row (inclusive).
    #[serde(default)]
    pub start: usize,
    /// Last row (exclusive); omitted means to the end of the view.
    #[serde(default)]
    pub end: Option<usize>,
    /// Display slots in the order they are written.
    pub slots: Vec<usize>,
    pub side: CopySide,
    /// A column with the kind of difference (変更, 左のみ, 右のみ, 一致).
    #[serde(default)]
    pub kind_column: bool,
    /// Columns with each side's line number in its file.
    #[serde(default)]
    pub line_numbers: bool,
    /// A column with the names of the columns whose value changed.
    #[serde(default)]
    pub changed_columns: bool,
    #[serde(default)]
    pub format: ExportFormat,
    /// CSV only.
    #[serde(default)]
    pub encoding: ExportEncoding,
    /// CSV only: put `'` before values that spreadsheet applications would
    /// read as a formula (see [`formula_like`]). This changes those values,
    /// so it is a choice.
    #[serde(default)]
    pub neutralize: bool,
}

/// Whether a spreadsheet application could read `v` as a formula: it starts
/// with `=`, `+`, `-`, `@`, a tab or a carriage return, or their full-width
/// forms, which Japanese Excel can also take as the start of a formula, or
/// the minus sign `−` (U+2212), which Shift_JIS writes as the full-width `－`.
/// Spaces before that character are skipped. A number such as `-5` and a lone `-`
/// or `+` stay as they are: they cannot do anything when read as a formula.
pub fn formula_like(v: &str) -> bool {
    let t = v.trim_start_matches([' ', '\u{3000}']);
    let mut cs = t.chars();
    match cs.next() {
        Some('=' | '@' | '\t' | '\r' | '\u{FF1D}' | '\u{FF20}') => true,
        Some('+' | '-' | '\u{FF0B}' | '\u{FF0D}' | '\u{2212}') => {
            let rest = cs.as_str();
            !rest.is_empty() && !(rest.chars().any(digit) && rest.chars().all(|c| digit(c) || matches!(c, '.' | ',')))
        }
        _ => false,
    }
}

fn digit(c: char) -> bool {
    c.is_ascii_digit() || ('\u{FF10}'..='\u{FF19}').contains(&c)
}

/// Columns the engine does not know (review state and memo), written after
/// the kind, line and changed-column columns.
pub struct Extra<'a> {
    pub names: Vec<String>,
    pub values: &'a (dyn Fn(u32) -> Vec<String> + Sync),
}

#[derive(Debug)]
pub enum ExportError {
    Io(std::io::Error),
    Cancelled,
    /// A value that the chosen encoding cannot represent. `line` is the
    /// line of the output file (the header is line 1).
    Unrepresentable {
        line: usize,
        column: String,
        ch: char,
    },
    /// Beyond what an Excel sheet can hold.
    Excel(String),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportError::Io(e) => write!(f, "{e}"),
            ExportError::Cancelled => write!(f, "書き出しを中止しました。"),
            ExportError::Unrepresentable { line, column, ch } => write!(
                f,
                "書き出すファイルの{line}行目、列「{column}」に、Shift_JIS（Windows-31J）で表せない文字「{ch}」（U+{:04X}）があります。\
                 UTF-8で書き出すか、Excel形式を選んでください。",
                *ch as u32
            ),
            ExportError::Excel(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<std::io::Error> for ExportError {
    fn from(e: std::io::Error) -> Self {
        ExportError::Io(e)
    }
}

/// Excel's limits for one sheet.
pub const XLSX_MAX_ROWS: usize = 1_048_576;
const XLSX_MAX_COLS: usize = 16_384;
const XLSX_MAX_CHARS: usize = 32_767;

/// What a value cell shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Same,
    Changed,
    /// Differs only in a way the comparison rules ignore.
    Ignored,
    /// The record does not exist on this side.
    NoRecord,
    /// The column does not exist on this side.
    NoColumn,
    /// Kind, line number and changed-column cells.
    Meta,
}

pub fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::Same => "一致",
        Kind::Changed => "変更",
        Kind::Added => "右のみ",
        Kind::Deleted => "左のみ",
    }
}

struct Plan<'a> {
    start: usize,
    end: usize,
    sides: Vec<Side>,
    slots: Vec<&'a Slot>,
    header: Vec<String>,
    /// Columns before the values (kind, lines, changed columns).
    meta: usize,
}

impl<'a> Plan<'a> {
    fn new(view: &View, all_slots: &'a [Slot], req: &ExportRequest, extra: Option<&Extra>) -> Plan<'a> {
        let start = req.start.min(view.len());
        let end = req.end.unwrap_or(usize::MAX).min(view.len()).max(start);
        let sides: Vec<Side> = match (view.order, req.side) {
            (Order::LeftSource, _) => vec![Side::Left],
            (Order::RightSource, _) => vec![Side::Right],
            (_, CopySide::Left) => vec![Side::Left],
            (_, CopySide::Right) => vec![Side::Right],
            (_, CopySide::Both) => vec![Side::Left, Side::Right],
        };
        let slots: Vec<&Slot> = req.slots.iter().filter_map(|&i| all_slots.get(i)).collect();
        let mut header = Vec::new();
        if req.kind_column {
            header.push("種類".to_string());
        }
        if req.line_numbers {
            for &side in &sides {
                header.push(format!("{}の行", side.label()));
            }
        }
        if req.changed_columns {
            header.push("変更した列".to_string());
        }
        if let Some(x) = extra {
            header.extend(x.names.iter().cloned());
        }
        let meta = header.len();
        for &side in &sides {
            for s in &slots {
                let name = match side {
                    Side::Left => s.name_left.as_deref().or(s.name_right.as_deref()),
                    Side::Right => s.name_right.as_deref().or(s.name_left.as_deref()),
                }
                .unwrap_or("");
                header.push(if sides.len() == 2 { format!("{}:{name}", side.label()) } else { name.to_string() });
            }
        }
        Plan { start, end, sides, slots, header, meta }
    }

    fn rows(&self) -> usize {
        self.end - self.start
    }

    /// Calls `f` with the kind and the cells of every row in order.
    fn each_row(
        &self,
        view: &View,
        cmp: &Comparison,
        req: &ExportRequest,
        extra: Option<&Extra>,
        cancel: &CancelToken,
        mut f: impl FnMut(usize, Kind, &[(String, Mark)]) -> Result<(), ExportError>,
    ) -> Result<(), ExportError> {
        let pair_name = |k: u32| cmp.left.headers[cmp.mapping.pairs[k as usize].left as usize].as_str();
        let mut cells: Vec<(String, Mark)> = Vec::with_capacity(self.header.len());
        let mut spans: Vec<FieldSpan> = Vec::new();
        for (i, row) in (self.start..self.end).enumerate() {
            if i % 4096 == 0 && cancel.is_cancelled() {
                return Err(ExportError::Cancelled);
            }
            let u = view.unit_at(cmp, row);
            let unit = cmp.units[u];
            cells.clear();
            if req.kind_column {
                cells.push((kind_label(unit.kind).to_string(), Mark::Meta));
            }
            let rec_of = |side: Side| -> (&Table, u32) {
                match side {
                    Side::Left => (&cmp.left, unit.left),
                    Side::Right => (&cmp.right, unit.right),
                }
            };
            if req.line_numbers {
                for &side in &self.sides {
                    let (t, rec) = rec_of(side);
                    let line = if rec == NONE { String::new() } else { t.records[rec as usize].line_start.to_string() };
                    cells.push((line, Mark::Meta));
                }
            }
            if req.changed_columns {
                let names: Vec<&str> = cmp.changed_pairs(u).map(pair_name).collect();
                cells.push((names.join("、"), Mark::Meta));
            }
            if let Some(x) = extra {
                cells.extend((x.values)(u as u32).into_iter().map(|v| (v, Mark::Meta)));
            }
            let marked = unit.kind == Kind::Changed || unit.ignored;
            for &side in &self.sides {
                let (t, rec) = rec_of(side);
                if rec != NONE {
                    t.split(rec as usize, usize::MAX, &mut spans);
                }
                for s in &self.slots {
                    let col = match side {
                        Side::Left => s.left,
                        Side::Right => s.right,
                    };
                    let cell = match (rec != NONE, col) {
                        (false, _) => (String::new(), Mark::NoRecord),
                        (true, None) => (String::new(), Mark::NoColumn),
                        (true, Some(c)) => {
                            let mark = match s.pair.filter(|_| marked).and_then(|k| cmp.diff_entry(u, k)) {
                                Some(e) if e & IGNORED_BIT != 0 => Mark::Ignored,
                                Some(_) => Mark::Changed,
                                None => Mark::Same,
                            };
                            (t.value(rec as usize, spans[c as usize]).into_owned(), mark)
                        }
                    };
                    cells.push(cell);
                }
            }
            f(i, unit.kind, &cells)?;
        }
        Ok(())
    }
}

fn csv_cell(out: &mut String, v: &str) {
    if v.contains([',', '\n', '\r', '"']) {
        out.push('"');
        out.push_str(&v.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(v);
    }
}

/// Writes rows of `view` as CSV (CRLF line ends). Returns the number of
/// data rows written.
pub fn export_csv<W: Write>(
    view: &View,
    cmp: &Comparison,
    all_slots: &[Slot],
    req: &ExportRequest,
    extra: Option<&Extra>,
    w: &mut W,
    cancel: &CancelToken,
) -> Result<usize, ExportError> {
    let plan = Plan::new(view, all_slots, req, extra);
    if req.encoding == ExportEncoding::Utf8Bom {
        w.write_all(b"\xEF\xBB\xBF")?;
    }
    let mut line = String::new();
    let mut write_line = |line_no: usize,
                          cells: &mut dyn Iterator<Item = &str>,
                          w: &mut W|
     -> Result<(), ExportError> {
        line.clear();
        let values: Vec<&str> = cells.collect();
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                line.push(',');
            }
            if req.neutralize && formula_like(v) {
                csv_cell(&mut line, &format!("'{v}"));
            } else {
                csv_cell(&mut line, v);
            }
        }
        line.push_str("\r\n");
        if req.encoding == ExportEncoding::Windows31j {
            // ¥ (U+00A5), ‾ (U+203E) and − (U+2212) are written at their
            // JIS places, which Windows reads as \, ~ and － (older systems
            // read 0x5C as ¥).
            let (bytes, _, unmappable) = encoding_rs::SHIFT_JIS.encode(&line);
            if unmappable {
                let (k, ch) = values
                    .iter()
                    .enumerate()
                    .find_map(|(k, v)| {
                        v.chars().find(|c| encoding_rs::SHIFT_JIS.encode(c.encode_utf8(&mut [0; 4])).2).map(|c| (k, c))
                    })
                    .unwrap_or((0, '\u{FFFD}'));
                return Err(ExportError::Unrepresentable { line: line_no, column: plan.header[k].clone(), ch });
            }
            w.write_all(&bytes)?;
        } else {
            w.write_all(line.as_bytes())?;
        }
        Ok(())
    };
    write_line(1, &mut plan.header.iter().map(String::as_str), w)?;
    plan.each_row(view, cmp, req, extra, cancel, |i, _, cells| {
        write_line(i + 2, &mut cells.iter().map(|(v, _)| v.as_str()), w)
    })?;
    w.flush()?;
    Ok(plan.rows())
}

/// Display width of a header in Excel's character units (wide characters
/// count twice).
fn text_width(s: &str) -> f64 {
    s.chars().map(|c| if (c as u32) < 0x1100 { 1.0 } else { 2.0 }).sum()
}

struct Formats {
    header: Format,
    kind: [Format; 4],
    changed: Format,
    ignored: Format,
    added: Format,
    deleted: Format,
    no_record: Format,
    no_column: Format,
    label: Format,
}

impl Formats {
    fn new() -> Formats {
        let text = || Format::new().set_num_format("@");
        Formats {
            header: text().set_bold().set_background_color(Color::RGB(0xEBE9E6)).set_border_bottom(FormatBorder::Thin),
            kind: [
                text().set_font_color(Color::RGB(0x5D5A56)),
                text().set_bold().set_font_color(Color::RGB(0x7A5C00)),
                text().set_bold().set_font_color(Color::RGB(0xA3241B)),
                text().set_bold().set_font_color(Color::RGB(0x17663A)),
            ],
            // Changed values are bold as well as filled, so they stand out
            // without relying on colour.
            changed: text().set_bold().set_background_color(Color::RGB(0xFDEEB0)),
            ignored: text()
                .set_italic()
                .set_font_color(Color::RGB(0x6B6763))
                .set_background_color(Color::RGB(0xEEEEEE)),
            added: text().set_background_color(Color::RGB(0xD9F2E1)),
            deleted: text().set_background_color(Color::RGB(0xFBE0DE)),
            no_record: text().set_background_color(Color::RGB(0xE3E1DE)),
            no_column: text().set_background_color(Color::RGB(0xEFEEEC)),
            label: Format::new().set_bold(),
        }
    }

    fn kind(&self, k: Kind) -> &Format {
        &self.kind[match k {
            Kind::Same => 0,
            Kind::Changed => 1,
            Kind::Deleted => 2,
            Kind::Added => 3,
        }]
    }
}

fn xlsx(e: XlsxError) -> ExportError {
    match e {
        XlsxError::IoError(e) => ExportError::Io(e),
        other => ExportError::Excel(format!("Excelファイルを作れませんでした（{other}）。")),
    }
}

/// Writes rows of `view` to `out` as an Excel workbook: a sheet with the
/// rows (changed values filled yellow and bold, added rows green, deleted
/// rows red, absent records grey) and a sheet with `info` (the files and
/// conditions) and the meaning of the colours.
#[allow(clippy::too_many_arguments)] // mirrors export_csv, plus the conditions sheet
pub fn export_xlsx(
    view: &View,
    cmp: &Comparison,
    all_slots: &[Slot],
    req: &ExportRequest,
    extra: Option<&Extra>,
    info: &[(String, String)],
    out: impl Write + Send,
    cancel: &CancelToken,
) -> Result<usize, ExportError> {
    let plan = Plan::new(view, all_slots, req, extra);
    if plan.rows() + 1 > XLSX_MAX_ROWS {
        return Err(ExportError::Excel(format!(
            "Excelの1つのシートに入るのは{}行までです（書き出す行は{}行）。範囲を絞るか、CSVで書き出してください。",
            XLSX_MAX_ROWS - 1,
            plan.rows()
        )));
    }
    if plan.header.len() > XLSX_MAX_COLS {
        return Err(ExportError::Excel(format!(
            "Excelの1つのシートに入るのは{XLSX_MAX_COLS}列までです（書き出す列は{}列）。左だけ・右だけにするか、CSVで書き出してください。",
            plan.header.len()
        )));
    }
    let f = Formats::new();
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet_with_constant_memory().map_err(xlsx)?;
    ws.set_name("比較結果").map_err(xlsx)?;

    // Column widths from the header and the first rows.
    let mut widths: Vec<f64> = plan.header.iter().map(|h| text_width(h) + 2.0).collect();
    let probe = ExportRequest { end: Some((plan.start + 200).min(plan.end)), ..req.clone() };
    let probe_plan = Plan::new(view, all_slots, &probe, extra);
    probe_plan.each_row(view, cmp, &probe, extra, cancel, |_, _, cells| {
        for (k, (v, _)) in cells.iter().enumerate() {
            let w = text_width(v.lines().next().unwrap_or("")) + 1.0;
            if w > widths[k] {
                widths[k] = w;
            }
        }
        Ok(())
    })?;
    for (k, w) in widths.iter().enumerate() {
        ws.set_column_width(k as u16, w.clamp(6.0, 48.0)).map_err(xlsx)?;
    }
    let leading_keys = plan.slots.iter().take_while(|s| s.key).count();
    ws.set_freeze_panes(1, (plan.meta + leading_keys) as u16).map_err(xlsx)?;

    for (k, h) in plan.header.iter().enumerate() {
        ws.write_string_with_format(0, k as u16, h, &f.header).map_err(xlsx)?;
    }
    let header = &plan.header;
    plan.each_row(view, cmp, req, extra, cancel, |i, kind, cells| {
        let r = (i + 1) as u32;
        for (k, (v, mark)) in cells.iter().enumerate() {
            let c = k as u16;
            let fmt = match mark {
                Mark::Meta if req.kind_column && k == 0 => Some(f.kind(kind)),
                Mark::Meta => None,
                Mark::Changed => Some(&f.changed),
                Mark::Ignored => Some(&f.ignored),
                Mark::NoRecord => Some(&f.no_record),
                Mark::NoColumn => Some(&f.no_column),
                Mark::Same => match kind {
                    Kind::Added => Some(&f.added),
                    Kind::Deleted => Some(&f.deleted),
                    _ => None,
                },
            };
            if v.chars().count() > XLSX_MAX_CHARS {
                return Err(ExportError::Excel(format!(
                    "書き出す{}行目、列「{}」の値が{XLSX_MAX_CHARS}文字を超えるため、Excelには書き出せません。CSVで書き出してください。",
                    i + 2,
                    header[k]
                )));
            }
            let res = match (v.is_empty(), fmt) {
                (true, Some(fmt)) => ws.write_blank(r, c, fmt),
                (true, None) => continue,
                (false, Some(fmt)) => ws.write_string_with_format(r, c, v, fmt),
                (false, None) => ws.write_string(r, c, v),
            };
            res.map_err(xlsx)?;
        }
        Ok(())
    })?;
    let last_row = plan.rows() as u32;
    let last_col = (plan.header.len().max(1) - 1) as u16;
    ws.autofilter(0, 0, last_row, last_col).map_err(xlsx)?;
    ws.ignore_error_range(1, 0, last_row.max(1), last_col, IgnoreError::NumberStoredAsText).map_err(xlsx)?;

    let ws2 = wb.add_worksheet();
    ws2.set_name("比較条件").map_err(xlsx)?;
    ws2.set_column_width(0, 22).map_err(xlsx)?;
    ws2.set_column_width(1, 90).map_err(xlsx)?;
    let mut r = 0u32;
    for (label, value) in info {
        ws2.write_string_with_format(r, 0, label, &f.label).map_err(xlsx)?;
        ws2.write_string(r, 1, value).map_err(xlsx)?;
        r += 1;
    }
    r += 1;
    ws2.write_string_with_format(r, 0, "色の意味", &f.label).map_err(xlsx)?;
    for (fmt, text) in [
        (&f.changed, "値が変わったセル（太字）"),
        (&f.deleted, "左のファイルだけにあるレコード（左のみ）"),
        (&f.added, "右のファイルだけにあるレコード（右のみ）"),
        (&f.ignored, "比較条件で無視した違い（比較しない列、前後の空白）"),
        (&f.no_record, "その側にレコードがない"),
        (&f.no_column, "その側のファイルにない列"),
    ] {
        r += 1;
        ws2.write_blank(r, 0, fmt).map_err(xlsx)?;
        ws2.write_string(r, 1, text).map_err(xlsx)?;
    }
    wb.save_to_writer(out).map_err(xlsx)?;
    Ok(plan.rows())
}
