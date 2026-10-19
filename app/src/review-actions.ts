// Review states on the comparison screen (when they are turned on): marking
// records, one at once or several after a confirmation that shows what the
// change applies to; notes; undo and redo; moving to the next unchecked
// difference; and watching the background save, so a failed save is shown
// and never lost silently.

import type { Backend } from './api';
import type { SelState } from './compare';
import type { DetailPanel } from './detail';
import { REVIEW_LABEL, type Pane, type Side } from './grid';
import { confirmDialog, toast } from './menu';
import type { Settings } from './settings';
import type { ReviewStatus, ReviewSummary, Sidebar } from './sidebar';
import { el, fmt } from './util';

/** What the review actions need from the comparison screen. */
export interface ReviewHost {
  readonly api: Backend;
  readonly settings: Settings;
  readonly root: HTMLElement;
  readonly panes: Record<Side, Pane>;
  readonly active: Side;
  readonly sel: SelState | null;
  readonly detail: DetailPanel;
  readonly sidebar: Sidebar;
  updateToolbar(): void;
  /** Selects `row` of pane `p` and shows its record. */
  goToRow(p: Pane, row: number, t0: number): void;
  onChangeConditions(): void;
}

export class ReviewActions {
  /** The last review summary from the service (counts, save state, undo labels). */
  summary: ReviewSummary | null = null;
  private saveTimer = 0;

  constructor(private h: ReviewHost) {}

  get enabled(): boolean {
    return this.h.settings.reviewEnabled;
  }

  /** Fetches the review summary; until saved states are looked for, polls. */
  async refresh(announce = false): Promise<void> {
    if (!this.enabled) {
      this.h.sidebar.setSummary(null);
      return;
    }
    let s: ReviewSummary;
    try {
      s = await this.h.api.call<ReviewSummary>('review_summary');
    } catch {
      return;
    }
    const first = !this.summary?.ready && s.ready;
    this.summary = s;
    this.h.sidebar.setSummary(s);
    if (!s.ready) {
      setTimeout(() => this.refresh(true), 300);
      return;
    }
    if (first && announce) {
      if (s.load_problem) toast(s.load_problem, 'error');
      else if (s.restored) toast(`前回の確認状況を読み込みました（確認済み ${fmt(s.checked)}件、要調査 ${fmt(s.investigate)}件）。`);
      else if (s.stale_saved_at) toast('前回この2つのファイルで付けた確認状況は、ファイルの内容か比較の条件が変わったため使っていません。');
      this.h.panes.left.model?.refresh();
      if (this.h.panes.right.model !== this.h.panes.left.model) this.h.panes.right.model?.refresh();
      if (this.h.detail.unit !== null) this.h.detail.load(this.h.detail.unit);
    }
  }

  private afterChange(s: ReviewSummary): void {
    this.noteSaveState(s);
    this.summary = s;
    this.h.sidebar.setSummary(s);
    this.h.panes.left.model?.refresh();
    if (this.h.panes.right.model !== this.h.panes.left.model) this.h.panes.right.model?.refresh();
    if (this.h.detail.unit !== null) this.h.detail.load(this.h.detail.unit);
    this.watchSave();
  }

  /**
   * Back to the setup screen. Review states not yet written are written
   * first; if that fails, the user decides whether to leave without them.
   */
  async leave(): Promise<void> {
    clearTimeout(this.saveTimer);
    if (this.enabled && this.summary?.unsaved) {
      try {
        await this.h.api.call('review_save');
      } catch (e) {
        const ok = await confirmDialog(
          '確認状況を保存できません',
          [el('p', '', (e as Error).message), el('p', '', 'このまま戻ると、保存していない確認状況は失われます。')],
          '保存せずに戻る',
        );
        if (!ok) return;
      }
    }
    this.h.onChangeConditions();
  }

  /**
   * Changes are written in the background shortly after they are made.
   * Checks that the write went through, and shows when it did not.
   */
  private watchSave(delay = 1500): void {
    if (!this.summary?.saved) return;
    clearTimeout(this.saveTimer);
    this.saveTimer = window.setTimeout(async () => {
      let s: ReviewSummary;
      try {
        s = await this.h.api.call<ReviewSummary>('review_summary');
      } catch {
        return;
      }
      if (!this.h.root.isConnected) return;
      this.noteSaveState(s);
      this.summary = s;
      this.h.sidebar.setSummary(s);
      // Still writing (a large set), or waiting to try again.
      if (s.unsaved) this.watchSave(s.save_error ? 6000 : 1000);
    }, delay);
  }

  /** Tells when saving starts or stops failing, and updates the toolbar's notice. */
  private noteSaveState(s: ReviewSummary): void {
    const was = this.summary?.save_error ?? null;
    if (s.save_error && s.save_error !== was) toast(s.save_error, 'error');
    else if (!s.save_error && was) toast('確認状況を保存しました。');
    this.summary = s;
    this.h.updateToolbar();
  }

  async retrySave(): Promise<void> {
    try {
      const s = await this.h.api.call<ReviewSummary>('review_save');
      this.summary = s;
      this.h.sidebar.setSummary(s);
      this.h.updateToolbar();
      toast('確認状況を保存しました。');
    } catch (e) {
      toast((e as Error).message, 'error');
    }
  }

  private ready(): boolean {
    if (!this.enabled) return false;
    if (!this.summary?.ready) {
      toast('前回の確認状況を読み込んでいます。少し待ってから操作してください。');
      return false;
    }
    return true;
  }

  /** Marks records. One record is changed at once; several are shown first. */
  async set(units: number[], status: ReviewStatus, advance: boolean): Promise<void> {
    if (!this.ready() || !units.length) return;
    try {
      const s = await this.h.api.call<ReviewSummary & { changed: number }>('review_set', { units, status });
      this.afterChange(s);
      if (advance && this.h.settings.advanceAfterReview && status !== 'unchecked') this.nextUnchecked(true, true);
    } catch (e) {
      toast((e as Error).message, 'error');
    }
  }

  async setMemo(unit: number, memo: string): Promise<void> {
    if (!this.ready()) return;
    try {
      const s = await this.h.api.call<ReviewSummary>('review_set', { units: [unit], memo, label: 'メモを書く' });
      this.summary = s;
      this.h.sidebar.setSummary(s);
      this.h.panes.left.model?.refresh();
      if (this.h.panes.right.model !== this.h.panes.left.model) this.h.panes.right.model?.refresh();
      this.watchSave();
    } catch (e) {
      toast((e as Error).message, 'error');
    }
  }

  /**
   * Shows what a bulk change applies to (count, kinds, changed columns), and
   * applies it to exactly those records when confirmed.
   */
  async markTarget(target: object, status: ReviewStatus, what: string, caution = ''): Promise<void> {
    if (!this.ready()) return;
    type Target = {
      token: number;
      count: number;
      skipped_same: number;
      kinds: { changed: number; added: number; deleted: number };
      columns: { name: string; count: number }[];
      columns_total: number;
      already: { checked: number; investigate: number };
    };
    let t: Target;
    try {
      t = await this.h.api.call<Target>('review_target', target);
    } catch (e) {
      toast((e as Error).message, 'error');
      return;
    }
    const label = REVIEW_LABEL[{ unchecked: 0, checked: 1, investigate: 2 }[status]];
    if (!t.count) {
      toast(t.skipped_same ? '選んだ行は一致のレコードだけです。確認状況は差分のレコードに付けます。' : '対象のレコードがありません。');
      return;
    }
    const body: Node[] = [el('p', '', `${what}の差分 ${fmt(t.count)}件を「${label}」にします。`)];
    const kinds = [
      t.kinds.changed ? `変更 ${fmt(t.kinds.changed)}件` : '',
      t.kinds.deleted ? `左のみ ${fmt(t.kinds.deleted)}件` : '',
      t.kinds.added ? `右のみ ${fmt(t.kinds.added)}件` : '',
    ].filter(Boolean);
    body.push(el('p', '', `内訳：${kinds.join('、')}`));
    if (t.columns.length) {
      const cols = t.columns.map((c) => `${c.name}（${fmt(c.count)}件）`).join('、');
      body.push(
        el('p', '', `変わった列：${cols}${t.columns_total > t.columns.length ? ` ほか${t.columns_total - t.columns.length}列` : ''}`),
      );
    }
    const notes: string[] = [];
    if (t.already.checked) notes.push(`すでに確認済み ${fmt(t.already.checked)}件`);
    if (t.already.investigate) notes.push(`要調査 ${fmt(t.already.investigate)}件`);
    if (notes.length) body.push(el('p', 'muted', `このうち${notes.join('、')}を含みます。`));
    if (t.skipped_same) body.push(el('p', 'muted', `一致のレコード ${fmt(t.skipped_same)}件は対象外です。`));
    if (caution) body.push(el('p', '', caution));
    body.push(el('p', 'muted', 'あとから「元に戻す」（Ctrl+Z）で取り消せます。'));
    if (!(await confirmDialog('まとめて確認状況を付ける', body, `${fmt(t.count)}件を${label}にする`))) return;
    try {
      const s = await this.h.api.call<ReviewSummary & { changed: number }>('review_set', {
        token: t.token,
        status,
        label: `${fmt(t.count)}件を${label}にする`,
      });
      this.afterChange(s);
      toast(`${fmt(t.count)}件を${label}にしました。`, 'info', { label: '元に戻す', run: () => this.undo() });
    } catch (e) {
      toast((e as Error).message, 'error');
    }
  }

  /** Clears every review state and note of this comparison, as one undoable change. */
  async clearAll(): Promise<void> {
    if (!this.ready()) return;
    type Target = { token: number; count: number; already: { checked: number; investigate: number; memos: number } };
    let t: Target;
    try {
      t = await this.h.api.call<Target>('review_target', { reviewed: true });
    } catch (e) {
      toast((e as Error).message, 'error');
      return;
    }
    if (!t.count) return;
    const parts = [
      t.already.checked ? `確認済み ${fmt(t.already.checked)}件` : '',
      t.already.investigate ? `要調査 ${fmt(t.already.investigate)}件` : '',
      t.already.memos ? `メモ ${fmt(t.already.memos)}件` : '',
    ].filter(Boolean);
    const body = [
      el('p', '', `この比較で付けた確認状況とメモ（${parts.join('、')}）を消して、${fmt(t.count)}件を未確認に戻します。`),
      el('p', 'muted', 'あとから「元に戻す」（Ctrl+Z）で取り消せます。'),
    ];
    if (!(await confirmDialog('すべて未確認に戻す', body, `${fmt(t.count)}件を未確認に戻す`))) return;
    try {
      const s = await this.h.api.call<ReviewSummary>('review_set', {
        token: t.token,
        status: 'unchecked',
        memo: '',
        label: `${fmt(t.count)}件を未確認に戻す`,
      });
      this.afterChange(s);
      toast(`${fmt(t.count)}件を未確認に戻しました。`, 'info', { label: '元に戻す', run: () => this.undo() });
    } catch (e) {
      toast((e as Error).message, 'error');
    }
  }

  /** Status keys and menu items act on the selected rows, or on the selected record. */
  forSelection(p: Pane, status: ReviewStatus, advance: boolean): void {
    const g = p.sel.active ? p.sel.range : null;
    if (g && g.r1 > g.r0 && p.model) {
      this.markTarget({ view: p.model.id, start: g.r0, end: g.r1 + 1 }, status, `選んだ${fmt(g.r1 - g.r0 + 1)}行`);
      return;
    }
    const u = this.h.sel?.unit;
    if (u === undefined || u === null || u < 0) return;
    const row = p.model?.cache.get(this.h.sel?.rows[p.side] ?? -1);
    if (row && row.kind === 'same') {
      toast('一致のレコードには確認状況を付けません。');
      return;
    }
    this.set([u], status, advance);
  }

  async undo(redo = false): Promise<void> {
    if (!this.ready()) return;
    try {
      const s = await this.h.api.call<ReviewSummary & { done: { label: string; count: number } | null }>(
        redo ? 'review_redo' : 'review_undo',
      );
      if (!s.done) {
        toast(redo ? 'やり直す操作はありません。' : '元に戻す操作はありません。');
        return;
      }
      this.afterChange(s);
      toast(redo ? `「${s.done.label}」をやり直しました。` : `「${s.done.label}」を元に戻しました。`);
    } catch (e) {
      toast((e as Error).message, 'error');
    }
  }

  /** Moves to the next (or previous) unchecked difference. */
  async nextUnchecked(forward: boolean, quiet = false): Promise<void> {
    if (!this.enabled) return;
    const p = this.h.panes[this.h.active];
    if (!p.model) return;
    const t0 = performance.now();
    const cur = this.h.sel?.rows[p.side];
    const r = await this.h.api.call<{ row: number | null }>('next_review', {
      view: p.model.id,
      from: cur === undefined ? null : cur,
      forward,
      status: ['unchecked'],
    });
    if (r.row === null) {
      if (!quiet || (this.summary && this.summary.unchecked === 0))
        toast(
          this.summary && this.summary.unchecked === 0
            ? '未確認の差分はもうありません。'
            : forward
              ? 'これより後に未確認の差分はありません。'
              : 'これより前に未確認の差分はありません。',
        );
      return;
    }
    this.h.goToRow(p, r.row, t0);
  }
}
