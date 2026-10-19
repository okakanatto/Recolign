// How one file is read, on the setup screen: encoding, layout (a delimiter,
// a character of one's own, or fixed widths), quotes and header, and the
// first lines of the file, with the columns marked off for fixed widths.
//
// The state of a file (FileState) is kept by the setup screen. Controls
// here change it only through the host: a change of options reads the file
// again; a layout not set yet, or text that cannot be used, marks the file
// as not ready, so that nothing is compared with settings other than the
// ones on screen.

import { DELIMITERS, ENCODING_LABEL, type Encoding, type FixedWidth, type LoadOptions, type Preview, type WidthUnit } from './types';
import { el } from './util';

export interface FileState {
  path: string;
  /** The options last asked for (a reading of them may be under way). */
  options: LoadOptions | null;
  /** The last reading that worked: what the card shows. */
  shown: Preview | null;
  /** Why the file could not be read at all (no reading has worked). */
  error: string | null;
  /** A reading is under way. */
  reading: boolean;
  /** Counts the readings asked for; only the answer to the last one is used. */
  seq: number;
  /** A layout picked but not usable yet: no widths, or no delimiter typed,
   *  or text typed for it that cannot be used (`invalid`). */
  draft?: 'fixed' | 'other';
  /** Text typed that cannot be used, kept on screen until it is corrected. */
  invalid?: { field: 'widths' | 'delimiter'; text: string };
  /** Unit and padding chosen for fixed width, not applied yet. */
  fixedChoice?: { unit: WidthUnit; trim: boolean };
}

export interface ReadOptionsHost {
  /** Reads the file again with these options changed. A change of layout
   *  ends a layout being set and text that could not be used. */
  reread(st: FileState, change: Partial<LoadOptions>): void;
  /** The draft or invalid state changed: show it. With `controls`, the
   *  controls change too (a layout was picked); otherwise only what is
   *  below them, so that the focus moving from a text box to another
   *  control (which uses the text) is not lost to a redraw. */
  marked(st: FileState, controls: boolean): void;
}

/** Full-width letters, digits and signs (as typed with the IME on) as half-width. */
export function halfWidth(text: string): string {
  return text.replace(/[\uff01-\uff5e]/g, (c) => String.fromCharCode(c.charCodeAt(0) - 0xfee0)).replace(/\u3000/g, ' ');
}

export const MAX_WIDTH = 1_000_000;

/** Widths typed as "4, 10, 8" (full-width digits and commas too); `null` if
 *  they are not all whole numbers from 1 to MAX_WIDTH. */
export function parseWidths(text: string): number[] | null {
  const parts = halfWidth(text)
    .split(/[\s,、]+/)
    .filter(Boolean);
  if (!parts.length || !parts.every((x) => /^[0-9]+$/.test(x) && Number(x) > 0 && Number(x) <= MAX_WIDTH)) return null;
  return parts.map(Number);
}

// ---- layouts -----------------------------------------------------------------
// A layout is what the 区切り select shows: 'fixed', 'other' (a character
// of one's own) or the code of one of the usual delimiters, as a string.

const usual = (d: number) => DELIMITERS.some(([v]) => v === d);

/** The layout of options, as read. */
function layoutOf(o: LoadOptions): string {
  return o.fixed ? 'fixed' : usual(o.delimiter) ? String(o.delimiter) : 'other';
}

/** The layout the card shows: one being set, or the one asked for. */
function layoutShown(st: FileState, o: LoadOptions): string {
  return st.draft ?? layoutOf(o);
}

/** What keeps the file from being compared as set on screen, as the end of
 *  a sentence after 「左のファイルの」; `null` if nothing does. */
export function notReady(st: FileState): string | null {
  if (st.invalid) return st.invalid.field === 'widths' ? '列の幅を直してください。' : '区切り文字を直してください。';
  if (st.draft) return st.draft === 'fixed' ? '列の幅を入力してください。' : '区切り文字を入力してください。';
  return null;
}

// ---- text typed and not used yet ----------------------------------------------
// A text box keeps, in `data-v`, the text last used. Text typed since then
// is not used yet: Enter, leaving the box (and, for a delimiter, each
// character) uses it. A card drawn anew takes such text over.

/** Before a card is drawn anew: the text typed and not used yet in its
 *  boxes, by box. The boxes are made to do nothing more, as taking out a
 *  focused box fires its blur. */
export function takeTyped(card: HTMLElement): Map<string, string> {
  const typed = new Map<string, string>();
  for (const t of card.querySelectorAll<HTMLInputElement>('input[data-v]')) {
    if (t.value !== t.dataset.v) typed.set(t.dataset.k!, t.value);
    t.dataset.v = t.value;
  }
  return typed;
}

/** After a card is drawn anew: the text taken over goes into the new boxes. */
export function putTyped(card: HTMLElement, typed: Map<string, string>): void {
  for (const [k, v] of typed) {
    const t = card.querySelector<HTMLInputElement>(`input[data-v][data-k="${k}"]`);
    if (t) t.value = v;
  }
}

// ---- controls -------------------------------------------------------------------

/** A select with `data-k`, so that a redraw of the card can put the focus back on it. */
function select(key: string, label: string, choices: [string, string][], value: string): HTMLSelectElement {
  const s = el('select');
  s.setAttribute('aria-label', label);
  s.dataset.k = key;
  for (const [v, text] of choices) {
    const o = el('option', '', text);
    o.value = v;
    o.selected = v === value;
    s.append(o);
  }
  return s;
}

function checkbox(key: string, text: string, checked: boolean, title: string): { label: HTMLLabelElement; box: HTMLInputElement } {
  const label = el('label', 'check');
  const box = el('input');
  box.type = 'checkbox';
  box.dataset.k = key;
  box.checked = checked;
  label.append(box, document.createTextNode(text));
  label.title = title;
  return { label, box };
}

/** The id of the note that says what is wrong with a file's settings. */
const noteIds = new WeakMap<FileState, string>();
let notes = 0;
function noteId(st: FileState): string {
  let id = noteIds.get(st);
  if (!id) noteIds.set(st, (id = `readnote${++notes}`));
  return id;
}

/** Marks a text box as holding text that cannot be used (pointing to the
 *  note that says why), or not. */
function setInvalid(t: HTMLInputElement, st: FileState, invalid: boolean): void {
  if (invalid) {
    t.setAttribute('aria-invalid', 'true');
    t.setAttribute('aria-describedby', noteId(st));
  } else {
    t.removeAttribute('aria-invalid');
    t.removeAttribute('aria-describedby');
  }
}

function textField(key: string, label: string, value: string): HTMLInputElement {
  const t = el('input');
  t.type = 'text';
  t.value = value;
  t.dataset.k = key;
  t.dataset.v = value;
  t.setAttribute('aria-label', label);
  return t;
}

/** The rows of reading options of a file that has been read once. */
export function readOptionsRows(st: FileState, p: Preview, host: ReadOptionsHost): HTMLElement[] {
  // Changes build on the options last asked for, not on the reading shown.
  const opts = (): LoadOptions => st.options ?? p.options;
  const o = opts();
  const row = el('div', 'opts');

  const enc = select(
    'enc',
    '文字コード',
    (Object.entries(ENCODING_LABEL) as [Encoding, string][]).map(([v, label]) => [
      v,
      label + (p.suggested.encoding === v ? '（推定）' : ''),
    ]),
    o.encoding,
  );
  enc.title =
    p.suggested.encoding === null
      ? 'ファイルの先頭は、どの文字コードとしても正しく読めません。'
      : 'ファイルの先頭から推定しました。文字化けしているときは切り替えます。';
  enc.addEventListener('change', () => host.reread(st, { encoding: enc.value as Encoding }));
  const l1 = el('label', 'field', '文字コード');
  l1.append(enc);

  const shown = layoutShown(st, o);
  const del = select(
    'del',
    '区切り',
    [
      ...DELIMITERS.map(([v, label]): [string, string] => [
        String(v),
        label + (p.suggested.delimiter.charCodeAt(0) === v ? '（推定）' : ''),
      ]),
      ['other', 'ほかの文字'],
      ['fixed', '固定長（列の幅で区切る）'],
    ],
    shown,
  );
  del.title = '1行目で最も多く使われている区切り文字を推定しました。区切り文字のないファイルは「固定長」を選びます。';
  del.addEventListener('change', () => {
    const now = opts();
    const picked = del.value;
    if (picked === layoutOf(now)) {
      // Back to the layout in use, from one being set: as it is used.
      st.draft = undefined;
      st.invalid = undefined;
      st.fixedChoice = undefined;
      host.marked(st, true);
    } else if (picked === 'other' && !usual(now.delimiter)) {
      // From fixed width back to a character of one's own set before: use it again.
      host.reread(st, { fixed: null });
    } else if (picked === 'fixed' || picked === 'other') {
      st.draft = picked;
      st.invalid = undefined;
      host.marked(st, true);
    } else {
      host.reread(st, { delimiter: Number(picked), fixed: null });
    }
  });
  const l2 = el('label', 'field', '区切り');
  l2.append(del);
  const items: HTMLElement[] = [l1, l2];

  // The box of a delimiter and the widths act only while 区切り shows their
  // layout: after another layout is picked, they stay until the file is
  // read again, and are not to bring back a layout no longer asked for.
  const shows = (layout: string) => () => del.isConnected && del.value === layout;
  if (shown === 'other') items.push(delimiterField(st, host, opts, shows('other')));

  if (shown !== 'fixed') {
    const q = checkbox(
      'quoted',
      '引用符（"）を使う',
      o.quoted !== false,
      (p.suggested.quoted ? '' : '「"」を引用符として読むとエラーになるため、オフと推定しました。\n') +
        'オンのとき、「"」で囲んだ値の中の区切り文字や改行は値の一部になります（CSVの決まり）。\n' +
        'オフにすると「"」もふつうの文字として読みます。データベースから出したタブ区切りのファイルなど、引用符を使わないファイルのときにオフにします。',
    );
    q.box.addEventListener('change', () => host.reread(st, { quoted: q.box.checked }));
    items.push(q.label);
  }

  const h = checkbox(
    'header',
    '1行目は列名',
    o.has_header,
    (p.suggested.has_header ? '' : '1行目がデータの行に見えるため、オフと推定しました。\n') + '外すと、列名を「列1」「列2」…とします。',
  );
  h.box.addEventListener('change', () => host.reread(st, { has_header: h.box.checked }));
  row.append(...items, h.label);
  return shown === 'fixed' ? [row, fixedRow(st, host, opts, shows('fixed'))] : [row];
}

/** The box for a delimiter of one's own. Each character typed is used. */
function delimiterField(st: FileState, host: ReadOptionsHost, opts: () => LoadOptions, live: () => boolean): HTMLInputElement {
  const invalid = st.invalid?.field === 'delimiter';
  const text = invalid ? st.invalid!.text : st.draft === 'other' ? '' : String.fromCharCode(opts().delimiter);
  const one = textField('one', '区切り文字', text);
  setInvalid(one, st, invalid);
  one.className = 'onechar';
  one.maxLength = 1;
  one.title = '区切りに使う1文字を入力します（「"」は使えません）。全角で入力した記号は半角として使います。';
  const use = () => {
    if (!live() || one.value === one.dataset.v) return; // replaced, another layout picked, or nothing new
    one.dataset.v = one.value;
    const c = halfWidth(one.value);
    const now = opts();
    const bad = !!c && (c.charCodeAt(0) >= 0x80 || c === '"');
    setInvalid(one, st, bad);
    if (!c) {
      st.draft = 'other';
      st.invalid = undefined;
      host.marked(st, false);
    } else if (bad) {
      // The layout being set is a character of one's own, whatever an
      // earlier character typed here (a comma, say) is being read with.
      st.draft = 'other';
      st.invalid = { field: 'delimiter', text: one.value };
      host.marked(st, false);
    } else {
      // A full-width sign shows as the half-width one it is used as.
      one.value = one.dataset.v = c;
      if (c.charCodeAt(0) !== now.delimiter || now.fixed || st.draft || st.invalid)
        host.reread(st, { delimiter: c.charCodeAt(0), fixed: null });
    }
  };
  // While the IME is composing, wait for the character it settles on. A
  // character still being composed when the card was drawn anew is used
  // when the focus leaves the box.
  one.addEventListener('input', (e) => {
    if (!(e as InputEvent).isComposing) use();
  });
  one.addEventListener('compositionend', use);
  one.addEventListener('blur', use);
  return one;
}

function sameFixed(a: FixedWidth, b: FixedWidth | null | undefined): boolean {
  return !!b && a.unit === b.unit && a.trim === b.trim && a.widths.join() === b.widths.join();
}

/** Fixed widths: the widths, their unit and whether spaces are padding. */
function fixedRow(st: FileState, host: ReadOptionsHost, opts: () => LoadOptions, live: () => boolean): HTMLElement {
  const f = opts().fixed;
  const encoding = opts().encoding;
  // Fixed-width files in Shift_JIS or EUC-JP count bytes (a full-width character is 2).
  const byteDefault = encoding === 'windows31j' || encoding === 'euc_jp';
  const choice = st.fixedChoice ?? (f ? { unit: f.unit, trim: f.trim } : { unit: byteDefault ? 'bytes' : 'chars', trim: true });
  const row = el('div', 'opts');
  const invalid = st.invalid?.field === 'widths';
  const applied = f && st.draft !== 'fixed' ? f.widths.join(', ') : '';
  const widths = textField('widths', '列の幅', invalid ? st.invalid!.text : applied);
  setInvalid(widths, st, invalid);
  widths.className = 'widths';
  widths.placeholder = '例：4, 10, 8';
  widths.title = '左の列から順に、各列の幅をカンマで区切って入力します。Enterを押すか、ほかの場所を押すと反映します。';
  const unit = select(
    'unit',
    '幅の単位',
    [
      ['bytes', 'バイト'],
      ['chars', '文字'],
    ],
    choice.unit,
  );
  unit.title =
    'Shift_JISでは、全角文字は2バイト、半角文字（半角カナを含む）は1バイトです。\n' +
    'EUC-JPでは、全角文字と半角カナは2バイトです。UTF-8では、漢字1文字が3バイトです。';
  const trim = checkbox(
    'trim',
    '前後の空白を除く',
    choice.trim,
    '値の前後にある半角と全角の空白を、詰め物として値から除きます。外すと、空白も値の一部として比べます。',
  );
  // Every change applies the widths, unit and padding together.
  const apply = () => {
    if (!live()) return; // replaced, or another layout picked
    widths.dataset.v = widths.value;
    st.fixedChoice = { unit: unit.value as WidthUnit, trim: trim.box.checked };
    const text = widths.value.trim();
    const w = text ? parseWidths(text) : null;
    setInvalid(widths, st, !!text && !w);
    if (!text) {
      st.draft = 'fixed';
      st.invalid = undefined;
      host.marked(st, false);
    } else if (!w) {
      // Fixed width stays the layout being set, even if a reading of earlier
      // widths fails and the options go back to a delimiter.
      st.draft = 'fixed';
      st.invalid = { field: 'widths', text: widths.value };
      host.marked(st, false);
    } else {
      const next = { widths: w, ...st.fixedChoice };
      if (st.draft || st.invalid || !sameFixed(next, opts().fixed)) host.reread(st, { fixed: next });
      else st.fixedChoice = undefined;
    }
  };
  // Typed widths are used with Enter (Ctrl+Enter too) and when the focus leaves the box.
  const commit = () => {
    if (widths.value !== widths.dataset.v) apply();
  };
  widths.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.isComposing) commit();
  });
  widths.addEventListener('blur', commit);
  unit.addEventListener('change', apply);
  trim.box.addEventListener('change', apply);
  const lw = el('label', 'field', '列の幅');
  lw.append(widths);
  row.append(lw, unit, trim.label);
  return row;
}

// ---- below the controls ---------------------------------------------------------

/** In place of the preview while the layout is not set: what to do, and
 *  the first lines of the file to set it by. `null` once it is set. */
export function layoutNotSet(st: FileState, p: Preview): HTMLElement[] | null {
  let note: HTMLElement;
  if (st.invalid?.field === 'widths') {
    note = el('div', 'error', '列の幅を読み取れません。「4, 10, 8」のように、1から1,000,000までの数をカンマで区切って入力してください。');
  } else if (st.invalid) {
    note = el('div', 'error', `「${st.invalid.text}」は区切り文字に使えません。「"」以外の半角の1文字を入力してください。`);
  } else if (st.draft === 'fixed') {
    note = el('div', 'muted', '列の幅を入力してEnterを押すと、区切った結果をここに表示します。');
  } else if (st.draft === 'other') {
    note = el('div', 'muted', '区切り文字を入力すると、区切った結果をここに表示します。');
  } else {
    return null;
  }
  note.id = noteId(st);
  return [note, rawLines(p, false)];
}

/** In place of a parse error, when the first line has no delimiter at all:
 *  the error is then about something else (a quote, say) and misleads.
 *  `null` if the line has one. */
export function delimiterMissing(p: Preview): HTMLElement[] | null {
  const d = p.options.delimiter;
  if (!p.error || p.options.fixed || !p.lines.length || p.lines[0].includes(String.fromCharCode(d))) return null;
  const names: Record<number, string> = { 44: 'カンマ', 9: 'タブ', 59: 'セミコロン', 124: '縦線' };
  const name = names[d] ?? `「${String.fromCharCode(d)}」`;
  const note = el(
    'div',
    'error',
    `1行目に区切り文字の${name}がありません。「区切り」を選び直してください。区切り文字のないファイルは「固定長」を選びます。`,
  );
  note.title = p.error;
  return [note, rawLines(p, false)];
}

/** The first lines of the file in a fixed-pitch font; with `cut`, the
 *  columns of fixed widths are marked off, so that wrong widths show at once. */
export function rawLines(p: Preview, cut = !!p.options.fixed): HTMLElement {
  const box = el('div', 'rawlines');
  box.setAttribute('aria-label', 'ファイルの先頭の行');
  const lines: string[][] = cut && p.pieces.length ? p.pieces : p.lines.map((l) => [l]);
  for (const pieces of lines) {
    const line = el('div', 'rawline');
    for (const piece of pieces) line.append(el('span', 'piece', piece));
    box.append(line);
  }
  box.title = cut ? '区切った位置を縦線で示します。' : 'ファイルの先頭の行です。';
  return box;
}
