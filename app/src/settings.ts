// User preferences kept between sessions: next to the exe (or in the user
// profile when that folder is not writable). File and column names (recent
// files, last keys, columns left out of the patterns) always stay in the
// user profile; the desktop shell splits them off (PRIVATE_KEYS).

import { ENCODING_LABEL, type LoadOptions } from './types';

export type Layout = 'side' | 'stack';
export type SortBy = 'unit' | 'key' | 'kind' | 'changed_count' | 'column';

export interface SortSpec {
  by: SortBy;
  desc: boolean;
  slot?: number;
  side?: 'left' | 'right';
}

export interface ExportOptions {
  format: 'xlsx' | 'csv';
  encoding: 'utf8_bom' | 'utf8' | 'windows31j';
  side: 'both' | 'left' | 'right';
  kind: boolean;
  changed: boolean;
  lines: boolean;
  hidden: boolean;
  /** CSV: put `'` before values a spreadsheet would read as a formula. */
  protectFormulas: boolean;
}

export const DEFAULT_EXPORT: ExportOptions = {
  format: 'xlsx',
  encoding: 'utf8_bom',
  side: 'both',
  kind: true,
  changed: true,
  lines: true,
  hidden: false,
  protectFormulas: true,
};

/** A comparison run before, offered again on the setup screen. */
export interface RecentComparison {
  left: { path: string; options: LoadOptions };
  right: { path: string; options: LoadOptions };
  keys: string[];
  excluded: string[];
  trim: 'none' | 'ascii_space_tab';
  /** When it was run (ms since 1970). */
  at: number;
}

export const RECENT_MAX = 8;

export interface Settings {
  layout: Layout;
  zoom: number;
  detailOpen: boolean;
  /** Height of the detail panel below side-by-side tables (0 = default). */
  detailHeight: number;
  /** Width of the detail panel beside stacked tables (0 = default). */
  detailWidth: number;
  showSpaces: boolean;
  changedColumnsOnly: boolean;
  /** Review states and notes (G3). Off by default (U-12). */
  reviewEnabled: boolean;
  /** Choices last made in the export dialog. */
  exportOptions: ExportOptions;
  /** Newest first. */
  recent: RecentComparison[];
  /** Keep recent comparisons (file and key column names). */
  keepHistory: boolean;
  /** Keep copies out of the Windows clipboard history and cloud clipboard. */
  clipboardExclude: boolean;
  /** The side panel with review progress and change patterns. */
  sidebarOpen: boolean;
  /** How change patterns are grouped (plan 6, U-16). */
  groupRule: 'columns' | 'per_column' | 'full';
  /** Columns left out of the change patterns, by name. */
  groupSkip: string[];
  /** After marking a record, move to the next unchecked one. */
  advanceAfterReview: boolean;
}

export const DEFAULTS: Settings = {
  layout: 'side',
  zoom: 1,
  detailOpen: true,
  detailHeight: 0,
  detailWidth: 0,
  showSpaces: false,
  changedColumnsOnly: false,
  reviewEnabled: false,
  exportOptions: { ...DEFAULT_EXPORT },
  recent: [],
  keepHistory: true,
  clipboardExclude: false,
  sidebarOpen: false,
  groupRule: 'per_column',
  groupSkip: [],
  advanceAfterReview: true,
};

export const ZOOM_STEPS = [0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2, 2.5, 3];

export function mergeSettings(saved: Partial<Settings>): Settings {
  const s = { ...DEFAULTS };
  for (const k of Object.keys(DEFAULTS) as (keyof Settings)[]) {
    if (saved && typeof saved[k] === typeof DEFAULTS[k]) (s as any)[k] = saved[k];
  }
  const eo: Partial<ExportOptions> = (saved && typeof saved.exportOptions === 'object' && saved.exportOptions) || {};
  s.exportOptions = { ...DEFAULT_EXPORT };
  for (const k of Object.keys(DEFAULT_EXPORT) as (keyof ExportOptions)[]) {
    if (typeof eo[k] === typeof DEFAULT_EXPORT[k]) (s.exportOptions as any)[k] = eo[k];
  }
  const okWidths = (f: any) =>
    f == null ||
    (Array.isArray(f.widths) &&
      f.widths.length > 0 &&
      f.widths.every((w: unknown) => Number.isInteger(w) && (w as number) > 0 && (w as number) <= 1_000_000) &&
      (f.unit === 'bytes' || f.unit === 'chars'));
  const okOptions = (o: any) =>
    o &&
    Object.hasOwn(ENCODING_LABEL, o.encoding) &&
    Number.isInteger(o.delimiter) &&
    o.delimiter > 0 &&
    o.delimiter < 0x80 &&
    typeof o.has_header === 'boolean' &&
    okWidths(o.fixed);
  const okSide = (x: any) => x && typeof x.path === 'string' && okOptions(x.options);
  s.recent = Array.isArray(s.recent)
    ? s.recent.filter((r: any) => r && okSide(r.left) && okSide(r.right) && Array.isArray(r.keys)).slice(0, RECENT_MAX)
    : [];
  if (!['columns', 'per_column', 'full'].includes(s.groupRule)) s.groupRule = 'per_column';
  if (!Array.isArray(s.groupSkip) || !s.groupSkip.every((x) => typeof x === 'string')) s.groupSkip = [];
  if (!ZOOM_STEPS.includes(s.zoom)) s.zoom = 1;
  return s;
}
