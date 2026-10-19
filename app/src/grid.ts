// Virtualised grid. Rows and columns are virtualised; only the rows and
// columns on screen (plus a margin) are requested from the engine. Views
// taller than MAX_SPACER pixels are mapped onto a scaled scrollbar.

import type { Backend } from './api';
import type { SortSpec } from './settings';
import { cellText, el, fmt, measureTexts } from './util';

export const ROW_H = 22; // keep in sync with --row-h in style.css
const MAX_SPACER = 8_000_000;
const OVERSCAN = 8;
const COL_PAD = 6;
const W_MARK = 22;
const W_NUM = 68;
const W_REV = 22;
export const REVIEW_MARK = ['', '✓', '?'];
export const REVIEW_LABEL = ['未確認', '確認済み', '要調査'];
const W_MIN = 56;
const W_MAX = 240;

export type Side = 'left' | 'right';
/** `added`: the record is only in the right file; `deleted`: only in the
 *  left one. Shown as 右のみ and 左のみ: the files need not be an old and a
 *  new version of each other. */
export type Kind = 'same' | 'changed' | 'added' | 'deleted';
export const KIND: Record<Kind, string> = { same: '一致', changed: '変更', added: '右のみ', deleted: '左のみ' };
export const MARK: Record<Kind, string> = { same: '=', changed: '~', added: '>', deleted: '<' };

export interface Slot {
  left: number | null;
  right: number | null;
  pair: number | null;
  key: boolean;
  excluded: boolean;
  name_left: string | null;
  name_right: string | null;
}

export interface Cell {
  v: string | null;
  cut?: boolean;
  d?: number;
}

export interface Rec {
  record: number;
  line: number;
  line_end: number;
  cells: Cell[];
}

export interface Row {
  row: number;
  unit: number;
  kind: Kind;
  ignored?: boolean;
  /** Review state (0 未確認, 1 確認済み, 2 要調査) when set. */
  review?: number;
  memo?: boolean;
  left: Rec | null;
  right: Rec | null;
}

export const metrics = {
  fetch: [] as number[],
  scrollPaint: [] as number[],
  navPaint: [] as number[],
  detail: [] as number[],
  viewOpen: [] as number[],
  firstPaint: null as number | null,
  readyAt: null as number | null,
};

/** One engine view (an ordered, filtered list of rows) and its row cache. */
export class Model {
  id = 0;
  total = 0;
  diffCount = 0;
  slots: Slot[] = [];
  pinned: number[] = [];
  cache = new Map<number, Row>();
  win: [number, number] | null = null;
  gen = 0;
  inflight = false;
  listeners = new Set<(resized: boolean) => void>();
  widths = new Map<number, number>();
  /** Widths set by the user (dragging a column edge). */
  fixed = new Map<number, number>();
  headerWidths: number[] | null = null;
  private sized = false;
  private resized = false;

  constructor(
    private api: Backend,
    readonly order: 'aligned' | 'left_source' | 'right_source',
  ) {}

  async open(filter: object, sort: SortSpec | null, extra: object = {}): Promise<void> {
    const t0 = performance.now();
    const r = await this.api.call<{ view: number; total: number; diff_count: number; slots: Slot[] }>('view', {
      order: this.order,
      filter,
      sort: sort ?? { by: 'unit', desc: false },
      ...extra,
    });
    metrics.viewOpen.push(performance.now() - t0);
    this.id = r.view;
    this.total = r.total;
    this.diffCount = r.diff_count;
    this.slots = r.slots;
    this.cache.clear();
    this.ver.clear();
    this.win = null;
    this.gen++;
    this.pinned = this.order === 'aligned' ? this.slots.flatMap((s, i) => (s.key ? [i] : [])) : [];
  }

  /** Rows fetched before the last `refresh` are shown until fetched again. */
  private version = 0;
  private ver = new Map<number, number>();

  /** Fetches the rows again (their review states changed) without blanking them. */
  refresh(): void {
    this.version++;
    this.listeners.forEach((f) => f(false));
  }

  covers(c0: number, c1: number): boolean {
    return !!this.win && this.win[0] <= c0 && c1 <= this.win[1];
  }

  /** Makes sure rows [r0, r1) are cached for slots [c0, c1). */
  ensure(r0: number, r1: number, c0: number, c1: number): void {
    if (!this.covers(c0, c1)) {
      const n = this.slots.length;
      this.win = [Math.max(0, c0 - COL_PAD), Math.min(n, c1 + COL_PAD)];
      this.cache.clear();
      this.ver.clear();
      this.gen++;
    }
    if (this.inflight) return;
    let a = -1;
    let b = -1;
    for (let r = Math.max(0, r0); r < Math.min(this.total, r1); r++) {
      if (!this.cache.has(r) || this.ver.get(r) !== this.version) {
        if (a < 0) a = r;
        b = r + 1;
      }
    }
    if (a < 0) return;
    a = Math.max(0, a - OVERSCAN);
    b = Math.min(this.total, b + OVERSCAN, a + 400);
    const gen = this.gen;
    const version = this.version;
    const win = this.win!;
    this.inflight = true;
    const t0 = performance.now();
    this.api
      .call<{ rows: Row[] }>('rows', {
        view: this.id,
        start: a,
        count: b - a,
        col_start: win[0],
        col_count: win[1] - win[0],
        pinned: this.pinned,
        max_chars: 200,
      })
      .then((resp) => {
        metrics.fetch.push(performance.now() - t0);
        if (gen !== this.gen) return;
        for (const row of resp.rows) {
          this.cache.set(row.row, row);
          this.ver.set(row.row, version);
        }
        if (!this.sized) {
          this.autoSize(resp.rows);
          this.resized = true;
        }
        if (this.cache.size > 4000) {
          for (const k of this.cache.keys())
            if (k < a - 1000 || k > b + 1000) {
              this.cache.delete(k);
              this.ver.delete(k);
            }
        }
      })
      .catch(() => undefined)
      .finally(() => {
        this.inflight = false;
        const resized = this.resized;
        this.resized = false;
        this.listeners.forEach((f) => f(resized));
      });
  }

  /** Column widths from the header and the values of the first rows fetched
   * (decided once, so columns do not move while scrolling). */
  private autoSize(rows: Row[]): void {
    const slots: number[] = [];
    const texts: string[] = [];
    for (const row of rows) {
      for (const rec of [row.left, row.right]) {
        if (!rec) continue;
        rec.cells.forEach((cell, k) => {
          if (!cell || cell.v === null || texts.length > 4000) return;
          slots.push(k < this.pinned.length ? this.pinned[k] : this.win![0] + (k - this.pinned.length));
          texts.push(cellText(cell.v, false) + (cell.cut ? '…' : ''));
        });
      }
    }
    measureTexts(texts, false).forEach((w, i) => {
      if (w > (this.widths.get(slots[i]) ?? 0)) this.widths.set(slots[i], w);
    });
    this.sized = true;
  }

  cell(rec: Rec, s: number): Cell | undefined {
    const pi = this.pinned.indexOf(s);
    if (pi >= 0) return rec.cells[pi];
    if (!this.win) return undefined;
    return rec.cells[this.pinned.length + (s - this.win[0])];
  }
}

export interface Range {
  r0: number;
  r1: number;
  c0: number;
  c1: number;
}

/** Cell selection inside one pane, in display coordinates. */
export class Selection {
  anchor = { row: 0, col: 0 };
  focus = { row: 0, col: 0 };
  active = false;
  /** Whole rows (from the anchor's row to the focus's), whole columns, or
   *  the whole table, however many columns or rows there are. The focus
   *  stays the cell cursor. */
  whole: 'rows' | 'cols' | 'all' | null = null;

  constructor(private size: () => { rows: number; cols: number } = () => ({ rows: 0, cols: 0 })) {}

  /** The whole table or whole columns are selected: copies include the column names. */
  get withHeader(): boolean {
    return this.whole === 'cols' || this.whole === 'all';
  }

  get range(): Range {
    const g = {
      r0: Math.min(this.anchor.row, this.focus.row),
      r1: Math.max(this.anchor.row, this.focus.row),
      c0: Math.min(this.anchor.col, this.focus.col),
      c1: Math.max(this.anchor.col, this.focus.col),
    };
    const { rows, cols } = this.size();
    if (this.whole === 'rows' || this.whole === 'all') [g.c0, g.c1] = [0, Math.max(0, cols - 1)];
    if (this.whole === 'cols' || this.whole === 'all') [g.r0, g.r1] = [0, Math.max(0, rows - 1)];
    return g;
  }

  /** Moves the cursor; with `extend`, the selection grows to it (whole rows
   *  or columns stay whole). */
  set(row: number, col: number, extend: boolean): void {
    if (!extend) {
      this.anchor = { row, col };
      this.whole = null;
    } else if (this.whole === 'all') {
      this.whole = null;
    }
    this.focus = { row, col };
    this.active = true;
  }

  /** Selects the whole rows or columns of the selection (the cursor's when
   *  there is none); both together are the whole table. */
  widen(whole: 'rows' | 'cols' | 'all', at: { row: number; col: number }): void {
    if (!this.active) this.set(at.row, at.col, false);
    const both = (this.whole === 'rows' && whole === 'cols') || (this.whole === 'cols' && whole === 'rows');
    this.whole = both ? 'all' : whole;
  }
  contains(row: number, col: number): boolean {
    if (!this.active) return false;
    const g = this.range;
    return row >= g.r0 && row <= g.r1 && col >= g.c0 && col <= g.c1;
  }
}

export interface PaneHost {
  order: 'aligned' | 'source';
  showSpaces: boolean;
  /** Show the review state column. */
  reviewEnabled: boolean;
  /** Text being searched for; cells containing it are marked. */
  findText: string;
  slotVisible(slot: Slot): boolean;
  /** Units in which the column's value changed / differed only in an ignored way. */
  changeCount(slot: Slot): { changed: number; ignored: number } | null;
  sort: SortSpec | null;
  selectedUnit(): number | null;
  peer(p: Pane): Pane;
  onRowsPainted(p: Pane): void;
  onCellDown(p: Pane, row: number, col: number, e: PointerEvent): void;
  onCellDrag(p: Pane, row: number, col: number): void;
  /** A column name was clicked (with Shift: `extend`). */
  onHeaderClick(p: Pane, slot: number, extend: boolean): void;
  onColumnResized(p: Pane): void;
  /** The top-left corner of the table was clicked. */
  onCornerClick(p: Pane): void;
  onContextMenu(p: Pane, e: MouseEvent, where: 'cell' | 'header', slot: number | null): void;
  onFocus(p: Pane): void;
}

interface Col {
  i: number;
  w: number;
  x: number;
}

export class Pane {
  readonly root: HTMLElement;
  readonly scroller: HTMLDivElement;
  private layer: HTMLDivElement;
  private hrow: HTMLDivElement;
  readonly rowsEl: HTMLDivElement;
  private spacer: HTMLDivElement;
  model: Model | null = null;
  top = 0;
  complete = false;
  sel = new Selection(() => this.size());
  /** Display order of slots: frozen (key) slots, then the scrollable ones. */
  order: number[] = [];
  private frozen: { i: number; w: number }[] = [];
  private frozenW = W_MARK + W_NUM;
  private cols: Col[] = [];
  private totalW = 0;
  private hdrWin: [number, number] | null = null;
  private expectTop: number | null = null;
  private expectLeft: number | null = null;
  scrollT0: number | null = null;
  private raf = 0;
  waiter: (() => void) | null = null;
  private dragging = false;
  private dragPoint = { x: 0, y: 0 };
  private autoScroll = 0;
  private afterResize = false;

  constructor(
    readonly side: Side,
    private host: PaneHost,
  ) {
    this.root = el('section', 'pane');
    this.root.id = side === 'left' ? 'paneL' : 'paneR';
    this.root.setAttribute('aria-label', side === 'left' ? '比較元（左）' : '比較先（右）');
    this.scroller = el('div', 'scroller');
    this.scroller.tabIndex = 0;
    this.layer = el('div', 'layer');
    this.hrow = el('div', 'hrow');
    this.rowsEl = el('div', 'rows');
    this.spacer = el('div', 'spacer');
    this.layer.append(this.hrow, this.rowsEl);
    this.scroller.append(this.layer, this.spacer);
    this.root.append(this.scroller);
    this.scroller.addEventListener('scroll', () => this.onScroll());
    this.scroller.addEventListener('wheel', (e) => this.onWheel(e), { passive: false });
    this.scroller.addEventListener('focus', () => host.onFocus(this));
    this.rowsEl.addEventListener('pointerdown', (e) => this.onPointerDown(e));
    this.rowsEl.addEventListener('pointermove', (e) => this.onPointerMove(e));
    this.rowsEl.addEventListener('pointerup', () => this.endDrag());
    this.rowsEl.addEventListener('pointercancel', () => this.endDrag());
    this.rowsEl.addEventListener('contextmenu', (e) => {
      e.preventDefault();
      host.onContextMenu(this, e, 'cell', null);
    });
    this.hrow.addEventListener('click', (e) => {
      // The click that ends a column resize does not select the column.
      if (this.afterResize || (e.target as HTMLElement).closest('.rz')) {
        this.afterResize = false;
        return;
      }
      if ((e.target as HTMLElement).closest('.c.mk, .c.num')) {
        host.onCornerClick(this);
        return;
      }
      const c = (e.target as HTMLElement).closest<HTMLElement>('.c[data-slot]');
      if (c) host.onHeaderClick(this, Number(c.dataset.slot), e.shiftKey);
    });
    this.hrow.addEventListener('pointerdown', (e) => this.startResize(e));
    this.hrow.addEventListener('dblclick', (e) => {
      const rz = (e.target as HTMLElement).closest<HTMLElement>('.rz');
      if (rz) this.fitColumn(Number(rz.dataset.slot));
    });
    this.hrow.addEventListener('contextmenu', (e) => {
      e.preventDefault();
      const c = (e.target as HTMLElement).closest<HTMLElement>('.c[data-slot]');
      host.onContextMenu(this, e, 'header', c ? Number(c.dataset.slot) : null);
    });
  }

  setModel(m: Model): void {
    this.model = m;
    this.top = 0;
    this.complete = false;
    this.sel = new Selection(() => this.size());
    m.listeners.add((resized) => (resized ? this.layoutColumns() : this.schedule()));
    this.layout();
  }

  private size(): { rows: number; cols: number } {
    return { rows: this.model?.total ?? 0, cols: this.colCount };
  }

  get visibleRows(): number {
    return Math.max(1, Math.floor((this.scroller.clientHeight - ROW_H) / ROW_H));
  }
  get maxTop(): number {
    return Math.max(0, (this.model?.total ?? 0) - this.visibleRows);
  }
  get spacerH(): number {
    return Math.min(MAX_SPACER, this.maxTop * ROW_H);
  }
  get scaled(): boolean {
    return this.maxTop * ROW_H > MAX_SPACER;
  }
  /** Number of display columns (frozen + visible scrollable). */
  get colCount(): number {
    return this.order.length;
  }

  layout(): void {
    this.layoutColumns();
    this.spacer.style.height = this.spacerH + 'px';
    this.expectTop = null;
    this.scroller.scrollTop = 0;
  }

  name(slot: Slot): string {
    return (this.side === 'left' ? slot.name_left : slot.name_right) ?? slot.name_left ?? slot.name_right ?? '';
  }

  layoutColumns(): void {
    const m = this.model;
    if (!m) return;
    if (!m.headerWidths)
      m.headerWidths = measureTexts(
        m.slots.map((s) => this.name(s)),
        true,
      ).map((w) => w + 12);
    const frozen: number[] = [];
    const scroll: number[] = [];
    m.slots.forEach((s, i) => {
      if (m.order === 'aligned' && s.key) frozen.push(i);
      else if (this.host.slotVisible(s)) scroll.push(i);
    });
    const width = (i: number) => m.fixed.get(i) ?? Math.min(W_MAX, Math.max(W_MIN, m.headerWidths![i], m.widths.get(i) ?? 0));
    this.frozen = frozen.map((i) => ({ i, w: width(i) }));
    let x = this.lead + this.frozen.reduce((a, c) => a + c.w, 0);
    this.frozenW = x;
    this.cols = scroll.map((i) => {
      const c = { i, w: width(i), x };
      x += c.w;
      return c;
    });
    this.totalW = x;
    this.order = [...frozen, ...scroll];
    this.layer.style.width = this.totalW + 'px';
    this.hdrWin = null;
    this.schedule();
  }

  private get revW(): number {
    return this.host.reviewEnabled ? W_REV : 0;
  }

  /** Width of the columns before the values (mark, review state, line). */
  private get lead(): number {
    return W_MARK + this.revW + W_NUM;
  }

  /** Width of a slot's column as laid out now. */
  private colWidth(slot: number): number {
    return this.frozen.find((f) => f.i === slot)?.w ?? this.cols.find((c) => c.i === slot)?.w ?? W_MIN;
  }

  private startResize(e: PointerEvent): void {
    const rz = (e.target as HTMLElement).closest<HTMLElement>('.rz');
    const m = this.model;
    if (!rz || !m || e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    const slot = Number(rz.dataset.slot);
    const x0 = e.clientX;
    const w0 = this.colWidth(slot);
    let raf = 0;
    const move = (ev: PointerEvent) => {
      m.fixed.set(slot, Math.round(Math.min(2000, Math.max(32, w0 + ev.clientX - x0))));
      if (!raf)
        raf = requestAnimationFrame(() => {
          raf = 0;
          this.host.onColumnResized(this);
        });
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      this.afterResize = true;
      setTimeout(() => (this.afterResize = false), 0);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  }

  /** Widens or narrows a column to the longest value among the rows fetched so far. */
  private fitColumn(slot: number): void {
    const m = this.model;
    if (!m) return;
    const texts: string[] = [];
    for (const row of m.cache.values()) {
      for (const rec of [row.left, row.right]) {
        const cell = rec ? m.cell(rec, slot) : undefined;
        if (cell && cell.v) texts.push(cellText(cell.v, false) + (cell.cut ? '…' : ''));
      }
      if (texts.length > 4000) break;
    }
    const w = Math.max(W_MIN, m.headerWidths?.[slot] ?? 0, ...measureTexts(texts, false));
    m.fixed.set(slot, Math.min(800, w));
    this.host.onColumnResized(this);
  }

  private colWindow(): [number, number] {
    const left = this.scroller.scrollLeft + this.frozenW;
    const right = this.scroller.scrollLeft + this.scroller.clientWidth;
    let a = this.cols.findIndex((c) => c.x + c.w > left);
    if (a < 0) a = this.cols.length;
    let b = a;
    while (b < this.cols.length && this.cols[b].x < right) b++;
    return [a, b];
  }

  private topFromScroll(): number {
    const st = this.scroller.scrollTop;
    if (!this.scaled) return st / ROW_H;
    return this.spacerH ? (st / this.spacerH) * this.maxTop : 0;
  }
  private scrollFromTop(t: number): number {
    return this.scaled ? Math.round((t / this.maxTop) * this.spacerH) : Math.round(t * ROW_H);
  }

  setTop(t: number, fromPeer = false): void {
    this.top = Math.max(0, Math.min(this.maxTop, t));
    const st = this.scrollFromTop(this.top);
    this.expectTop = st;
    this.scroller.scrollTop = st;
    if (!fromPeer && this.host.order === 'aligned') this.host.peer(this).setTop(this.top, true);
    this.schedule();
  }

  private onScroll(): void {
    const st = this.scroller.scrollTop;
    const sl = this.scroller.scrollLeft;
    if (this.expectTop === null || Math.abs(st - this.expectTop) > 1) {
      this.top = this.topFromScroll();
      this.expectTop = null;
      if (this.scrollT0 === null) this.scrollT0 = performance.now();
      if (this.host.order === 'aligned') this.host.peer(this).setTop(this.top, true);
    }
    if (this.host.order === 'aligned' && (this.expectLeft === null || Math.abs(sl - this.expectLeft) > 1)) {
      const p = this.host.peer(this);
      p.expectLeft = sl;
      p.scroller.scrollLeft = sl;
    }
    this.expectLeft = null;
    this.schedule();
  }

  private onWheel(e: WheelEvent): void {
    // Ctrl zooms; Shift scrolls sideways (the browser does both).
    if (!this.scaled || e.ctrlKey || e.shiftKey) return;
    e.preventDefault();
    const unit = e.deltaMode === 1 ? 1 : e.deltaMode === 2 ? this.visibleRows : 1 / ROW_H;
    if (this.scrollT0 === null) this.scrollT0 = performance.now();
    this.setTop(this.top + e.deltaY * unit);
    if (e.deltaX) this.scroller.scrollLeft += e.deltaX;
  }

  private hit(e: PointerEvent | MouseEvent): { row: number; col: number } | null {
    const target = e.target as HTMLElement;
    const rowEl = target.closest<HTMLElement>('.row');
    const cellEl = target.closest<HTMLElement>('.c');
    if (!rowEl || rowEl.dataset.row === undefined) return null;
    const col = cellEl?.dataset.col !== undefined ? Number(cellEl.dataset.col) : -1;
    return { row: Number(rowEl.dataset.row), col };
  }

  private onPointerDown(e: PointerEvent): void {
    if (e.button !== 0) return;
    const h = this.hit(e);
    if (!h) return;
    this.dragging = true;
    this.dragPoint = { x: e.clientX, y: e.clientY };
    this.rowsEl.setPointerCapture(e.pointerId);
    this.host.onCellDown(this, h.row, h.col, e);
  }

  private onPointerMove(e: PointerEvent): void {
    if (!this.dragging) return;
    this.dragPoint = { x: e.clientX, y: e.clientY };
    this.dragTo();
    // Past an edge, the table scrolls by itself while the button is held.
    if (!this.autoScroll && this.edgeDelta().some((d) => d !== 0)) {
      this.autoScroll = window.setInterval(() => {
        const [dx, dy] = this.edgeDelta();
        if (!this.dragging || (dx === 0 && dy === 0)) return this.stopAutoScroll();
        if (dy) this.setTop(this.top + dy);
        if (dx) this.scroller.scrollLeft += dx * 24;
        this.dragTo();
      }, 50);
    }
  }

  private endDrag(): void {
    this.dragging = false;
    this.stopAutoScroll();
  }

  private stopAutoScroll(): void {
    clearInterval(this.autoScroll);
    this.autoScroll = 0;
  }

  /** Rows (and column steps) to scroll by when the pointer is past an edge. */
  private edgeDelta(): [number, number] {
    const r = this.scroller.getBoundingClientRect();
    const { x, y } = this.dragPoint;
    const step = (d: number) => Math.max(1, Math.min(20, Math.round(d / 8)));
    const dy = y < r.top + ROW_H ? -step(r.top + ROW_H - y) : y > r.bottom ? step(y - r.bottom) : 0;
    const dx = x < r.left + this.frozenW ? -1 : x > r.right ? 1 : 0;
    return [dx, dy];
  }

  /** Extends the selection to the cell under the pointer (clamped to the table). */
  private dragTo(): void {
    const m = this.model;
    if (!m || !m.total) return;
    const r = this.scroller.getBoundingClientRect();
    const y = this.dragPoint.y - r.top - ROW_H;
    const row = Math.max(0, Math.min(m.total - 1, Math.floor(this.top + y / ROW_H)));
    const x = this.dragPoint.x - r.left;
    let col = -1;
    if (x >= this.lead) {
      if (x < this.frozenW) {
        let left = this.lead;
        col = this.frozen.findIndex((f) => {
          left += f.w;
          return x < left;
        });
      } else {
        const ax = x + this.scroller.scrollLeft;
        const k = this.cols.findIndex((c) => ax < c.x + c.w);
        col = this.frozen.length + (k < 0 ? this.cols.length - 1 : k);
      }
    }
    this.host.onCellDrag(this, row, col < 0 ? this.sel.focus.col : col);
  }

  schedule(): void {
    if (!this.raf) {
      this.raf = requestAnimationFrame(() => {
        this.raf = 0;
        this.render();
      });
    }
  }

  private fz(cls: string, w: number, left: number, text: string): HTMLDivElement {
    const c = el('div', 'c fz ' + cls, text);
    c.style.width = w + 'px';
    c.style.left = left + 'px';
    return c;
  }

  private renderHeader(a: number, b: number): void {
    const m = this.model!;
    const frag = document.createDocumentFragment();
    const sort = this.host.sort;
    const mark = (i: number) =>
      sort && sort.by === 'column' && sort.slot === i && (sort.side ?? 'left') === this.side ? (sort.desc ? ' ▼' : ' ▲') : '';
    const head = (s: Slot, i: number, cls: string, w: number) => {
      const c = el('div', 'c ' + cls, this.name(s) + mark(i));
      c.dataset.slot = String(i);
      const rz = el('div', 'rz');
      rz.dataset.slot = String(i);
      rz.title = 'ドラッグで幅を変えます。ダブルクリックで値に合わせます。';
      c.append(rz);
      c.style.width = w + 'px';
      c.title = this.colTitle(s);
      if (this.colOf(s) === null) c.classList.add('nocol');
      if ((this.host.changeCount(s)?.changed ?? 0) > 0) c.classList.add('haschg');
      return c;
    };
    const cornerTitle = '行番号はファイルでの行です。\nクリックすると表全体を選択します（Ctrl+Cで列名付きでコピーできます）。';
    const corner = [this.fz('mk corner', W_MARK, 0, '')];
    if (this.revW) corner.push(this.fz('rv corner', W_REV, W_MARK, ''));
    corner.push(this.fz('num corner', W_NUM, W_MARK + this.revW, '行'));
    corner.forEach((c) => (c.title = cornerTitle));
    if (this.revW) corner[1].title = '確認状況（✓ 確認済み、? 要調査）\n' + cornerTitle;
    frag.append(...corner);
    let left = this.lead;
    this.frozen.forEach((f, k) => {
      const c = head(m.slots[f.i], f.i, 'fz key' + (k === this.frozen.length - 1 ? ' lastfz' : ''), f.w);
      c.style.left = left + 'px';
      left += f.w;
      frag.append(c);
    });
    if (!this.frozen.length) (frag.lastChild as HTMLElement).classList.add('lastfz');
    if (a < this.cols.length) {
      const pad = el('div', 'pad');
      pad.style.width = this.cols[a].x - this.frozenW + 'px';
      frag.append(pad);
    }
    for (let k = a; k < b; k++) frag.append(head(m.slots[this.cols[k].i], this.cols[k].i, '', this.cols[k].w));
    this.hrow.style.width = this.totalW + 'px';
    this.hrow.replaceChildren(frag);
    this.hdrWin = [a, b];
  }

  colOf(slot: Slot): number | null {
    return this.side === 'left' ? slot.left : slot.right;
  }

  private colTitle(s: Slot): string {
    const c = this.colOf(s);
    const n = this.name(s);
    if (c === null) return `「${n}」は${this.side === 'left' ? '左' : '右'}のファイルにありません`;
    const parts = [`${n}（${this.side === 'left' ? '左' : '右'}のファイルの${c + 1}列目）`];
    if (s.key) parts.push('キーの列');
    if (s.excluded) parts.push('比較しない列（差は記録だけします）');
    if (s.pair === null) parts.push(`${this.side === 'left' ? '右' : '左'}のファイルにはない列`);
    const cc = this.host.changeCount(s);
    if (cc && cc.changed) parts.push(`値が変わったレコード ${fmt(cc.changed)}件`);
    if (cc && cc.ignored) parts.push(`比較条件で無視した差 ${fmt(cc.ignored)}件`);
    parts.push(
      this.host.order === 'aligned' ? 'クリックで列を選びます。並べ替えは右クリックのメニューにあります。' : 'クリックで列を選びます。',
    );
    return parts.join('\n');
  }

  render(): void {
    const m = this.model;
    if (!m) return;
    const [a, b] = this.colWindow();
    if (!this.hdrWin || this.hdrWin[0] !== a || this.hdrWin[1] !== b) this.renderHeader(a, b);
    const first = Math.floor(this.top);
    const n = this.visibleRows + 1;
    const c0 = a < this.cols.length ? this.cols[a].i : m.slots.length;
    const c1 = b > a ? this.cols[b - 1].i + 1 : c0;
    m.ensure(first - n, first + 2 * n, c0, c1);
    const frag = document.createDocumentFragment();
    let complete = true;
    for (let r = first; r < Math.min(m.total, first + n); r++) {
      const row = m.covers(c0, c1) ? m.cache.get(r) : undefined;
      if (!row) complete = false;
      frag.append(this.rowEl(r, row, a, b));
    }
    this.rowsEl.style.transform = `translateY(${-(this.top - first) * ROW_H}px)`;
    this.rowsEl.replaceChildren(frag);
    this.complete = complete;
    if (complete) {
      if (this.scrollT0 !== null) {
        const t0 = this.scrollT0;
        this.scrollT0 = null;
        requestAnimationFrame(() => metrics.scrollPaint.push(performance.now() - t0));
      }
      if (this.waiter) {
        const w = this.waiter;
        this.waiter = null;
        requestAnimationFrame(() => w());
      }
    }
    this.host.onRowsPainted(this);
  }

  private rowEl(r: number, row: Row | undefined, a: number, b: number): HTMLDivElement {
    const div = el('div', 'row');
    div.style.width = this.totalW + 'px';
    div.dataset.row = String(r);
    const rec = row ? (this.side === 'left' ? row.left : row.right) : null;
    if (!row) div.classList.add('pending');
    else {
      div.dataset.unit = String(row.unit);
      div.classList.add(row.kind);
      if (!rec) div.classList.add('absent');
      if (this.host.selectedUnit() === row.unit) div.classList.add('sel');
    }
    const mk = this.fz('mk', W_MARK, 0, row ? MARK[row.kind] : '');
    if (row) {
      mk.classList.add('mark', row.kind);
      mk.title = KIND[row.kind] + (row.ignored ? '（比較条件で無視した差があります）' : '');
    }
    const line = rec ? (rec.line === rec.line_end ? fmt(rec.line) : `${fmt(rec.line)}–`) : '';
    const num = this.fz('num', W_NUM, W_MARK + this.revW, line);
    num.dataset.col = '-1';
    if (rec && rec.line !== rec.line_end) num.title = `${fmt(rec.line)}〜${fmt(rec.line_end)}行目（値の中に改行があります）`;
    if (row && !rec) num.title = `${this.side === 'left' ? '左' : '右'}に対応するレコードはありません`;
    div.append(mk);
    if (this.revW) {
      const st = row?.review ?? 0;
      const rv = this.fz('rv', W_REV, W_MARK, row && row.kind !== 'same' ? REVIEW_MARK[st] : '');
      if (row && row.kind !== 'same') {
        rv.classList.add('st' + st);
        rv.title = REVIEW_LABEL[st] + (row.memo ? '\nメモがあります' : '');
        if (row.memo) rv.classList.add('hasmemo');
      }
      div.append(rv);
    }
    div.append(num);
    let left = this.lead;
    this.frozen.forEach((f, k) => {
      const c = this.cellEl(rec, f.i, f.w, r, k);
      c.classList.add('fz');
      if (k === this.frozen.length - 1) c.classList.add('lastfz');
      c.style.left = left + 'px';
      left += f.w;
      div.append(c);
    });
    if (!this.frozen.length) num.classList.add('lastfz');
    if (a < this.cols.length) {
      const pad = el('div');
      pad.style.flex = 'none';
      pad.style.width = this.cols[a].x - this.frozenW + 'px';
      div.append(pad);
    }
    for (let k = a; k < b; k++) div.append(this.cellEl(rec, this.cols[k].i, this.cols[k].w, r, this.frozen.length + k));
    return div;
  }

  private cellEl(rec: Rec | null, s: number, w: number, row: number, col: number): HTMLDivElement {
    const c = el('div', 'c v');
    c.style.width = w + 'px';
    c.dataset.col = String(col);
    if (this.sel.contains(row, col)) {
      c.classList.add('insel');
      if (this.sel.focus.row === row && this.sel.focus.col === col) c.classList.add('cursor');
    }
    if (!rec) return c;
    const cell = this.model!.cell(rec, s);
    if (!cell) return c;
    if (cell.v === null) {
      c.classList.add('nocol');
      return c;
    }
    c.textContent = cellText(cell.v, this.host.showSpaces) + (cell.cut ? '…' : '');
    const ft = this.host.findText;
    if (ft && cell.v.includes(ft)) c.classList.add('hit');
    if (cell.d === 1) c.classList.add('chg');
    else if (cell.d === 2) c.classList.add('ign');
    return c;
  }

  /** Scrolls so that `row` is visible. */
  ensureVisible(row: number): void {
    if (row < this.top || row > this.top + this.visibleRows - 1) this.setTop(row - Math.floor(this.visibleRows / 3));
    else this.schedule();
  }

  /** Scrolls horizontally so that display column `col` is visible. */
  ensureColVisible(col: number): void {
    const k = col - this.frozen.length;
    if (k < 0 || k >= this.cols.length) return;
    const c = this.cols[k];
    const left = this.scroller.scrollLeft + this.frozenW;
    const right = this.scroller.scrollLeft + this.scroller.clientWidth;
    if (c.x < left) this.scroller.scrollLeft = c.x - this.frozenW;
    else if (c.x + c.w > right) this.scroller.scrollLeft = c.x + c.w - this.scroller.clientWidth;
  }

  relayout(): void {
    this.spacer.style.height = this.spacerH + 'px';
    this.setTop(this.top, true);
  }
}
