// Side panel: review progress (when review states are on) and change
// patterns grouped by a rule the user chooses (plan 6, U-16). Closed by
// default so the tables keep their width.

import type { Backend } from './api';
import { toast } from './menu';
import type { Settings } from './settings';
import { button, el, fmt } from './util';

export type ReviewStatus = 'unchecked' | 'checked' | 'investigate';
export type GroupRuleName = 'columns' | 'per_column' | 'full';

export interface ReviewSummary {
  ready: boolean;
  saved: boolean;
  restored: number;
  stale_saved_at: number | null;
  /** The saved file could not be used (damaged or from a newer version). */
  load_problem: string | null;
  /** Why the last write failed; null once a write succeeds. */
  save_error: string | null;
  unsaved: boolean;
  total: number;
  checked: number;
  investigate: number;
  unchecked: number;
  memos: number;
  undo: string | null;
  redo: string | null;
}

interface GroupItem {
  id: number;
  count: number;
  with_other: number;
  first_unit: number;
  changes: { name: string; before?: string; after?: string }[];
}

interface GroupsPage {
  rule: GroupRuleName;
  total: number;
  singles: number;
  changed_units: number;
  /** Changed records whose changes are all in skipped columns. */
  only_skipped: number;
  /** Columns actually left out (names present in this comparison). */
  skip: string[];
  items: GroupItem[];
}

export interface GroupRef {
  rule: GroupRuleName;
  id: number;
  label: string;
  skip: string[];
}

export interface SidebarHost {
  settings: Settings;
  saveSettings(): void;
  reviewFilter: ReviewStatus | null;
  group: GroupRef | null;
  setReviewFilter(f: ReviewStatus | null): void;
  setGroup(g: GroupRef | null): void;
  nextUnchecked(forward: boolean): void;
  markGroup(g: GroupRef, status: ReviewStatus): void;
  clearReviews(): void;
  /** Compared columns with at least one changed value. */
  changedColumns(): { name: string; changed: number }[];
  onSidebarToggle(): void;
}

const RULES: [GroupRuleName, string, string][] = [
  ['per_column', '列ごとの変更', '列ごとに、変更前と変更後の値が同じものをまとめます。1件のレコードは、変更した列の数だけ数えます。'],
  ['columns', '変更列の組合せ', '変更した列の組合せが同じレコードをまとめます。値は比べません。'],
  [
    'full',
    '完全な変更',
    '変更したすべての列と、その前後の値が同じレコードをまとめます。まとめて確認済みにできるのは、このまとめ方だけです。',
  ],
];

const shown = (v: string | undefined) => (v === undefined ? '' : v === '' ? '（空）' : v);

export function groupLabel(rule: GroupRuleName, g: GroupItem): string {
  if (rule === 'columns') return g.changes.map((c) => c.name).join('、');
  return g.changes.map((c) => `${c.name}：${shown(c.before)} → ${shown(c.after)}`).join('、');
}

export class Sidebar {
  readonly root: HTMLElement;
  private strip: HTMLButtonElement;
  private panel: HTMLDivElement;
  private reviewBox: HTMLElement;
  private groupBox: HTMLElement;
  private list: HTMLUListElement;
  private info: HTMLElement;
  private summary: ReviewSummary | null = null;
  private page: GroupsPage | null = null;
  private seq = 0;

  constructor(
    private api: Backend,
    private host: SidebarHost,
  ) {
    this.root = el('aside', 'sidebar');
    this.root.setAttribute('aria-label', '変更パターンと確認状況');
    this.strip = button('', () => this.setOpen(true), { cls: 'strip', title: '変更パターンを表示します' });
    this.panel = el('div', 'spanel');
    const head = el('div', 'shead');
    head.append(
      el('span', 'grow'),
      button('閉じる', () => this.setOpen(false), { title: 'この欄を閉じて、表を広くします' }),
    );
    this.reviewBox = el('section', 'ssec');
    this.groupBox = el('section', 'ssec');
    this.list = el('ul', 'glist');
    this.info = el('div', 'muted ginfo');
    this.panel.append(head, this.reviewBox, this.groupBox);
    this.root.append(this.strip, this.panel);
    this.applyOpen();
  }

  get open(): boolean {
    return this.host.settings.sidebarOpen;
  }

  setOpen(open: boolean): void {
    this.host.settings.sidebarOpen = open;
    this.host.saveSettings();
    this.applyOpen();
    this.host.onSidebarToggle();
    if (open) this.loadGroups();
  }

  private applyOpen(): void {
    this.root.classList.toggle('collapsed', !this.open);
    this.strip.hidden = this.open;
    this.panel.hidden = !this.open;
    this.renderStrip();
  }

  private renderStrip(): void {
    const s = this.summary;
    const review = this.host.settings.reviewEnabled && s?.ready;
    this.strip.textContent = review ? `確認 ${fmt(s!.checked)}／${fmt(s!.total)}　変更パターン` : '変更パターン';
  }

  /** Called whenever the review summary changes. */
  setSummary(s: ReviewSummary | null): void {
    this.summary = s;
    this.renderStrip();
    this.renderReview();
  }

  render(): void {
    this.renderReview();
    this.renderGroupsHead();
    if (this.open) this.loadGroups();
  }

  private renderReview(): void {
    const s = this.summary;
    const box = this.reviewBox;
    box.hidden = !this.host.settings.reviewEnabled;
    if (box.hidden) return;
    if (!s || !s.ready) {
      box.replaceChildren(el('h2', '', '確認状況'), el('div', 'muted', '前回の確認状況を探しています…'));
      return;
    }
    const pct = s.total ? Math.round((s.checked / s.total) * 100) : 100;
    const bar = el('div', 'rbar');
    const fill = el('div', 'rfill');
    fill.style.width = `${pct}%`;
    bar.append(fill);
    bar.title = `差分 ${fmt(s.total)}件のうち、確認済み ${fmt(s.checked)}件（${pct}%）`;
    const filters = el('div', 'rfilters');
    filters.setAttribute('role', 'group');
    filters.setAttribute('aria-label', '確認状況で絞り込む');
    for (const [st, label, n] of [
      ['unchecked', '未確認', s.unchecked],
      ['investigate', '要調査', s.investigate],
      ['checked', '確認済み', s.checked],
    ] as [ReviewStatus, string, number][]) {
      const b = button(`${label} ${fmt(n)}`, () => this.host.setReviewFilter(this.host.reviewFilter === st ? null : st), {
        title: `${label}の差分だけを表示します。もう一度押すと戻します。`,
      });
      b.setAttribute('aria-pressed', String(this.host.reviewFilter === st));
      filters.append(b);
    }
    const next = el('div', 'rrow');
    next.append(button('次の未確認へ', () => this.host.nextUnchecked(true), { title: '次の未確認の差分へ移動します（N。前へはShift+N）' }));
    const nodes: Node[] = [
      el('h2', '', '確認状況'),
      el('div', 'rcount', `確認済み ${fmt(s.checked)}／${fmt(s.total)}件`),
      bar,
      filters,
      next,
    ];
    if (s.memos) nodes.push(el('div', 'muted', `メモあり ${fmt(s.memos)}件`));
    if (s.checked || s.investigate || s.memos) {
      nodes.push(
        button('すべて未確認に戻す…', () => this.host.clearReviews(), {
          cls: 'rclear',
          title: 'この比較で付けた確認状況とメモを消します。件数を確かめてから実行し、あとから元に戻せます。',
        }),
      );
    }
    box.replaceChildren(...nodes);
  }

  private renderGroupsHead(): void {
    const rule = this.host.settings.groupRule;
    const sel = el('select');
    sel.setAttribute('aria-label', 'まとめ方');
    for (const [v, label, title] of RULES) {
      const o = el('option', '', label);
      o.value = v;
      o.title = title;
      o.selected = v === rule;
      sel.append(o);
    }
    sel.addEventListener('change', () => {
      this.host.settings.groupRule = sel.value as GroupRuleName;
      this.host.saveSettings();
      if (this.host.group) this.host.setGroup(null);
      this.renderGroupsHead();
      this.loadGroups();
    });
    const l = el('label', 'field', 'まとめ方');
    l.append(sel);
    const explain = el('p', 'gexplain', RULES.find((r) => r[0] === rule)![2]);
    this.groupBox.replaceChildren(el('h2', '', '変更パターン'), l, explain, this.skipControl(), this.info, this.list);
  }

  /** The columns left out of the grouping: always shown when set, chosen from the changed columns. */
  private skipControl(): HTMLElement {
    const st = this.host.settings;
    const cols = this.host.changedColumns();
    const applied = cols.filter((c) => st.groupSkip.includes(c.name)).map((c) => c.name);
    const box = el('details', 'gskip');
    box.hidden = cols.length === 0;
    const sum = el('summary', '', `まとめに使わない列：${applied.length ? applied.join('、') : 'なし'}`);
    sum.title =
      '時刻のように毎回変わる列を外すと、ほかの変更が同じレコードをまとめられます。外した列の変更は、差分としてはそのまま残ります。';
    box.append(sum);
    for (const c of cols) {
      const lab = el('label', 'check');
      const i = el('input');
      i.type = 'checkbox';
      i.checked = applied.includes(c.name);
      i.addEventListener('change', () => {
        const rest = st.groupSkip.filter((n) => n !== c.name);
        st.groupSkip = i.checked ? [...rest, c.name] : rest;
        this.host.saveSettings();
        if (this.host.group) this.host.setGroup(null);
        sum.textContent = `まとめに使わない列：${
          cols
            .filter((x) => st.groupSkip.includes(x.name))
            .map((x) => x.name)
            .join('、') || 'なし'
        }`;
        this.loadGroups();
      });
      lab.append(i, document.createTextNode(`${c.name}（変更 ${fmt(c.changed)}件）`));
      box.append(lab);
    }
    return box;
  }

  async loadGroups(more = false): Promise<void> {
    if (!this.open) return;
    if (!this.groupBox.firstChild) this.renderGroupsHead();
    const rule = this.host.settings.groupRule;
    const seq = ++this.seq;
    const start = more && this.page ? this.page.items.length : 0;
    if (!more) this.info.textContent = 'まとめています…';
    let page: GroupsPage;
    try {
      page = await this.api.call<GroupsPage>('groups', { rule, skip: this.host.settings.groupSkip, start, count: 100 });
    } catch (e) {
      if (seq !== this.seq) return;
      // Full patterns are aggregated in the background after the comparison.
      if ((e as Error).message.includes('集計中')) {
        setTimeout(() => seq === this.seq && this.loadGroups(more), 500);
        return;
      }
      this.info.textContent = (e as Error).message;
      return;
    }
    if (seq !== this.seq) return;
    if (more && this.page) page = { ...page, items: [...this.page.items, ...page.items] };
    this.page = page;
    this.renderGroups();
  }

  renderGroups(): void {
    const p = this.page;
    if (!p) return;
    const rule = p.rule;
    if (!p.changed_units) {
      this.info.textContent = '値が変わったレコードはありません。';
      this.list.replaceChildren();
      return;
    }
    const unit = rule === 'per_column' ? '組' : 'まとまり';
    this.info.textContent = `変更 ${fmt(p.changed_units)}件 → ${fmt(p.total)}${unit}（1件だけのもの ${fmt(p.singles)}）`;
    if (p.only_skipped) this.info.textContent += `。まとめに使わない列だけが変わったもの ${fmt(p.only_skipped)}件は含みません`;
    const items = p.items.map((g) => {
      const li = el('li');
      const label = groupLabel(rule, g);
      const active = this.host.group?.rule === rule && this.host.group.id === g.id;
      const b = button('', () => this.host.setGroup(active ? null : { rule, id: g.id, label, skip: p.skip }), {
        cls: 'gitem',
        title: `${label}\n${fmt(g.count)}件。クリックすると、このまとまりの行だけを表示します。`,
      });
      b.setAttribute('aria-pressed', String(active));
      const lines = rule === 'full' && g.changes.length > 3 ? [...g.changes.slice(0, 3)] : g.changes;
      const text = el('span', 'gtext');
      if (rule === 'columns') text.textContent = label;
      else
        for (const c of lines) {
          const line = el('span', 'gline');
          line.append(el('b', '', c.name), document.createTextNode(`：${shown(c.before)} → ${shown(c.after)}`));
          text.append(line);
        }
      if (rule === 'full' && g.changes.length > 3) text.append(el('span', 'muted', `ほか${g.changes.length - 3}列`));
      b.append(el('span', 'gcount', fmt(g.count)), text);
      li.append(b);
      if (rule === 'per_column' && g.with_other) {
        li.append(el('div', 'gnote', `うち ${fmt(g.with_other)}件は、ほかの列も変わっています`));
      }
      if (active && rule === 'full' && this.host.settings.reviewEnabled) {
        li.append(
          button(`この${fmt(g.count)}件を確認済みにする…`, () => this.host.markGroup({ rule, id: g.id, label, skip: p.skip }, 'checked'), {
            cls: 'gmark',
            title: '対象の件数と内容を確かめてから、まとめて確認済みにします',
          }),
        );
      }
      return li;
    });
    if (p.items.length < p.total) {
      const li = el('li');
      li.append(button(`さらに表示（残り ${fmt(p.total - p.items.length)}）`, () => this.loadGroups(true)));
      items.push(li);
    }
    this.list.replaceChildren(...items);
  }

  /** The group filter changed elsewhere (cleared from the toolbar). */
  refreshSelection(): void {
    this.renderGroups();
  }

  notify(text: string): void {
    toast(text);
  }
}
