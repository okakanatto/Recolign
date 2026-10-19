// Detail of the selected record: changed columns with the changed characters
// marked, every column, and the original CSV text of both records.

import type { Backend } from './api';
import { diffChars } from './diff';
import { KIND, MARK, metrics, type Kind } from './grid';
import { blockText, button, el, fmt } from './util';

interface RecDetail {
  side: 'left' | 'right';
  record: number;
  line_start: number;
  line_end: number;
  byte_start: number;
  byte_end: number;
  raw: string;
  /** `raw` is only the start of the record's text. */
  raw_cut: boolean;
}

interface DetailCell {
  slot: number;
  name_left: string | null;
  name_right: string | null;
  key: boolean;
  excluded: boolean;
  left: string | null;
  right: string | null;
  /** The value is only its first characters. */
  left_cut: boolean;
  right_cut: boolean;
  /** 0 same, 1 changed, 2 differs only in a way the rules ignore. */
  d: number;
}

export interface DetailData {
  unit: number;
  kind: Kind;
  ignored: boolean;
  key: string[];
  pattern: number | null;
  left: RecDetail | null;
  right: RecDetail | null;
  cells: DetailCell[];
  review?: { status: 'unchecked' | 'checked' | 'investigate'; memo: string; at: number };
}

type Tab = 'changes' | 'all' | 'raw';

export interface DetailHost {
  showSpaces: boolean;
  keyNames: string[];
  reviewEnabled: boolean;
  onToggle(open: boolean): void;
  onStatus(unit: number, status: 'unchecked' | 'checked' | 'investigate'): void;
  onMemo(unit: number, memo: string): void;
}

const STATUS: ['unchecked' | 'checked' | 'investigate', string, string][] = [
  ['unchecked', '未確認', '1'],
  ['checked', '確認済み', '2'],
  ['investigate', '要調査', '3'],
];

function lines(r: RecDetail): string {
  return r.line_start === r.line_end ? `${fmt(r.line_start)}行目` : `${fmt(r.line_start)}〜${fmt(r.line_end)}行目`;
}

export class DetailPanel {
  readonly root: HTMLElement;
  readonly grip: HTMLDivElement;
  private sum: HTMLDivElement;
  private body: HTMLDivElement;
  private tabs = new Map<Tab, HTMLButtonElement>();
  private toggle: HTMLButtonElement;
  private tab: Tab = 'changes';
  private data: DetailData | null = null;
  private seq = 0;

  constructor(
    private api: Backend,
    private host: DetailHost,
    open: boolean,
  ) {
    this.root = el('section', 'detail');
    this.root.id = 'detail';
    this.root.setAttribute('aria-label', '選択したレコードの詳細');
    this.grip = el('div', 'grip');
    this.grip.title = 'ドラッグして大きさを変えます';
    const head = el('div', 'dhead');
    this.sum = el('div', 'dsum');
    const tabs = el('div', 'tabs');
    tabs.setAttribute('role', 'tablist');
    for (const [t, label, title] of [
      ['changes', '変更した列', '値が違う列だけを表示します'],
      ['all', '全列', 'すべての列を表示します'],
      ['raw', '元の行', 'ファイルに書かれているとおりのレコードを表示します'],
    ] as const) {
      const b = button(label, () => this.setTab(t), { title });
      b.setAttribute('role', 'tab');
      this.tabs.set(t, b);
      tabs.append(b);
    }
    this.toggle = button('', () => this.setOpen(this.root.classList.contains('collapsed')));
    this.toggle.id = 'detailToggle';
    head.append(this.sum, tabs, this.toggle);
    this.body = el('div', 'dbody');
    this.body.tabIndex = 0;
    this.root.append(this.grip, head, this.body);
    this.setOpen(open, false);
    this.renderEmpty();
  }

  get open(): boolean {
    return !this.root.classList.contains('collapsed');
  }

  setOpen(open: boolean, notify = true): void {
    this.root.classList.toggle('collapsed', !open);
    this.toggle.textContent = open ? '詳細を隠す' : '詳細を表示';
    this.toggle.setAttribute('aria-expanded', String(open));
    if (notify) this.host.onToggle(open);
  }

  private setTab(t: Tab): void {
    this.tab = t;
    this.render();
  }

  clear(): void {
    this.memoSave?.();
    this.seq++;
    this.data = null;
    this.renderReview({ kind: 'same' } as DetailData);
    this.renderEmpty();
  }

  /** Shown record, if any. */
  get unit(): number | null {
    return this.data?.unit ?? null;
  }

  async load(unit: number): Promise<void> {
    // A memo being typed for the previous record is saved first.
    this.memoSave?.();
    const seq = ++this.seq;
    const t0 = performance.now();
    try {
      const d = await this.api.call<DetailData>('detail', { unit });
      if (seq !== this.seq) return;
      metrics.detail.push(performance.now() - t0);
      this.data = d;
      this.render();
    } catch (e) {
      if (seq !== this.seq) return;
      this.sum.textContent = (e as Error).message;
    }
  }

  private renderEmpty(): void {
    this.sum.replaceChildren(el('span', 'muted', 'レコードを選ぶと、ここに値の違いを表示します。'));
    this.body.replaceChildren();
    this.tabs.forEach((b) => (b.disabled = true));
  }

  render(): void {
    const d = this.data;
    if (!d) return this.renderEmpty();
    const changed = d.cells.filter((c) => c.d === 1).length;
    const ignored = d.cells.filter((c) => c.d === 2).length;
    const kind = el('span', `kind ${d.kind}`);
    kind.append(el('span', 'mk', MARK[d.kind]), document.createTextNode(KIND[d.kind]));
    const key = el('span', 'key', d.key.join(' / '));
    key.title = this.host.keyNames.map((n, i) => `${n}: ${d.key[i] ?? ''}`).join('\n');
    const parts: Node[] = [kind, key];
    if (d.kind === 'changed') parts.push(el('span', '', `${changed}列`));
    if (ignored) {
      const ig = el('span', 'muted', `比較条件で無視した差 ${ignored}列`);
      ig.title = '比較しない列や前後の空白の違いです。変更としては数えていません。';
      parts.push(ig);
    }
    this.sum.replaceChildren(...parts);
    this.renderReview(d);

    const oneSided = d.kind === 'added' || d.kind === 'deleted';
    this.tabs.forEach((b, t) => {
      b.disabled = t === 'changes' && oneSided;
      b.hidden = t === 'changes' && oneSided;
    });
    const tab: Tab = this.tab === 'changes' && oneSided ? 'all' : this.tab;
    this.tabs.forEach((b, t) => b.setAttribute('aria-selected', String(t === tab)));
    const content = tab === 'raw' ? this.raw(d) : this.table(d, tab === 'changes');
    this.body.replaceChildren(...(this.memoRow ? [this.memoRow, content] : [content]));
  }

  private reviewSeg: HTMLElement | null = null;
  private memoRow: HTMLElement | null = null;
  private memoBox: HTMLTextAreaElement | null = null;
  private memoTimer = 0;
  private memoSave: (() => void) | null = null;

  /** Review state buttons and the memo, for difference records when reviews are on. */
  private renderReview(d: DetailData): void {
    this.reviewSeg?.remove();
    this.reviewSeg = null;
    this.memoRow = null;
    this.memoBox = null;
    this.memoSave = null;
    if (!this.host.reviewEnabled || d.kind === 'same') return;
    const cur = d.review?.status ?? 'unchecked';
    const seg = el('div', 'seg rseg');
    seg.setAttribute('role', 'group');
    seg.setAttribute('aria-label', '確認状況');
    for (const [st, label, key] of STATUS) {
      const b = button(label, () => this.host.onStatus(d.unit, st), {
        title: `${label}にする（${key}）` + (st === 'checked' ? '\nSpaceキーで、確認済みにして次の未確認へ進みます' : ''),
      });
      b.dataset.status = st;
      b.setAttribute('aria-pressed', String(st === cur));
      seg.append(b);
    }
    this.toggle.before(seg);
    this.reviewSeg = seg;
    const row = el('div', 'memorow');
    const box = el('textarea', 'memo');
    box.rows = 1;
    box.placeholder = 'メモ（M）';
    box.setAttribute('aria-label', 'メモ');
    box.value = d.review?.memo ?? '';
    const save = () => {
      clearTimeout(this.memoTimer);
      if (box.value !== (d.review?.memo ?? '')) {
        d.review = { status: cur, memo: box.value, at: Date.now() };
        this.host.onMemo(d.unit, box.value);
      }
    };
    box.addEventListener('input', () => {
      clearTimeout(this.memoTimer);
      this.memoTimer = window.setTimeout(save, 800);
    });
    box.addEventListener('blur', save);
    box.addEventListener('keydown', (e) => {
      e.stopPropagation();
      if (e.key === 'Escape') {
        save();
        box.blur();
      }
    });
    row.append(box);
    this.memoRow = row;
    this.memoBox = box;
    this.memoSave = save;
  }

  /** Moves the focus to the memo (opening the panel if needed). */
  focusMemo(): void {
    if (!this.open) this.setOpen(true);
    this.memoBox?.focus();
  }

  private raw(d: DetailData): HTMLElement {
    const grid = el('div', 'rawgrid');
    for (const [label, r] of [
      ['左', d.left],
      ['右', d.right],
    ] as const) {
      const box = el('div', 'rawbox');
      if (r) {
        const h = el('div', 'rawhead', `${label}　${lines(r)}`);
        h.title = `データの${fmt(r.record)}件目、ファイルの${fmt(r.byte_start)}〜${fmt(r.byte_end)}バイト`;
        box.append(h);
        if (r.raw_cut) box.append(cutNote('行の全体は、ファイルをテキストエディターで開いて、見出しの行番号で確かめられます。'));
        box.append(el('pre', '', blockText(r.raw, this.host.showSpaces)));
      } else {
        box.append(el('div', 'rawhead', label), el('div', 'none', '対応するレコードはありません'));
      }
      grid.append(box);
    }
    return grid;
  }

  private table(d: DetailData, changesOnly: boolean): HTMLElement {
    const cells = changesOnly ? d.cells.filter((c) => c.d !== 0) : d.cells;
    const table = el('table', 'dtable');
    const head = el('tr');
    head.append(el('th', 'colname', '列'), el('th', '', '左'), el('th', '', '右'));
    table.append(head);
    for (const c of cells) {
      const tr = el('tr');
      if (c.d === 1) tr.className = 'chg';
      if (c.d === 2) tr.className = 'ign';
      const name = c.name_left ?? c.name_right ?? '';
      const th = el('th', 'colname', name);
      const notes: string[] = [];
      if (c.key) notes.push('キーの列');
      if (c.d === 2)
        notes.push(c.excluded ? '比較しない列のため、変更として数えていません' : '前後の空白の違いのため、変更として数えていません');
      if (notes.length) {
        th.append(el('span', 'note', c.key ? 'キー' : '無視'));
        th.title = notes.join('\n');
      }
      tr.append(th);
      const segs = c.d !== 0 && c.left !== null && c.right !== null ? diffChars(c.left, c.right) : null;
      tr.append(
        this.valueCell(c.left, c.left_cut, c.name_left !== null, d.left !== null, segs, 'removed'),
        this.valueCell(c.right, c.right_cut, c.name_right !== null, d.right !== null, segs, 'added'),
      );
      table.append(tr);
    }
    if (!cells.length) {
      const tr = el('tr');
      const td = el('td', 'none', '値が違う列はありません。');
      td.colSpan = 3;
      tr.append(td);
      table.append(tr);
    }
    return table;
  }

  private valueCell(
    v: string | null,
    cut: boolean,
    hasColumn: boolean,
    hasRecord: boolean,
    segs: ReturnType<typeof diffChars> | null,
    mine: 'removed' | 'added',
  ): HTMLTableCellElement {
    if (!hasRecord) return el('td', 'none', '対応するレコードなし');
    if (!hasColumn || v === null) return el('td', 'none', 'この列はありません');
    if (v === '') return el('td', 'none', '（空）');
    const td = el('td', 'val');
    // Above the value, where it is seen without scrolling through it.
    if (cut) td.append(cutNote('値の全体は、表でセルを選んで Ctrl+C でコピーできます。'));
    if (!segs) td.append(document.createTextNode(blockText(v, this.host.showSpaces)));
    else
      for (const s of segs) {
        if (s.kind === 'same') td.append(document.createTextNode(blockText(s.text, this.host.showSpaces)));
        else if (s.kind === mine) td.append(el(mine === 'removed' ? 'del' : 'ins', '', blockText(s.text, true)));
      }
    return td;
  }
}

/** Under a value the detail shows only the start of. */
function cutNote(title: string): HTMLElement {
  const n = el('div', 'muted small', '長いため、先頭だけを表示しています。');
  n.title = title;
  return n;
}
