// The comparison screen: two virtualised tables, the toolbar and the detail
// panel. All row data comes from the engine on demand.

import type { Backend } from './api';
import { DetailPanel } from './detail';
import { KIND, MARK, Model, Pane, metrics, type Kind, type PaneHost, type Side, type Slot } from './grid';
import { closeMenu, openMenu, Popover, toast, type MenuItem } from './menu';
import { showKeys } from './help';
import { fillExport, fillGo, fillView, type PanelHost } from './toolbar-panels';
import { ReviewActions } from './review-actions';
import { Sidebar, type GroupRef, type ReviewStatus } from './sidebar';
import { ZOOM_STEPS, type Layout, type Settings, type SortSpec } from './settings';
import { ENCODING_LABEL, type SideSummary, type Summary } from './types';
import { button, clamp, el, fmt, mib } from './util';

export type Scope = 'all' | 'diff' | Kind;
type ViewOrder = 'aligned' | 'left_source' | 'right_source';
const DIFF_KINDS: Kind[] = ['changed', 'added', 'deleted'];
const SIDE_NAME: Record<Side, string> = { left: '左', right: '右' };
type Direction = 'up' | 'down' | 'left' | 'right';
/** A key pressed on the tables. */
interface Press {
  k: string;
  sh: boolean;
  ctrl: boolean;
  alt: boolean;
}
const ARROW: Record<string, Direction> = { ArrowUp: 'up', ArrowDown: 'down', ArrowLeft: 'left', ArrowRight: 'right' };

export interface SelState {
  unit: number;
  rows: Partial<Record<Side, number | null>>;
}

export interface CompareHost {
  settings: Settings;
  saveSettings(): void;
  setZoom(z: number): void;
  onChangeConditions(): void;
}

function seg(attr: string, items: [string, string, string][], onPick: (v: string) => void): HTMLDivElement {
  const g = el('div', 'seg');
  g.setAttribute('role', 'group');
  for (const [v, label, title] of items) {
    const b = button(label, () => onPick(v), { title });
    b.setAttribute(attr, v);
    g.append(b);
  }
  return g;
}

function press(g: HTMLElement, attr: string, v: string | null): void {
  g.querySelectorAll<HTMLButtonElement>(`[${attr}]`).forEach((b) => b.setAttribute('aria-pressed', String(b.getAttribute(attr) === v)));
}

function fileTitle(s: SideSummary): string {
  const le = s.line_endings;
  const ends = [le.crlf && 'CRLF', le.lf && 'LF', le.cr && 'CR'].filter(Boolean).join('・') || 'なし';
  return [
    s.path ?? s.name,
    `${fmt(s.records)}件、${s.columns}列、${mib(s.bytes)}`,
    `文字コード ${ENCODING_LABEL[s.encoding]}${s.bom ? '（BOMあり）' : ''}、${s.layout}、改行 ${ends}`,
    s.sha256 ? `SHA-256 ${s.sha256}` : 'SHA-256 計算中',
  ].join('\n');
}

export class CompareScreen implements PaneHost {
  readonly root: HTMLElement;
  order: 'aligned' | 'source' = 'aligned';
  scope: Scope = 'all';
  private lastScope: 'all' | 'diff' = 'all';
  text = '';
  sort: SortSpec | null = null;
  hidden = new Set<string>();
  sel: SelState | null = null;
  active: Side = 'left';
  readonly panes: Record<Side, Pane>;
  private nav: { pane: Pane; row: number; t0: number } | null = null;
  private detail: DetailPanel;
  private work: HTMLDivElement;
  private grids: HTMLDivElement;
  private caps: Record<Side, HTMLDivElement>;
  private empties: Record<Side, HTMLDivElement>;
  private seq = 0;
  private widths: Record<ViewOrder, Map<number, number>> = { aligned: new Map(), left_source: new Map(), right_source: new Map() };
  private fixed: Record<ViewOrder, Map<number, number>> = { aligned: new Map(), left_source: new Map(), right_source: new Map() };
  private tb: {
    scope: HTMLElement;
    order: HTMLElement;
    kinds: HTMLElement;
    layout: HTMLElement;
    hidden: HTMLButtonElement;
    chips: HTMLElement;
    search: HTMLInputElement;
    hits: HTMLElement;
    narrow: HTMLButtonElement;
  };
  reviewFilter: ReviewStatus | null = null;
  group: GroupRef | null = null;
  /** Review states: marking, notes, undo, saving (`review-actions.ts`). */
  readonly reviews: ReviewActions;
  private sidebar: Sidebar;
  private main: HTMLDivElement;
  /** The text last searched for, and the matching cells of the current row. */
  private find: { text: string; row: number; side: Side; cells: { side: Side; col: number }[]; at: number } | null = null;
  private selInfo: Record<Side, HTMLSpanElement> = { left: el('span', 'csel'), right: el('span', 'csel') };
  private viewPop: Popover;
  private goPop: Popover;
  private exportPop: Popover;
  private ro: ResizeObserver;

  constructor(
    private api: Backend,
    private host: CompareHost,
    readonly summary: Summary,
  ) {
    this.root = el('main', 'compare');
    this.panes = { left: new Pane('left', this), right: new Pane('right', this) };
    const settings = host.settings;
    const me = this;
    this.reviews = new ReviewActions({
      api,
      settings,
      root: this.root,
      panes: this.panes,
      get active() {
        return me.active;
      },
      get sel() {
        return me.sel;
      },
      get detail() {
        return me.detail;
      },
      get sidebar() {
        return me.sidebar;
      },
      updateToolbar: () => this.updateToolbar(),
      goToRow: (p, row, t0) => {
        p.sel.set(row, p.sel.active ? p.sel.focus.col : 0, false);
        this.clearPeerCells(p);
        this.selectRow(p, row, t0);
      },
      onChangeConditions: () => host.onChangeConditions(),
    });
    this.detail = new DetailPanel(
      api,
      {
        get showSpaces() {
          return settings.showSpaces;
        },
        keyNames: summary.keys,
        get reviewEnabled() {
          return settings.reviewEnabled;
        },
        onToggle: (open) => this.onDetailToggle(open),
        onStatus: (unit, status) => this.reviews.set([unit], status, false),
        onMemo: (unit, memo) => this.reviews.setMemo(unit, memo),
      },
      settings.detailOpen,
    );
    this.detail.grip.addEventListener('pointerdown', (e) => this.resizeDetail(e));

    // Toolbar
    const bar = el('div', 'toolbar');
    const back = button('条件を変更', () => this.reviews.leave(), {
      title: 'ファイル・キー・比較の設定を変えて比較し直します。今の比較結果は閉じます。',
    });
    const cond = this.condition();
    const kinds = el('div', 'kinds');
    kinds.setAttribute('role', 'group');
    kinds.setAttribute('aria-label', '差分の種類で絞り込む');
    for (const k of ['changed', 'deleted', 'added', 'same'] as Kind[]) {
      const n = k === 'same' ? summary.counts.same : summary.counts[k];
      const b = button('', () => this.setScope(this.scope === k ? this.lastScope : k), {
        cls: 'kindbtn',
        title:
          `${KIND[k]} ${fmt(n)}件。クリックすると${KIND[k]}の行だけを表示します` +
          (k === 'same' && summary.counts.same_with_ignored
            ? `\nうち${fmt(summary.counts.same_with_ignored)}件は比較条件で無視した差があります`
            : ''),
      });
      b.dataset.kind = k;
      b.setAttribute('aria-label', `${KIND[k]} ${n}件`);
      b.append(el('span', `mk ${k}`, MARK[k]), el('span', 'klabel', KIND[k]), el('span', '', fmt(n)));
      kinds.append(b);
    }
    const scope = seg(
      'data-scope',
      [
        ['all', '全行', 'すべての行を表示します'],
        ['diff', '差分のみ', '変更・左のみ・右のみの行だけを表示します'],
      ],
      (v) => this.setScope(v as Scope),
    );
    const order = seg(
      'data-order',
      [
        ['aligned', 'キー整列', '同じキーのレコードを同じ高さに並べます。左右のスクロールは連動します。'],
        ['source', '元ファイル順', '左右それぞれのファイルの並び順で表示します。左右のスクロールは連動しません。'],
      ],
      (v) => this.setOrder(v as 'aligned' | 'source'),
    );
    const nav = el('div', 'seg');
    nav.append(
      button('前の差分', () => this.gotoDiff(false), { title: '前の差分へ移動します（K）' }),
      button('次の差分', () => this.gotoDiff(true), { title: '次の差分へ移動します（J）' }),
    );
    const searchBox = el('div', 'searchbox');
    const search = el('input', 'search');
    search.type = 'search';
    search.placeholder = '検索';
    search.title =
      '左右の値から文字を探して、その行へ移動します。Enterで次、Shift+Enterで前へ進みます（Ctrl+F、F3）。\n大文字と小文字は区別します。';
    search.setAttribute('aria-label', '検索');
    search.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        this.findNext(!e.shiftKey);
      } else if (e.key === 'Escape') {
        e.preventDefault();
        this.panes[this.active].scroller.focus({ preventScroll: true });
      }
    });
    search.addEventListener('input', () => {
      if (search.value !== this.find?.text) this.resetFind();
      if (!search.value) this.clearSearch();
    });
    const hits = el('span', 'hits');
    hits.setAttribute('aria-live', 'polite');
    const narrow = button('絞り込む', () => this.toggleNarrow(), { title: '検索した文字を含む行だけを表示します' });
    narrow.setAttribute('aria-pressed', 'false');
    searchBox.append(search, hits);
    const goBtn = button('移動', () => undefined, { title: 'キーの値や行番号を指定して移動します（Ctrl+G）' });
    this.goPop = new Popover(goBtn, 'キーまたは行番号で移動').whenOpened(() => fillGo(this.goPop.panel, this.panelHost()));
    const hidden = button('', () => this.showAllColumns(), { cls: 'note' });
    hidden.hidden = true;
    const chips = el('span', 'chips');
    const layout = seg(
      'data-layout',
      [
        ['side', '左右', '表を左右に並べます'],
        ['stack', '上下', '表を上下に並べます。列の多い表に向きます。'],
      ],
      (v) => this.setLayout(v as Layout),
    );
    const viewBtn = button('設定', () => undefined, { title: '並び順・列の表示・拡大率と、そのほかの設定を変えます' });
    this.viewPop = new Popover(viewBtn, '設定').whenOpened(() => fillView(this.viewPop.panel, this.panelHost()));
    const exportBtn = button('書き出す', () => undefined, { title: '表示中の行をExcelまたはCSVのファイルに書き出します' });
    this.exportPop = new Popover(exportBtn, '書き出す').whenOpened(() => fillExport(this.exportPop.panel, this.panelHost()));
    // Three groups, so that a narrow window wraps the toolbar between them
    // rather than in the middle of one.
    const group = (...items: Node[]) => {
      const g = el('div', 'tgroup');
      g.append(...items);
      return g;
    };
    bar.append(
      group(back, cond, kinds, chips),
      el('span', 'grow'),
      group(scope, order, nav),
      group(searchBox, narrow, goBtn, hidden, layout, viewBtn, exportBtn),
    );
    this.tb = { scope, order, kinds, layout, hidden, search, hits, narrow, chips };

    // Tables and detail
    this.work = el('div', 'work');
    this.grids = el('div', 'grids');
    this.grids.id = 'grids';
    this.caps = { left: el('div', 'cap'), right: el('div', 'cap') };
    this.empties = { left: el('div', 'noresult'), right: el('div', 'noresult') };
    for (const side of ['left', 'right'] as Side[]) {
      const wrap = el('div', 'pwrap');
      wrap.dataset.side = side;
      this.panes[side].root.append(this.empties[side]);
      this.empties[side].hidden = true;
      wrap.append(this.caps[side], this.panes[side].root);
      this.grids.append(wrap);
    }
    this.work.append(this.grids, this.detail.root);
    this.sidebar = new Sidebar(api, {
      settings,
      saveSettings: () => host.saveSettings(),
      get reviewFilter() {
        return me.reviewFilter;
      },
      get group() {
        return me.group;
      },
      setReviewFilter: (f) => this.setReviewFilter(f),
      setGroup: (g) => this.setGroup(g),
      nextUnchecked: (fwd) => this.reviews.nextUnchecked(fwd),
      markGroup: (g, st) =>
        this.reviews.markTarget(
          { group: { rule: g.rule, id: g.id, skip: g.skip } },
          st,
          `パターン「${g.label}」`,
          g.skip.length ? `まとめに使わない列（${g.skip.join('、')}）の変更は、このまとまりの中でも値が違うことがあります。` : '',
        ),
      changedColumns: () =>
        this.summary.pair_names.map((name, k) => ({ name, changed: this.summary.pair_changed[k] ?? 0 })).filter((c) => c.changed > 0),
      clearReviews: () => this.reviews.clearAll(),
      onSidebarToggle: () => this.relayout(),
    });
    this.main = el('div', 'mainrow');
    this.main.append(this.sidebar.root, this.work);
    this.root.append(bar, this.main);
    this.applyLayout();
    this.ro = new ResizeObserver(() => this.relayout());
    this.ro.observe(this.grids);
    this.root.addEventListener('keydown', (e) => this.onKey(e));
    press(scope, 'data-scope', this.scope);
    press(order, 'data-order', this.order);
  }

  destroy(): void {
    this.ro.disconnect();
    this.viewPop.destroy();
    this.goPop.destroy();
    this.exportPop.destroy();
    closeMenu();
  }

  /** The toolbar panels' view of this screen (`toolbar-panels.ts`). */
  private panelHost(): PanelHost {
    const me = this;
    return {
      api: this.api,
      settings: this.host.settings,
      summary: this.summary,
      panes: this.panes,
      active: this.active,
      order: this.order,
      scope: this.scope,
      text: this.text,
      sort: this.sort,
      hidden: this.hidden,
      reviewFilter: this.reviewFilter,
      group: this.group,
      get reviewEnabled() {
        return me.reviewEnabled;
      },
      saveSettings: () => this.host.saveSettings(),
      setZoom: (z) => this.host.setZoom(z),
      zoomStep: (dir) => this.zoomStep(dir),
      slotName: (sl) => this.slotName(sl),
      setSort: (so) => this.setSort(so),
      relayoutColumns: () => this.relayoutColumns(),
      repaint: () => this.repaint(),
      showAllColumns: () => this.showAllColumns(),
      onReviewToggle: () => this.onReviewToggle(),
      findKey: (values) => this.findKey(values),
      findLine: (side, text) => this.findLine(side, text),
      closeExport: () => this.exportPop.close(false),
    };
  }

  // ---- PaneHost -----------------------------------------------------------------

  get reviewEnabled(): boolean {
    return this.host.settings.reviewEnabled;
  }

  get showSpaces(): boolean {
    return this.host.settings.showSpaces;
  }

  slotVisible(s: Slot): boolean {
    if (this.hidden.has(this.slotName(s))) return false;
    if (!this.host.settings.changedColumnsOnly) return true;
    return s.key || s.pair === null || this.summary.pair_changed[s.pair] > 0;
  }

  changeCount(s: Slot): { changed: number; ignored: number } | null {
    if (s.pair === null) return null;
    return { changed: this.summary.pair_changed[s.pair] ?? 0, ignored: this.summary.pair_ignored[s.pair] ?? 0 };
  }

  selectedUnit(): number | null {
    return this.sel ? this.sel.unit : null;
  }

  peer(p: Pane): Pane {
    return p.side === 'left' ? this.panes.right : this.panes.left;
  }

  get findText(): string {
    return this.find?.text ?? '';
  }

  onRowsPainted(p: Pane): void {
    this.completeNav(p);
    this.updateSelInfo(p);
  }

  onCornerClick(p: Pane): void {
    this.active = p.side;
    this.selectAll(p);
    p.scroller.focus({ preventScroll: true });
  }

  onFocus(p: Pane): void {
    this.active = p.side;
  }

  onCellDown(p: Pane, row: number, col: number, e: PointerEvent): void {
    this.active = p.side;
    p.scroller.focus({ preventScroll: true });
    const extend = e.shiftKey && p.sel.active;
    if (col < 0) {
      // The line number selects the whole row (with Shift, the rows up to it).
      p.sel.set(row, extend ? p.sel.focus.col : 0, extend);
      p.sel.whole = 'rows';
    } else {
      p.sel.set(row, col, extend);
    }
    this.clearPeerCells(p);
    this.selectRow(p, row, 0, false);
  }

  onCellDrag(p: Pane, row: number, col: number): void {
    if (p.sel.focus.row === row && p.sel.focus.col === col) return;
    p.sel.focus = { row, col: Math.max(0, col) };
    p.schedule();
  }

  /** As in a spreadsheet: the column is selected; with Shift, the columns
   *  from the one selected before. Sorting is in the header's menu. */
  onHeaderClick(p: Pane, slot: number, extend: boolean): void {
    this.active = p.side;
    p.scroller.focus({ preventScroll: true });
    const col = p.order.indexOf(slot);
    if (col < 0 || !p.model?.total) return;
    if (!extend || !p.sel.active) return this.selectColumn(p, col);
    p.sel.set(p.sel.focus.row, col, true);
    p.sel.whole = 'cols';
    this.clearPeerCells(p);
    p.schedule();
    this.updateSelInfo(p);
  }

  onColumnResized(p: Pane): void {
    if (this.order === 'aligned') this.relayoutColumns();
    else p.layoutColumns();
  }

  onContextMenu(p: Pane, e: MouseEvent, where: 'cell' | 'header', slot: number | null): void {
    this.active = p.side;
    const aligned = this.order === 'aligned';
    let items: (MenuItem | null)[];
    if (where === 'header' && slot !== null && p.model) {
      const s = p.model.slots[slot];
      const name = this.slotName(s);
      const display = p.order.indexOf(slot);
      const sorted = this.sort?.by === 'column' && this.sort.slot === slot && (this.sort.side ?? 'left') === p.side;
      items = [
        {
          label: '昇順で並べ替え',
          disabled: !aligned,
          title: aligned ? '' : '並べ替えはキー整列で使えます',
          action: () => this.setSort({ by: 'column', desc: false, slot, side: p.side }),
        },
        {
          label: '降順で並べ替え',
          disabled: !aligned,
          title: aligned ? '' : '並べ替えはキー整列で使えます',
          action: () => this.setSort({ by: 'column', desc: true, slot, side: p.side }),
        },
        ...(sorted ? [{ label: '並べ替えを解除', action: () => this.setSort(null) }] : []),
        null,
        { label: 'この列を選択', disabled: display < 0, action: () => this.selectColumn(p, display) },
        {
          label: 'この列を隠す',
          disabled: aligned && s.key,
          title: aligned && s.key ? 'キーの列は隠せません' : '',
          action: () => this.hideColumn(name),
        },
      ];
    } else {
      const has = p.sel.active;
      const value = this.cursorValue(p);
      items = [
        {
          label: p.sel.withHeader ? 'コピー（列名を含む）' : 'コピー',
          shortcut: 'Ctrl+C',
          disabled: !has,
          action: () => this.copy(p, false, false),
        },
        { label: '列名を付けてコピー', shortcut: 'Ctrl+Shift+C', disabled: !has, action: () => this.copy(p, true, false) },
        {
          label: '左右を並べてコピー',
          disabled: !has || !aligned,
          title: aligned ? '選んだ列を、左の値・右の値の順に並べ、列名を付けてコピーします' : 'キー整列で使えます',
          action: () => this.copy(p, true, true),
        },
        null,
        { label: 'すべて選択', shortcut: 'Ctrl+A', action: () => this.selectAll(p) },
        ...this.reviewMenu(p),
        null,
        {
          label: 'この値を検索',
          disabled: !value,
          title: value ? `「${value.length > 40 ? value.slice(0, 40) + '…' : value}」を検索します` : '',
          action: () => {
            this.tb.search.value = value;
            this.resetFind();
            this.findNext(true);
          },
        },
      ];
    }
    openMenu(e.clientX, e.clientY, items);
  }

  private reviewMenu(p: Pane): (MenuItem | null)[] {
    if (!this.reviewEnabled) return [];
    const g = p.sel.active ? p.sel.range : null;
    const many = g && g.r1 > g.r0 ? g.r1 - g.r0 + 1 : 0;
    const what = many ? `選んだ${fmt(many)}行を` : '';
    const dots = many ? '…' : '';
    return [
      null,
      { label: `${what}確認済みにする${dots}`, shortcut: 'Space', action: () => this.reviews.forSelection(p, 'checked', true) },
      { label: `${what}要調査にする${dots}`, shortcut: '3', action: () => this.reviews.forSelection(p, 'investigate', true) },
      { label: `${what}未確認に戻す${dots}`, shortcut: '1', action: () => this.reviews.forSelection(p, 'unchecked', false) },
      { label: 'メモを書く', shortcut: 'M', disabled: !!many, action: () => this.detail.focusMemo() },
      {
        label: this.reviews.summary?.undo ? `元に戻す：${this.reviews.summary.undo}` : '元に戻す',
        shortcut: 'Ctrl+Z',
        disabled: !this.reviews.summary?.undo,
        action: () => this.reviews.undo(),
      },
    ];
  }

  // ---- toolbar -------------------------------------------------------------------

  private condition(): HTMLElement {
    const s = this.summary;
    const c = el('div', 'cond');
    const k = el('span', '', `キー ${s.keys.join(' + ')}`);
    k.title = [
      '左右のレコードを対応づけた列です。',
      s.left_only.length ? `左だけにある列：${s.left_only.join('、')}` : '',
      s.right_only.length ? `右だけにある列：${s.right_only.join('、')}` : '',
      s.left_only.length || s.right_only.length ? '片側だけの列は、値を比べずに表示だけします。' : '',
    ]
      .filter(Boolean)
      .join('\n');
    c.append(k);
    if (s.excluded.length) {
      const x = el('span', 'muted', `比較しない列 ${s.excluded.length}`);
      x.title = '値が違っても変更として数えない列：\n' + s.excluded.join('\n');
      c.append(x);
    }
    if (s.trim !== 'none') {
      const x = el('span', 'muted', '前後の空白を無視');
      x.title = '値の前後にある半角スペースとタブの違いを、変更として数えていません（キーの列を除く）';
      c.append(x);
    }
    return c;
  }

  private onReviewToggle(): void {
    this.relayoutColumns();
    this.detail.render();
    this.sidebar.render();
    this.reviews.summary = null;
    this.reviews.refresh(true);
    if (!this.reviewEnabled && this.reviewFilter) this.setReviewFilter(null);
    this.updateToolbar();
  }

  private zoomStep(dir: number): void {
    const z = this.host.settings.zoom;
    const i = ZOOM_STEPS.indexOf(z);
    this.host.setZoom(ZOOM_STEPS[clamp(i + dir, 0, ZOOM_STEPS.length - 1)]);
    if (this.viewPop.isOpen) fillView(this.viewPop.panel, this.panelHost());
  }

  private updateToolbar(): void {
    press(this.tb.scope, 'data-scope', this.scope === 'all' || this.scope === 'diff' ? this.scope : null);
    press(this.tb.order, 'data-order', this.order);
    press(this.tb.kinds, 'data-kind', this.scope === 'all' || this.scope === 'diff' ? null : this.scope);
    press(this.tb.layout, 'data-layout', this.host.settings.layout);
    // Filters set from the side panel stay visible here, so rows never go missing unnoticed.
    const chips: Node[] = [];
    if (this.group) {
      const b = button(`パターンで絞り込み中 ×`, () => this.setGroup(null), { cls: 'note' });
      b.title = `${this.group.label}\nクリックで解除します`;
      chips.push(b);
    }
    if (this.reviewFilter && this.reviewEnabled) {
      const label = { unchecked: '未確認', checked: '確認済み', investigate: '要調査' }[this.reviewFilter];
      chips.push(button(`${label}だけ表示中 ×`, () => this.setReviewFilter(null), { cls: 'note', title: 'クリックで解除します' }));
    }
    const saveError = this.reviewEnabled ? this.reviews.summary?.save_error : null;
    if (saveError) {
      chips.push(
        button('確認状況を保存できません', () => this.reviews.retrySave(), {
          cls: 'note err',
          title: `${saveError}\nクリックすると、もう一度保存します。`,
        }),
      );
    }
    this.tb.chips.replaceChildren(...chips);
    const h = this.tb.hidden;
    h.hidden = this.hidden.size === 0;
    h.textContent = `隠した列 ${this.hidden.size}`;
    h.title = `${[...this.hidden].join('\n')}\nクリックで、隠した列をすべて表示します`;
  }

  private updateCaptions(): void {
    for (const side of ['left', 'right'] as Side[]) {
      const s = this.summary[side];
      const cap = this.caps[side];
      const name = el('span', 'cname', s.name);
      name.title = fileTitle(s);
      const total = this.panes[side].model?.total ?? 0;
      const rows = el('span', 'crows', `${fmt(total)}行`);
      rows.title = `この表示の行数（ファイルは${fmt(s.records)}件）`;
      cap.replaceChildren(el('span', 'cside', SIDE_NAME[side]), name, el('span', 'grow'), this.selInfo[side], rows);
      const empty = this.empties[side];
      empty.hidden = total > 0;
      if (total === 0) {
        const msg = this.text ? `「${this.text}」を含む行はありません。` : 'この条件に合う行はありません。';
        const nodes: Node[] = [el('div', '', msg)];
        if (this.text) nodes.push(button('絞り込みを解除', () => this.toggleNarrow(false)));
        else if (this.scope !== 'all') nodes.push(button('全行を表示', () => this.setScope('all')));
        empty.replaceChildren(...nodes);
      }
    }
  }

  // ---- layout --------------------------------------------------------------------

  private applyLayout(): void {
    const st = this.host.settings;
    this.work.dataset.layout = st.layout;
    this.grids.dataset.layout = st.layout;
    const d = this.detail.root;
    d.style.height = st.layout === 'side' && st.detailHeight ? st.detailHeight + 'px' : '';
    d.style.width = st.layout === 'stack' && st.detailWidth ? st.detailWidth + 'px' : '';
    this.updateToolbar();
  }

  private setLayout(l: Layout): void {
    if (this.host.settings.layout === l) return;
    this.host.settings.layout = l;
    this.host.saveSettings();
    this.applyLayout();
    this.relayout();
  }

  private onDetailToggle(open: boolean): void {
    this.host.settings.detailOpen = open;
    this.host.saveSettings();
    this.relayout();
  }

  private resizeDetail(e: PointerEvent): void {
    const d = this.detail.root;
    if (!this.detail.open) return;
    e.preventDefault();
    const stack = this.host.settings.layout === 'stack';
    const r = d.getBoundingClientRect();
    const start = stack ? e.clientX : e.clientY;
    const size0 = stack ? r.width : r.height;
    const max = stack ? window.innerWidth * 0.7 : window.innerHeight * 0.7;
    const move = (ev: PointerEvent) => {
      const delta = (stack ? ev.clientX : ev.clientY) - start;
      const v = Math.round(clamp(size0 - delta, 80, max));
      if (stack) d.style.width = v + 'px';
      else d.style.height = v + 'px';
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      const v = Math.round(stack ? d.getBoundingClientRect().width : d.getBoundingClientRect().height);
      if (stack) this.host.settings.detailWidth = v;
      else this.host.settings.detailHeight = v;
      this.host.saveSettings();
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  }

  relayout(): void {
    for (const p of [this.panes.left, this.panes.right]) if (p.model) p.relayout();
  }

  private relayoutColumns(): void {
    for (const p of [this.panes.left, this.panes.right]) {
      if (!p.model) continue;
      p.layoutColumns();
      p.sel.anchor.col = clamp(p.sel.anchor.col, 0, Math.max(0, p.colCount - 1));
      p.sel.focus.col = clamp(p.sel.focus.col, 0, Math.max(0, p.colCount - 1));
    }
  }

  repaint(): void {
    this.detail.render();
    this.panes.left.schedule();
    this.panes.right.schedule();
  }

  // ---- views ---------------------------------------------------------------------

  private filter(): object {
    const kinds = this.scope === 'all' ? [] : this.scope === 'diff' ? DIFF_KINDS : [this.scope];
    return this.text ? { kinds, text: this.text } : { kinds };
  }

  private model(order: ViewOrder): Model {
    const m = new Model(this.api, order);
    m.widths = this.widths[order];
    m.fixed = this.fixed[order];
    return m;
  }

  /** Views being opened (tests wait for 0). */
  busy = 0;

  async openViews(): Promise<void> {
    this.busy++;
    try {
      await this.openViewsInner();
    } finally {
      this.busy--;
    }
  }

  private async openViewsInner(): Promise<void> {
    const seq = ++this.seq;
    const prevUnit = this.sel && this.sel.unit >= 0 ? this.sel.unit : null;
    const filter = this.filter();
    const extra = {
      group: this.group ? { rule: this.group.rule, id: this.group.id, skip: this.group.skip } : null,
      review: this.reviewFilter && this.reviewEnabled ? [this.reviewFilter] : null,
    };
    let models: Record<Side, Model>;
    try {
      if (this.order === 'aligned') {
        const m = this.model('aligned');
        await m.open(filter, this.sort, extra);
        models = { left: m, right: m };
      } else {
        const l = this.model('left_source');
        const r = this.model('right_source');
        await Promise.all([l.open(filter, null, extra), r.open(filter, null, extra)]);
        models = { left: l, right: r };
      }
    } catch (e) {
      if (seq === this.seq) toast((e as Error).message, 'error');
      return;
    }
    if (seq !== this.seq) return;
    this.nav = null;
    // Row numbers of the old view mean nothing in the new one.
    this.sel = prevUnit !== null ? { unit: prevUnit, rows: {} } : null;
    this.panes.left.setModel(models.left);
    this.panes.right.setModel(models.right);
    this.updateCaptions();
    this.updateToolbar();
    if (prevUnit !== null) {
      const found = await this.locateAndSelect(prevUnit, this.panes[this.active]);
      if (seq !== this.seq) return;
      if (!found) {
        this.sel = null;
        this.detail.clear();
        if (models.left.total || models.right.total) {
          toast('選択していたレコードは、この表示には含まれていません。先頭の行を選びました。');
          this.moveCursor(this.panes[this.active].model?.total ? this.panes[this.active] : this.panes.left, 0, 0, false);
        }
      }
    } else {
      this.sel = null;
    }
  }

  private setScope(s: Scope): void {
    if (s === this.scope) return;
    this.scope = s;
    if (s === 'all' || s === 'diff') this.lastScope = s;
    this.updateToolbar();
    this.openViews();
  }

  private setOrder(o: 'aligned' | 'source'): void {
    if (o === this.order) return;
    this.order = o;
    this.updateToolbar();
    this.openViews();
  }

  private setText(t: string): void {
    if (t === this.text) return;
    this.text = t;
    this.openViews();
  }

  /** Shows only the rows containing the searched text, or all rows again. */
  private toggleNarrow(on = this.tb.narrow.getAttribute('aria-pressed') !== 'true'): void {
    this.tb.narrow.setAttribute('aria-pressed', String(on));
    this.setText(on ? this.tb.search.value : '');
  }

  private clearSearch(): void {
    this.resetFind();
    this.tb.hits.textContent = '';
    if (this.text) this.toggleNarrow(false);
  }

  private resetFind(): void {
    this.find = null;
    this.tb.hits.textContent = '';
    this.repaint();
  }

  /** Moves to the next (or previous) cell containing the searched text. */
  async findNext(forward: boolean): Promise<void> {
    const text = this.tb.search.value;
    if (!text) {
      this.tb.search.focus();
      return;
    }
    const fresh = !this.find || this.find.text !== text;
    if (this.tb.narrow.getAttribute('aria-pressed') === 'true' && this.text !== text) {
      this.setText(text);
      await this.untilIdle();
    }
    let p = this.panes[this.active];
    if (!p.model) return;
    const f = this.find;
    const cur = p.sel.active ? p.sel.focus.row : null;
    // Further matches in the same row first.
    if (!fresh && f && cur === f.row && f.side === p.side) {
      const at = f.at + (forward ? 1 : -1);
      if (at >= 0 && at < f.cells.length) {
        f.at = at;
        this.gotoCell(f.row, f.cells[at]);
        return;
      }
    }
    // In the key-aligned order both tables show the same rows: search from the left one.
    if (this.order === 'aligned') p = this.panes.left;
    let from: number | null = cur;
    if (fresh && cur !== null) from = forward ? cur - 1 : cur + 1;
    if (from !== null && (from < 0 || from >= p.model!.total)) from = null;
    const t0 = performance.now();
    const r = await this.api.call<{ hit: { row: number; wrapped: boolean; left: number[]; right: number[] } | null; count: number | null }>(
      'find_text',
      {
        view: p.model!.id,
        text,
        from,
        forward,
        count: fresh,
      },
    );
    if (r.count !== null) this.tb.hits.textContent = r.count ? `${fmt(r.count)}行` : 'なし';
    if (this.tb.search.value !== text) return;
    if (!r.hit) {
      this.find = { text, row: -1, side: p.side, cells: [], at: 0 };
      this.repaint();
      toast(`「${text}」は見つかりません。`);
      return;
    }
    const h = r.hit;
    const cells: { side: Side; col: number }[] = [];
    for (const side of ['left', 'right'] as Side[]) {
      const q = this.order === 'aligned' ? this.panes[side] : p;
      if (this.order !== 'aligned' && side !== p.side) continue;
      for (const slot of h[side]) {
        const col = q.order.indexOf(slot);
        if (col >= 0) cells.push({ side, col });
      }
    }
    cells.sort((a, b) => (a.side === b.side ? a.col - b.col : a.side === 'left' ? -1 : 1));
    const at = forward ? 0 : Math.max(0, cells.length - 1);
    this.find = { text, row: h.row, side: p.side, cells, at };
    if (h.wrapped) toast(forward ? '末尾まで探したので、先頭から続けました。' : '先頭まで探したので、末尾から続けました。');
    if (cells.length) this.gotoCell(h.row, cells[at], t0);
    else {
      this.gotoCell(h.row, { side: p.side, col: p.sel.active ? p.sel.focus.col : 0 }, t0);
      toast('一致した値は、隠している列にあります。');
    }
    this.repaint();
  }

  private gotoCell(row: number, c: { side: Side; col: number }, t0 = performance.now()): void {
    const q = this.panes[c.side];
    if (!q.model) return;
    this.active = c.side;
    if (this.find) this.find.side = c.side;
    q.sel.set(row, c.col, false);
    this.clearPeerCells(q);
    q.ensureColVisible(c.col);
    this.selectRow(q, row, t0);
  }

  /** Resolves when the views have been opened and painted. */
  private untilIdle(): Promise<void> {
    return new Promise((res) => {
      const chk = () => (this.busy ? requestAnimationFrame(chk) : res());
      requestAnimationFrame(chk);
    });
  }

  /** The value under the cell cursor, from the rows on screen. */
  private cursorValue(p: Pane): string {
    if (!p.sel.active || !p.model) return '';
    const row = p.model.cache.get(p.sel.focus.row);
    const rec = row ? (p.side === 'left' ? row.left : row.right) : null;
    const slot = p.order[p.sel.focus.col];
    if (!rec || slot === undefined) return '';
    return p.model.cell(rec, slot)?.v ?? '';
  }

  private updateSelInfo(p: Pane): void {
    const span = this.selInfo[p.side];
    let text = '';
    if (p.sel.active && p.model?.total) {
      const g = p.sel.range;
      const rows = g.r1 - g.r0 + 1;
      const cols = Math.max(0, g.c1) - Math.max(0, g.c0) + 1;
      if (rows > 1 || cols > 1) text = `選択 ${fmt(rows)}行×${fmt(cols)}列`;
    }
    if (span.textContent !== text) span.textContent = text;
  }

  private setSort(s: SortSpec | null): void {
    this.sort = s;
    if (this.viewPop.isOpen) fillView(this.viewPop.panel, this.panelHost());
    if (this.order === 'aligned') this.openViews();
  }

  private slotName(s: Slot): string {
    return s.name_left ?? s.name_right ?? '';
  }

  private hideColumn(name: string): void {
    this.hidden.add(name);
    this.relayoutColumns();
    this.updateToolbar();
    toast(`列「${name}」を隠しました。差分の判定には影響しません。`);
  }

  private showAllColumns(): void {
    this.hidden.clear();
    this.relayoutColumns();
    this.updateToolbar();
    if (this.viewPop.isOpen) fillView(this.viewPop.panel, this.panelHost());
  }

  // ---- review states and groups ------------------------------------------------------

  private setReviewFilter(f: ReviewStatus | null): void {
    this.reviewFilter = f;
    this.sidebar.setSummary(this.reviews.summary);
    this.updateToolbar();
    this.openViews();
  }

  private setGroup(g: GroupRef | null): void {
    this.group = g;
    this.sidebar.refreshSelection();
    this.updateToolbar();
    this.openViews();
  }

  // ---- selection and navigation ----------------------------------------------------

  private clearPeerCells(p: Pane): void {
    const q = this.peer(p);
    if (q.sel.active) {
      q.sel.active = false;
      q.schedule();
    }
  }

  /** Selects the record at `row` of pane `p` once its row data is known. */
  private selectRow(p: Pane, row: number, t0: number, scroll = true): void {
    this.nav = { pane: p, row, t0 };
    const rows: SelState['rows'] = this.order === 'aligned' ? { left: row, right: row } : { [p.side]: row };
    const cached = p.model?.cache.get(row);
    this.sel = { unit: cached ? cached.unit : -1, rows };
    if (scroll) p.ensureVisible(row);
    p.schedule();
    this.peer(p).schedule();
    this.completeNav(p);
  }

  private completeNav(p: Pane): void {
    const n = this.nav;
    if (!n || n.pane !== p || !p.model) return;
    const row = p.model.cache.get(n.row);
    if (!row) return;
    this.nav = null;
    const rows: SelState['rows'] = this.order === 'aligned' ? { left: n.row, right: n.row } : { [p.side]: n.row };
    this.sel = { unit: row.unit, rows };
    this.detail.load(row.unit);
    if (this.order === 'source') this.locateInPeer(row.unit, p);
    this.panes.left.schedule();
    this.panes.right.schedule();
    if (n.t0) requestAnimationFrame(() => metrics.navPaint.push(performance.now() - n.t0));
  }

  private async locate(m: Model, unit: number): Promise<number | null> {
    const r = await this.api.call<{ row: number | null }>('locate', { view: m.id, unit });
    return r.row;
  }

  private async locateInPeer(unit: number, p: Pane): Promise<void> {
    const q = this.peer(p);
    if (!q.model) return;
    const row = await this.locate(q.model, unit).catch(() => null);
    if (!this.sel || this.sel.unit !== unit) return;
    this.sel.rows[q.side] = row;
    if (row !== null) q.ensureVisible(row);
    q.schedule();
  }

  /** Selects `unit` in both panes and scrolls to it. False if the view does not contain it. */
  async locateAndSelect(unit: number, origin: Pane): Promise<boolean> {
    const rows: SelState['rows'] = {};
    if (this.order === 'aligned') {
      const r = await this.locate(this.panes.left.model!, unit);
      rows.left = r;
      rows.right = r;
    } else {
      const [l, r] = await Promise.all([this.locate(this.panes.left.model!, unit), this.locate(this.panes.right.model!, unit)]);
      rows.left = l;
      rows.right = r;
    }
    if (rows.left == null && rows.right == null) return false;
    this.nav = null;
    this.sel = { unit, rows };
    for (const side of ['left', 'right'] as Side[]) {
      const r = rows[side];
      const p = this.panes[side];
      if (r == null) continue;
      p.ensureVisible(r);
      if (side === origin.side || (rows[origin.side] == null && side !== origin.side)) {
        p.sel.set(r, Math.min(p.sel.focus.col, Math.max(0, p.colCount - 1)), false);
      }
    }
    if (rows[origin.side] == null) this.active = origin.side === 'left' ? 'right' : 'left';
    this.clearPeerCells(this.panes[this.active]);
    this.detail.load(unit);
    this.repaint();
    return true;
  }

  private moveCursor(p: Pane, row: number, col: number, extend: boolean): void {
    if (!p.model || p.model.total === 0) return;
    row = clamp(row, 0, p.model.total - 1);
    col = clamp(col, 0, Math.max(0, p.colCount - 1));
    p.sel.set(row, col, extend);
    this.clearPeerCells(p);
    p.ensureColVisible(col);
    this.selectRow(p, row, performance.now());
  }

  async gotoDiff(forward: boolean): Promise<void> {
    const p = this.panes[this.active];
    if (!p.model) return;
    const t0 = performance.now();
    const cur = this.sel?.rows[p.side];
    const from = cur === undefined ? null : cur;
    const r = await this.api.call<{ row: number | null }>('next', { view: p.model.id, from, forward });
    if (r.row === null) {
      toast(forward ? 'これより後に差分はありません。' : 'これより前に差分はありません。');
      return;
    }
    p.sel.set(r.row, p.sel.active ? p.sel.focus.col : 0, false);
    this.clearPeerCells(p);
    this.selectRow(p, r.row, t0);
  }

  /** The cell keys start from: the cursor, or, in a table without one, the
   *  row of the record selected (the top row when it is not there). */
  private cursorOf(p: Pane): { row: number; col: number } {
    if (p.sel.active) return p.sel.focus;
    const last = Math.max(0, (p.model?.total ?? 1) - 1);
    return { row: clamp(this.sel?.rows[p.side] ?? Math.floor(p.top), 0, last), col: 0 };
  }

  /** Selects the whole rows or columns of the selection, or the whole
   *  table. The cell cursor stays where it is. */
  private selectWhole(p: Pane, whole: 'rows' | 'cols' | 'all'): void {
    if (!p.model || !p.model.total) return;
    const had = p.sel.active;
    p.sel.widen(whole, this.cursorOf(p));
    this.clearPeerCells(p);
    // The record selected is the cursor's, as after a click.
    if (!had) this.selectRow(p, p.sel.focus.row, 0, false);
    p.schedule();
    this.updateSelInfo(p);
  }

  private selectAll(p: Pane): void {
    this.selectWhole(p, 'all');
  }

  private selectColumn(p: Pane, col: number): void {
    if (!p.model || !p.model.total || col < 0) return;
    p.sel.set(this.cursorOf(p).row, col, false);
    this.selectWhole(p, 'cols');
  }

  /** The Ctrl+arrow being worked out, and the keys pressed meanwhile. */
  private edge: { pane: Pane } | null = null;
  private pressed: Press[] = [];

  /** Ctrl+arrow: to the edge of the values, as in a spreadsheet. A click or
   *  another change of the selection while it is worked out wins: the reply
   *  and the keys pressed before it are dropped. */
  private async jumpToEdge(p: Pane, dir: Direction, extend: boolean): Promise<void> {
    const m = p.model;
    if (!m || !m.total) return;
    const from = this.cursorOf(p);
    const snap = () => JSON.stringify([this.active, p.model?.id, p.sel.active, p.sel.anchor, p.sel.focus, p.sel.whole]);
    const was = snap();
    this.edge = { pane: p };
    let ok = false;
    try {
      const r = await this.api.call<{ row: number; col: number }>('edge', {
        view: m.id,
        side: p.side,
        row: from.row,
        col: from.col,
        dir,
        slots: p.order,
      });
      ok = snap() === was;
      if (ok) this.moveCursor(p, r.row, r.col, extend);
    } catch (e) {
      toast((e as Error).message, 'error');
    } finally {
      this.edge = null;
    }
    const keys = this.pressed;
    this.pressed = [];
    if (!ok) return;
    for (let i = 0; i < keys.length; i++) {
      this.gridKey(this.panes[this.active], keys[i]);
      if (this.edge) {
        // Another Ctrl+arrow: the rest wait for it.
        this.pressed = keys.slice(i + 1).concat(this.pressed);
        break;
      }
    }
  }

  async copy(p: Pane, header: boolean, both: boolean): Promise<void> {
    if (!p.model || !p.sel.active) {
      toast('コピーする範囲を選んでください。');
      return;
    }
    const g = p.sel.range;
    const slots = p.order.slice(Math.max(0, g.c0), g.c1 + 1);
    header = header || p.sel.withHeader;
    try {
      const r = await this.api.copy(
        {
          view: p.model.id,
          start: g.r0,
          end: g.r1 + 1,
          slots,
          side: both ? 'both' : p.side,
          header,
        },
        this.host.settings.clipboardExclude,
      );
      const cols = slots.length * (both ? 2 : 1);
      toast(`${fmt(r.rows)}行×${fmt(cols)}列を${header ? '列名付きで' : ''}コピーしました。`);
    } catch (e) {
      toast((e as Error).message, 'error');
    }
  }

  private async findKey(values: string[]): Promise<void> {
    const r = await this.api.call<{ unit: number | null }>('find', { key: values });
    if (r.unit === null) {
      toast(`キー「${values.join(' / ')}」のレコードはありません。値は完全に一致するものを探します。`);
      return;
    }
    this.goPop.close(false);
    if (!(await this.locateAndSelect(r.unit, this.panes[this.active]))) {
      toast('そのレコードは今の表示に含まれていません。「全行」にして絞り込みを解除すると表示できます。');
    }
    this.panes[this.active].scroller.focus({ preventScroll: true });
  }

  private async findLine(side: Side, text: string): Promise<void> {
    const line = Number(text.replace(/[,\s]/g, ''));
    if (!Number.isInteger(line) || line < 1) {
      toast('行番号には1以上の整数を入力してください。');
      return;
    }
    const r = await this.api.call<{ unit: number | null }>('find', { side, line });
    if (r.unit === null) {
      toast(`${SIDE_NAME[side]}のファイルの${fmt(line)}行目はデータの行ではありません（列名の行か、ファイルの範囲外です）。`);
      return;
    }
    this.goPop.close(false);
    this.active = side;
    if (!(await this.locateAndSelect(r.unit, this.panes[side]))) {
      toast('そのレコードは今の表示に含まれていません。「全行」にして絞り込みを解除すると表示できます。');
    }
    this.panes[side].scroller.focus({ preventScroll: true });
  }

  // ---- keyboard ------------------------------------------------------------------

  private onKey(e: KeyboardEvent): void {
    const t = e.target as HTMLElement;
    const inField = t instanceof HTMLInputElement || t instanceof HTMLSelectElement || t instanceof HTMLTextAreaElement;
    const k = e.key;
    const ctrl = e.ctrlKey || e.metaKey;
    if (k === 'F1') {
      e.preventDefault();
      showKeys(this.reviewEnabled);
      return;
    }
    if (ctrl && (k === 'f' || k === 'F')) {
      e.preventDefault();
      const v = this.cursorValue(this.panes[this.active]);
      // With nothing typed yet, start from the value under the cursor.
      if (!this.tb.search.value && v && !v.includes('\n') && v.length <= 100) this.tb.search.value = v;
      this.tb.search.focus();
      this.tb.search.select();
      return;
    }
    if (k === 'F3') {
      e.preventDefault();
      this.findNext(!e.shiftKey);
      return;
    }
    if (ctrl && (k === 'g' || k === 'G')) {
      e.preventDefault();
      this.goPop.open();
      return;
    }
    if (inField || t.closest('.popover, .menu')) return;
    // Enter and Space work the focused button, tab or link, as anywhere else.
    if ((k === 'Enter' || (k === ' ' && !ctrl)) && t.closest('button, a[href], summary, label, [role="tab"], [role="button"]')) return;
    const p = this.panes[this.active];
    if (!p.model) return;
    const d: Press = { k, sh: e.shiftKey, ctrl, alt: e.altKey };
    if (this.edge) {
      // Keys pressed while a Ctrl+arrow is worked out are applied after it, in turn.
      if (this.gridKey(p, d, true)) {
        e.preventDefault();
        this.pressed.push(d);
      }
      return;
    }
    if (this.gridKey(p, d)) e.preventDefault();
  }

  /** Keys on the tables. True if `d` is one of them; with `dry`, nothing is done. */
  private gridKey(p: Pane, d: Press, dry = false): boolean {
    const { k, sh, ctrl, alt } = d;
    const m = p.model;
    if (!m) return false;
    const cur = this.cursorOf(p);
    const act = (f: () => void) => {
      if (!dry) f();
      return true;
    };
    // Keys as in a spreadsheet, before Space marks a record reviewed.
    if (k === ' ' && (sh || ctrl) && !alt) return act(() => this.selectWhole(p, sh && ctrl ? 'all' : sh ? 'rows' : 'cols'));
    if (ctrl && !alt && k in ARROW) return act(() => this.jumpToEdge(p, ARROW[k], sh));
    if (ctrl && (k === 'PageDown' || k === 'PageUp'))
      return act(() => {
        const q = this.panes[k === 'PageDown' ? 'right' : 'left'];
        this.active = q.side;
        q.scroller.focus({ preventScroll: true });
      });
    if (this.reviewEnabled && !alt) {
      const status: Record<string, ReviewStatus> = { ' ': 'checked', '2': 'checked', '3': 'investigate', '1': 'unchecked' };
      if (!ctrl && k in status) return act(() => this.reviews.forSelection(p, status[k], k !== '1'));
      if (!ctrl && (k === 'n' || k === 'N')) return act(() => this.reviews.nextUnchecked(!sh));
      if (!ctrl && (k === 'm' || k === 'M')) return act(() => this.detail.focusMemo());
      if (ctrl && (k === 'z' || k === 'Z')) return act(() => this.reviews.undo(sh));
      if (ctrl && (k === 'y' || k === 'Y')) return act(() => this.reviews.undo(true));
    }
    const move = (row: number, col: number, extend = sh) => act(() => this.moveCursor(p, row, col, extend));
    if (!ctrl && (k === 'j' || k === 'J')) return act(() => this.gotoDiff(true));
    if (!ctrl && (k === 'k' || k === 'K')) return act(() => this.gotoDiff(false));
    if (k === 'ArrowDown') return move(cur.row + 1, cur.col);
    if (k === 'ArrowUp') return move(cur.row - 1, cur.col);
    if (k === 'ArrowRight') return move(cur.row, cur.col + 1);
    if (k === 'ArrowLeft') return move(cur.row, cur.col - 1);
    if (k === 'PageDown') return move(cur.row + p.visibleRows, cur.col);
    if (k === 'PageUp') return move(cur.row - p.visibleRows, cur.col);
    if (k === 'Home') return move(ctrl ? 0 : cur.row, 0);
    if (k === 'End') return move(ctrl ? m.total - 1 : cur.row, p.colCount - 1);
    if (k === 'Enter' && !ctrl && !alt) return move(cur.row + (sh ? -1 : 1), cur.col, false);
    if (ctrl && (k === 'a' || k === 'A')) return act(() => this.selectAll(p));
    if (ctrl && (k === 'c' || k === 'C')) return act(() => this.copy(p, sh, false));
    if (k === 'F6')
      return act(() => {
        const q = this.peer(p);
        this.active = q.side;
        q.scroller.focus({ preventScroll: true });
      });
    if (k === 'ContextMenu' || (sh && k === 'F10'))
      return act(() => {
        // Open the menu at the cell cursor.
        const cell = p.rowsEl.querySelector(`.row[data-row="${cur.row}"] .c[data-col="${cur.col}"]`);
        const r = (cell ?? p.scroller).getBoundingClientRect();
        this.onContextMenu(p, new MouseEvent('contextmenu', { clientX: r.left + 8, clientY: r.bottom }), 'cell', null);
      });
    return false;
  }

  // ---- start ---------------------------------------------------------------------

  async start(): Promise<void> {
    metrics.readyAt = performance.now();
    this.sidebar.render();
    this.reviews.refresh(true);
    await this.openViews();
    const c = this.summary.counts;
    if (c.changed + c.added + c.deleted === 0) {
      toast('この比較条件では、値の違うレコードはありません。');
      if (this.panes.left.model?.total) this.moveCursor(this.panes.left, 0, 0, false);
    } else {
      await this.gotoDiff(true);
    }
    this.panes.left.scroller.focus({ preventScroll: true });
    await new Promise((r) => requestAnimationFrame(r));
    metrics.firstPaint = performance.now();
  }
}
