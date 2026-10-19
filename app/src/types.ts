// Shapes of the engine's requests and replies (see cdr-service).

export type Encoding = 'utf8' | 'windows31j' | 'utf16le' | 'utf16be' | 'euc_jp';
export type Trim = 'none' | 'ascii_space_tab';

export type WidthUnit = 'bytes' | 'chars';

/** Fixed-width text: the width of each column, left to right. */
export interface FixedWidth {
  widths: number[];
  unit: WidthUnit;
  /** Spaces around each value are padding. */
  trim: boolean;
}

export interface LoadOptions {
  encoding: Encoding;
  /** Code of the delimiter (an ASCII character): 44 `,`, 9 TAB, 59 `;`, 124 `|`. */
  delimiter: number;
  has_header: boolean;
  /** Values may be enclosed in `"`. Missing in requests saved before it existed. */
  quoted?: boolean;
  fixed?: FixedWidth | null;
}

export interface SideInput {
  path: string;
  options: LoadOptions;
}

export interface CompareRequest {
  left: SideInput;
  right: SideInput;
  keys: string[];
  excluded: string[];
  trim: Trim;
}

export interface Preview {
  path: string;
  name: string;
  bytes: number;
  modified_unix_ms: number | null;
  suggested: { encoding: Encoding | null; delimiter: string; quoted: boolean; has_header: boolean };
  options: LoadOptions;
  bom: boolean;
  headers: string[];
  rows: string[][];
  partial: boolean;
  error: string | null;
  /** The first lines as they are in the file. */
  lines: string[];
  /** With fixed widths: the same lines cut into columns, spaces kept. */
  pieces: string[][];
  /** For each column, why it cannot be the key on its own, as far as the
   *  records read show; null when its values are all filled and different. */
  key_issues: (KeyIssue | null)[];
}

export type KeyIssue = { kind: 'duplicate'; value: string; lines: [number, number] } | { kind: 'empty'; line: number };

export interface SideSummary {
  name: string;
  path: string | null;
  bytes: number;
  records: number;
  columns: number;
  encoding: Encoding;
  /** Empty for fixed-width text. */
  delimiter: string;
  quoted: boolean;
  fixed: FixedWidth | null;
  /** How the file was cut into values, in words. */
  layout: string;
  bom: boolean;
  sha256: string | null;
  line_endings: { crlf: number; lf: number; cr: number; unterminated_last: boolean };
  has_header: boolean;
}

export interface Counts {
  left_records: number;
  right_records: number;
  units: number;
  same: number;
  same_with_ignored: number;
  changed: number;
  deleted: number;
  added: number;
}

export interface Summary {
  comparison_id: string | null;
  left: SideSummary;
  right: SideSummary;
  counts: Counts;
  keys: string[];
  excluded: string[];
  trim: Trim;
  left_only: string[];
  right_only: string[];
  pair_names: string[];
  pair_changed: number[];
  pair_ignored: number[];
  timings_ms: { total_to_usable: number; load: number; compare: number };
}

export interface RecordRef {
  record: number;
  line_start: number;
  line_end: number;
}

export interface KeyProblems {
  duplicate_groups_total: number;
  duplicate_records_total: number;
  duplicate_groups: { key: string[]; records: RecordRef[] }[];
  empty_total: number;
  empty: RecordRef[];
}

export interface FailedInfo {
  code: string;
  message: string;
  detail: { left_name: string; right_name: string; left: KeyProblems; right: KeyProblems } | null;
}

export interface SideProgress {
  bytes_total: number;
  bytes_read: number;
  bytes_indexed: number;
  records: number;
  keys: number;
}

export interface Status {
  generation: number;
  phase: { state: 'idle' } | { state: 'running' } | { state: 'failed'; error: FailedInfo } | { state: 'ready'; summary: Summary };
  stage: string;
  left: SideProgress;
  right: SideProgress;
  units_total: number;
  units_compared: number;
  patterns_ready: boolean;
  elapsed_ms: number;
}

export const ENCODING_LABEL: Record<Encoding, string> = {
  utf8: 'UTF-8',
  windows31j: 'Shift_JIS（Windows-31J）',
  utf16le: 'UTF-16 LE',
  utf16be: 'UTF-16 BE',
  euc_jp: 'EUC-JP',
};
export const DELIMITERS: [number, string][] = [
  [44, 'カンマ（,）'],
  [9, 'タブ'],
  [59, 'セミコロン（;）'],
  [124, '縦線（|）'],
];

/** The request that produced a summary, for changing the conditions. */
export function requestOf(s: Summary): CompareRequest | null {
  if (!s.left.path || !s.right.path) return null;
  const side = (x: SideSummary): SideInput => ({
    path: x.path!,
    options: {
      encoding: x.encoding,
      delimiter: x.delimiter ? x.delimiter.charCodeAt(0) : 44,
      has_header: x.has_header,
      quoted: x.quoted,
      fixed: x.fixed,
    },
  });
  return { left: side(s.left), right: side(s.right), keys: s.keys, excluded: s.excluded, trim: s.trim };
}
