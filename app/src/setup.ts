// Choosing the two files, how to read them, the key and the comparison rules.

import type { Backend } from './api';
import { confirmDialog, Popover, toast } from './menu';
import { otherSettings } from './prefs';
import {
  delimiterMissing,
  layoutNotSet,
  notReady,
  putTyped,
  rawLines,
  readOptionsRows,
  takeTyped,
  type FileState,
  type ReadOptionsHost,
} from './read-options';
import type { RecentComparison, Settings } from './settings';
import type { CompareRequest, LoadOptions, Preview, SideInput } from './types';
import { button, el, fmt, mib } from './util';

type Side = 'left' | 'right';
const SIDES: Side[] = ['left', 'right'];
const SIDE_LABEL: Record<Side, string> = { left: '左（比較元）', right: '右（比較先）' };
const SIDE_SHORT: Record<Side, string> = { left: '左', right: '右' };

function when(ms: number | null): string {
  if (ms === null) return '';
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}/${p(d.getMonth() + 1)}/${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

export class SetupScreen {
  readonly root: HTMLElement;
  private sides: Record<Side, FileState | null> = { left: null, right: null };
  private cards: Record<Side, HTMLElement>;
  /** The file each card was last drawn for. */
  private drawn: Record<Side, FileState | null> = { left: null, right: null };
  /**
   * The keys and the columns not compared, as chosen. Those of the columns
   * both files have now are used: keys only all together, so that a column
   * going away for a moment (a delimiter being tried, say) never leaves a
   * key of fewer columns. Keys of a previous comparison wait here too.
   */
  private keys: string[];
  /** The keys are the user's, or a previous comparison's; until then they
   *  are guessed from the files and follow them. */
  private keysChosen = false;
  private keyTitle: HTMLElement;
  /** Why the columns or the keys chosen cannot be compared, found before comparing. */
  private keyProblem: HTMLElement;
  private excluded = new Set<string>();
  private trim = false;
  private keyBox: HTMLElement;
  private colInfo: HTMLElement;
  private exBox: HTMLElement;
  private trimBox: HTMLInputElement;
  private go: HTMLButtonElement;
  private hint: HTMLElement;
  private recentBox: HTMLElement;
  private prefs: Popover;
  private rules: HTMLDetailsElement;
  private rulesTitle: HTMLElement;
  /** Files and keys given at start: compare as soon as both files are read. */
  private autoStart = false;
  /** The reading options on the cards change a file's state through this. */
  private host: ReadOptionsHost = {
    reread: (st, change) => this.reread(st, change),
    marked: (st, controls) => this.marked(st, controls),
  };

  constructor(
    private api: Backend,
    private settings: Settings,
    private onCompare: (req: CompareRequest) => void,
    initial: CompareRequest | null,
    private save: () => void,
  ) {
    this.root = el('main', 'setup');
    const sides = el('div', 'sides');
    this.cards = { left: el('section', 'card'), right: el('section', 'card') };
    sides.append(this.cards.left, this.cards.right);
    this.recentBox = el('section', 'block recent');

    const keys = el('section', 'block');
    this.keyTitle = el('h2', '', 'キー');
    this.keyBox = el('div', 'chips');
    this.keyBox.setAttribute('role', 'group');
    this.keyBox.setAttribute('aria-label', 'キーにする列');
    this.keyProblem = el('div', 'error');
    this.keyProblem.hidden = true;
    this.colInfo = el('div', 'colinfo');
    keys.append(this.keyTitle, this.keyBox, this.keyProblem, this.colInfo);

    const rules = el('details', 'block rules');
    this.rules = rules;
    const rs = el('summary', '', '比較の設定');
    this.rulesTitle = rs;
    const exh = el('h3', '', '比較しない列');
    exh.title = '選んだ列は、値が違っても変更として数えません。違いは詳細欄に「無視」として表示します。';
    this.exBox = el('div', 'chips');
    this.exBox.setAttribute('role', 'group');
    this.exBox.setAttribute('aria-label', '比較しない列');
    const tl = el('label', 'check');
    this.trimBox = el('input');
    this.trimBox.type = 'checkbox';
    this.trimBox.addEventListener('change', () => {
      this.trim = this.trimBox.checked;
      this.renderColumns();
    });
    tl.append(this.trimBox, document.createTextNode('値の前後にある半角スペースとタブの違いを無視する'));
    tl.title = 'キーの列には適用しません。無視した違いは詳細欄に表示します。';
    const rb = el('div', 'rbody');
    rb.append(exh, this.exBox, tl);
    rules.append(rs, rb);

    const actions = el('div', 'actions');
    // The reason the button is disabled; a status, so that it is read out when it changes.
    this.hint = el('span', 'hint');
    this.hint.id = 'compareHint';
    this.hint.setAttribute('role', 'status');
    const swap = button('左右を入れ替える', () => this.swap());
    const prefsBtn = button('設定', () => undefined, { title: '記録やコピーの扱いを変えます' });
    this.prefs = new Popover(prefsBtn, '設定').whenOpened(() => {
      this.prefs.panel.replaceChildren(
        ...otherSettings(this.api, this.settings, () => {
          this.save();
          this.renderRecent();
        }),
      );
      this.showSavedReviews();
    });
    this.go = button('比較する', () => this.submit(), { cls: 'primary' });
    this.go.id = 'compareBtn';
    this.go.setAttribute('aria-describedby', this.hint.id);
    actions.append(swap, prefsBtn, el('span', 'grow'), this.hint, this.go);

    this.root.append(sides, this.recentBox, keys, rules, actions);
    // Ctrl+Enter compares. Widths typed and not used yet are used first by
    // their box (on Enter), and the file is read again: nothing is compared
    // until then, and the focus stays in the box.
    this.root.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && e.ctrlKey) this.submit();
    });

    this.keys = [];
    this.load(initial);
  }

  /** How many comparisons have saved review states, with a way to delete them. */
  private async showSavedReviews(): Promise<void> {
    type Store = { comparisons: number; set_aside: number; bytes: number; deleted: number; failed: number; in_use: number };
    let info: Store;
    try {
      info = await this.api.call<Store>('review_store', { action: 'info' });
    } catch {
      return;
    }
    if (!(info.comparisons || info.set_aside) || !this.prefs.isOpen) return;
    const row = el('div', 'vrow');
    const del = button('削除…', async () => {
      const body = [el('p', '', `${fmt(info.comparisons)}件の比較で付けた確認状況とメモを、すべて削除します。元に戻せません。`)];
      if (info.set_aside) body.push(el('p', '', `読み込めずに残しておいたファイル ${fmt(info.set_aside)}件も削除します。`));
      if (!(await confirmDialog('保存した確認状況を削除', body, '削除する'))) return;
      try {
        const r = await this.api.call<Store>('review_store', { action: 'clear' });
        row.remove();
        const left = [
          r.in_use ? `別のウィンドウで開いている比較 ${fmt(r.in_use)}件は残しました` : '',
          r.failed ? `${fmt(r.failed)}件のファイルは削除できませんでした（ほかのプログラムが使っている可能性があります）` : '',
        ].filter(Boolean);
        if (left.length) toast(`保存した確認状況を削除しました。ただし、${left.join('。')}。`, 'error');
        else toast('保存した確認状況を削除しました。');
      } catch (e) {
        toast((e as Error).message, 'error');
      }
    });
    del.title = 'このPCのユーザーフォルダに保存した、確認状況とメモのファイルを削除します。比較するファイルには触れません。';
    row.append(el('span', '', `保存した確認状況：${fmt(info.comparisons)}件の比較（${mib(info.bytes)}）`), del);
    this.prefs.panel.append(row);
  }

  /** Fills the screen with a previous comparison's files and conditions. */
  private load(req: CompareRequest | null): void {
    if (req) {
      this.keys = [...req.keys];
      this.keysChosen = true;
      this.excluded = new Set(req.excluded);
      this.trim = req.trim !== 'none';
      this.trimBox.checked = this.trim;
      this.rules.open = this.excluded.size > 0 || this.trim;
    }
    for (const side of SIDES) {
      if (req) this.setFile(side, req[side].path, req[side].options);
      else this.renderCard(side);
    }
    this.renderRecent();
    this.renderColumns();
  }

  private renderRecent(): void {
    const list = this.settings.recent;
    // Shown while choosing from scratch; hidden once a file is chosen.
    this.recentBox.hidden = !list.length || !!this.sides.left || !!this.sides.right;
    if (this.recentBox.hidden) return;
    const base = (p: string) => p.split(/[\\/]/).pop() ?? p;
    const shortWhen = (ms: number) => {
      const d = new Date(ms);
      return `${d.getMonth() + 1}/${d.getDate()} ${d.getHours()}:${String(d.getMinutes()).padStart(2, '0')}`;
    };
    const items = list.map((r: RecentComparison, i) => {
      const li = el('li');
      const b = button('', () => {
        this.autoStart = false;
        this.load(r);
      });
      b.className = 'recentbtn';
      b.append(
        el('span', 'rfiles', `${base(r.left.path)} と ${base(r.right.path)}`),
        el('span', 'muted', `キー ${r.keys.join(' + ')}`),
        el('span', 'muted', shortWhen(r.at)),
      );
      b.title = `左：${r.left.path}\n右：${r.right.path}\nクリックすると、このファイルと条件を読み込みます。`;
      const x = button(
        '×',
        () => {
          this.settings.recent.splice(i, 1);
          this.save();
          this.renderRecent();
        },
        { cls: 'rdel', title: '一覧から外します（ファイルは消えません）' },
      );
      x.setAttribute('aria-label', '一覧から外す');
      li.append(b, x);
      return li;
    });
    const ul = el('ul');
    ul.append(...items);
    this.recentBox.replaceChildren(el('h2', '', '最近の比較'), ul);
  }

  destroy(): void {
    this.prefs.destroy();
  }

  /**
   * Files given when the app was started (dropped on the exe, Explorer's
   * 送る, or the command line). With keys, the comparison starts as soon
   * as both files are read and have those columns.
   */
  open(paths: string[], keys: string[]): void {
    if (keys.length) {
      this.keys = [...keys];
      this.keysChosen = true;
      this.autoStart = paths.length >= 2;
    }
    this.place(paths, null);
  }

  /** Files dropped on the window. `target` is the element under the pointer. */
  drop(paths: string[], target: Element | null): void {
    // A file chosen by hand is not to be compared on its own.
    this.autoStart = false;
    this.place(paths, target);
  }

  private place(paths: string[], target: Element | null): void {
    if (!paths.length) return;
    if (paths.length >= 2) {
      this.setFile('left', paths[0], null);
      this.setFile('right', paths[1], null);
      if (paths.length > 2) toast(`3つ目以降のファイル（${paths.length - 2}件）は使いません。`);
      return;
    }
    const card = target?.closest('.card');
    const side: Side =
      card === this.cards.right
        ? 'right'
        : card === this.cards.left
          ? 'left'
          : this.sides.left
            ? this.sides.right
              ? 'left'
              : 'right'
            : 'left';
    this.setFile(side, paths[0], null);
  }

  private async pick(side: Side): Promise<void> {
    const path = await this.api.pickFile();
    if (!path) return;
    this.autoStart = false;
    this.setFile(side, path, null);
  }

  /** A file chosen for a side, read with these options or with the ones suggested. */
  private setFile(side: Side, path: string, options: LoadOptions | null): void {
    const st: FileState = { path, options, shown: null, error: null, reading: false, seq: 0 };
    this.sides[side] = st;
    this.renderCard(side);
    this.renderRecent();
    this.renderColumns();
    void this.read(st);
  }

  /** Back to no file on a side. A reading of the file still under way is not used. */
  private clearFile(side: Side): void {
    this.sides[side] = null;
    this.autoStart = false;
    this.renderCard(side);
    this.renderRecent();
    this.renderColumns();
  }

  private sideOf(st: FileState): Side | null {
    return this.sides.left === st ? 'left' : this.sides.right === st ? 'right' : null;
  }

  /** Reads a file again with options changed on its card. The card stays as
   *  it is meanwhile, so that a click on it is not lost. */
  private reread(st: FileState, change: Partial<LoadOptions>): void {
    if (!st.options) return;
    if ('fixed' in change || 'delimiter' in change) {
      st.draft = undefined;
      st.invalid = undefined;
      st.fixedChoice = undefined;
    }
    st.options = { ...st.options, ...change };
    // Someone setting options by hand is not to be overtaken by the start.
    this.autoStart = false;
    void this.read(st);
  }

  /** A layout being set, or text that cannot be used, on a card. */
  private marked(st: FileState, controls: boolean): void {
    const side = this.sideOf(st);
    if (!side) return;
    this.autoStart = false;
    if (controls) this.renderCard(side);
    else this.cards[side].querySelector('.below')?.replaceChildren(...this.below(st));
    this.renderColumns();
  }

  /** Reads a file with the options last asked for. Only the answer to the
   *  last reading asked for is used. */
  private async read(st: FileState): Promise<void> {
    const seq = ++st.seq;
    st.reading = true;
    this.updateGo();
    let p: Preview | null = null;
    let failed = '';
    try {
      p = await this.api.call<Preview>('preview', { path: st.path, options: st.options });
    } catch (e) {
      failed = (e as Error).message;
    }
    if (seq !== st.seq) return;
    st.reading = false;
    // The sides may have been swapped or the file replaced meanwhile.
    const side = this.sideOf(st);
    if (p) {
      st.shown = p;
      st.options = p.options;
    } else if (st.shown) {
      // The file could not be read again: keep what was read before.
      st.options = st.shown.options;
      if (side) toast(`${SIDE_SHORT[side]}のファイル：${failed}`, 'error');
    } else {
      st.error = failed;
    }
    if (!side) return;
    this.renderCard(side);
    this.renderColumns();
    this.startIfAsked();
  }

  private swap(): void {
    const { left, right } = this.sides;
    this.sides = { left: right, right: left };
    this.renderCard('left');
    this.renderCard('right');
    this.renderColumns();
  }

  private renderCard(side: Side): void {
    const card = this.cards[side];
    card.dataset.side = side;
    const st = this.sides[side];
    // The card is drawn anew. For the same file, the focus goes back to the
    // same control, text typed and not used yet stays, and so do the places
    // scrolled to in the records and the lines.
    const active = document.activeElement;
    const focused = active instanceof HTMLElement && card.contains(active) ? active : null;
    const typed = takeTyped(card);
    const same = this.drawn[side] === st;
    const scrolled = [...card.querySelectorAll<HTMLElement>('.pv, .rawlines')].map(
      (e) => [e.className, e.scrollLeft, e.scrollTop] as const,
    );
    this.drawn[side] = st;

    const head = el('div', 'chead');
    head.append(el('h2', '', SIDE_LABEL[side]));
    const pick = button(st ? '別のファイルを選ぶ' : 'ファイルを選ぶ', () => this.pick(side), { cls: st ? '' : 'primary' });
    pick.dataset.k = 'pick';
    head.append(el('span', 'grow'), pick);
    if (st) {
      const clear = button('×', () => this.clearFile(side), { cls: 'cclear', title: 'このファイルの選択を解除します' });
      clear.setAttribute('aria-label', `${SIDE_SHORT[side]}のファイルの選択を解除`);
      head.append(clear);
    }
    const nodes: Node[] = [head];
    if (!st) {
      const empty = el('div', 'dropzone');
      empty.append(
        el(
          'div',
          '',
          this.api.kind === 'desktop'
            ? 'ファイルをここにドロップするか、「ファイルを選ぶ」を押します。'
            : '「ファイルを選ぶ」を押して、ファイルのパスを入力します。',
        ),
      );
      nodes.push(empty);
    } else {
      const p = st.shown;
      nodes.push(this.fileInfo(st));
      if (st.error) {
        nodes.push(el('div', 'error', st.error));
      } else if (!p) {
        nodes.push(el('div', 'muted', '読み込んでいます…'));
      } else {
        const below = el('div', 'below');
        below.append(...this.below(st));
        nodes.push(...readOptionsRows(st, p, this.host), below);
      }
    }
    card.replaceChildren(...nodes);

    if (same) {
      putTyped(card, typed);
      for (const [cls, left, top] of scrolled) {
        const e = card.querySelector<HTMLElement>(`.${cls}`);
        if (e) [e.scrollLeft, e.scrollTop] = [left, top];
      }
    }
    if (!focused) return;
    const find = (k: string | undefined) => (k ? card.querySelector<HTMLElement>(`[data-k="${k}"]`) : null);
    // A control that is gone (the box of a delimiter, say) leaves the focus on the layout.
    const target = find(focused.dataset.k) ?? find('del') ?? pick;
    target.focus();
    if (target instanceof HTMLInputElement && target.type === 'text' && focused instanceof HTMLInputElement)
      target.setSelectionRange(focused.selectionStart, focused.selectionEnd);
  }

  /** What a card shows below the reading options: the first records, or
   *  what is wrong with the options. */
  private below(st: FileState): HTMLElement[] {
    const p = st.shown!;
    const unset = layoutNotSet(st, p) ?? delimiterMissing(p);
    if (unset) return unset;
    const nodes: HTMLElement[] = [];
    if (p.error) nodes.push(el('div', 'error', p.error));
    nodes.push(this.previewTable(p));
    if (p.options.fixed) nodes.push(rawLines(p));
    return nodes;
  }

  /** The name, size, date and BOM of a file. */
  private fileInfo(st: FileState): HTMLElement {
    const info = el('div', 'finfo');
    const p = st.shown;
    const name = el('b', 'fname', p?.name ?? st.path.split(/[\\/]/).pop() ?? st.path);
    name.title = st.path;
    info.append(name);
    if (!p) return info;
    info.append(el('span', 'muted', mib(p.bytes)));
    if (p.modified_unix_ms !== null) {
      const m = el('span', 'muted', when(p.modified_unix_ms));
      m.title = '更新日時';
      info.append(m);
    }
    if (p.bom) {
      const b = el('span', 'muted', 'BOMあり');
      b.title = 'ファイルの先頭に、文字コードを示す印（BOM）があります。値には含めません。';
      info.append(b);
    }
    return info;
  }

  private previewTable(p: Preview): HTMLElement {
    const wrap = el('div', 'pv');
    const t = el('table');
    const hr = el('tr');
    p.headers.forEach((h) => hr.append(el('th', '', h)));
    t.append(hr);
    for (const r of p.rows) {
      const tr = el('tr');
      r.forEach((v) => tr.append(el('td', v === '' ? 'empty' : '', v)));
      t.append(tr);
    }
    wrap.append(t);
    const note = p.partial ? `先頭の${fmt(p.rows.length)}件（ファイルの一部だけを読んでいます）` : `先頭の${fmt(p.rows.length)}件`;
    wrap.title = note;
    return wrap;
  }

  /** The columns of the two files, once both are read and their layouts set. */
  private common(): { names: string[]; leftOnly: string[]; rightOnly: string[] } | null {
    const { left, right } = this.sides;
    if (!left || !right || notReady(left) || notReady(right)) return null;
    const l = left.shown;
    const r = right.shown;
    if (!l || !r || l.error || r.error) return null;
    const rs = new Set(r.headers);
    const ls = new Set(l.headers);
    return {
      names: l.headers.filter((h) => rs.has(h)),
      leftOnly: l.headers.filter((h) => !rs.has(h)),
      rightOnly: r.headers.filter((h) => !ls.has(h)),
    };
  }

  /** The keys and the columns not compared, of the columns both files have now. */
  private selection(names: string[]): { keys: string[]; excluded: string[] } {
    const has = new Set(names);
    const keys = this.keys.every((k) => has.has(k)) ? this.keys : [];
    return { keys, excluded: names.filter((n) => this.excluded.has(n) && !keys.includes(n)) };
  }

  private renderColumns(): void {
    const c = this.common();
    const sel = c ? this.selection(c.names) : null;
    // Rules changed from the default show on the closed section too.
    const changed = [sel?.excluded.length ? `比較しない列 ${sel.excluded.length}` : '', this.trim ? '前後の空白の違いを無視' : ''].filter(
      Boolean,
    );
    this.rulesTitle.textContent = '比較の設定' + (changed.length ? `（${changed.join('、')}）` : '');
    // The chips are drawn anew: the focus goes back to the same chip.
    const active = document.activeElement;
    const focused =
      active instanceof HTMLElement && active.matches('.chip') ? { box: active.parentElement, name: active.dataset.name } : null;
    this.keyTitle.textContent = 'キー';
    this.keyTitle.title =
      '左右のレコードを対応づける列です。値が完全に一致するレコード同士を比べます。\n' +
      '複数の列を選ぶと、選んだ順の組み合わせで対応づけます。';
    if (!c || !sel) {
      const waiting = this.columnsWaitingFor();
      this.keyBox.replaceChildren(el('span', 'muted', waiting));
      this.colInfo.replaceChildren();
      this.exBox.replaceChildren(el('span', 'muted', waiting));
      this.showKeyProblem(null);
      this.updateGo();
      return;
    }
    if (!this.keysChosen) {
      // No key chosen yet: a column whose values are all filled and different in both files.
      const guess = this.guessKey(c.names);
      this.keys = guess ? [guess] : [];
      sel.keys = this.keys;
      if (guess) {
        this.keyTitle.textContent = 'キー（推定）';
        this.keyTitle.title =
          '左右のファイルの読み込んだ範囲で、値がすべて埋まっていて重複しない列を選びました。違うときは選び直してください。';
      }
    }

    this.keyBox.replaceChildren(
      ...c.names.map((n) => {
        const i = sel.keys.indexOf(n);
        const b = button('', () => {
          // A choice made now replaces keys of columns not there, and a
          // guess: another column is not added to it.
          const base = this.keysChosen ? sel.keys : sel.keys.filter((k) => k === n);
          this.keysChosen = true;
          this.keys = base.filter((k) => k !== n);
          if (i < 0) {
            this.keys.push(n);
            this.excluded.delete(n);
          }
          this.renderColumns();
        });
        b.className = 'chip';
        b.dataset.name = n;
        b.setAttribute('aria-pressed', String(i >= 0));
        if (i >= 0 && sel.keys.length > 1) b.append(el('span', 'ord', String(i + 1)));
        b.append(document.createTextNode(n));
        return b;
      }),
    );
    if (!c.names.length) this.keyBox.replaceChildren();
    this.showKeyProblem(this.columnProblem(c.names) ?? this.keyIssue(sel.keys));

    const info: Node[] = [];
    if (c.leftOnly.length) info.push(el('span', '', `左だけにある列：${c.leftOnly.join('、')}`));
    if (c.rightOnly.length) info.push(el('span', '', `右だけにある列：${c.rightOnly.join('、')}`));
    this.colInfo.replaceChildren(...info);
    this.colInfo.title = info.length ? '片側にしかない列は、値を比べずに表示だけします。' : '';

    this.exBox.replaceChildren(
      ...c.names
        .filter((n) => !sel.keys.includes(n))
        .map((n) => {
          const on = this.excluded.has(n);
          const b = button(n, () => {
            if (on) this.excluded.delete(n);
            else this.excluded.add(n);
            this.renderColumns();
          });
          b.className = 'chip';
          b.dataset.name = n;
          b.setAttribute('aria-pressed', String(on));
          return b;
        }),
    );
    if (focused?.box === this.keyBox || focused?.box === this.exBox) {
      const chip = focused.name && focused.box.querySelector<HTMLElement>(`.chip[data-name="${CSS.escape(focused.name)}"]`);
      (chip || focused.box.querySelector<HTMLElement>('.chip'))?.focus();
    }
    this.updateGo();
  }

  /** A key: the first column, in the left file's order, whose values are all
   *  filled and different in what both files show; one named like a key
   *  (番号, コード, ID…) first. */
  private guessKey(names: string[]): string | null {
    const at = SIDES.map((s) => {
      const p = this.sides[s]!.shown!;
      return { p, col: new Map(p.headers.map((h, i) => [h, i])) };
    });
    const fine = (n: string) =>
      at.every(({ p, col }) => {
        const i = col.get(n);
        return i !== undefined && p.rows.length > 0 && p.key_issues[i] === null;
      });
    const candidates = names.filter(fine);
    // 社員ID, user_id, No., ＩＤ: an id at the end of the name, not inside a word (userid is not one).
    const named = /番号|コード|ｺｰﾄﾞ|キー|ＩＤ|Ｎｏ|(^|[^a-z])(id|no\.?|code|key|cd)$/i;
    const contact = /電話|携帯|ＦＡＸ|FAX|郵便|口座|phone|e-?mail|zip/i; // numbers, but not of the record
    return candidates.find((n) => named.test(n) && !contact.test(n)) ?? candidates[0] ?? null;
  }

  /** Column names that keep the columns from being paired: none in common,
   *  names empty or repeated in a file. `null` if there is no such problem. */
  private columnProblem(names: string[]): string | null {
    const fix = '列名を1つずつ違う名前にしてください。1行目が列名でないファイルは、読み込みの設定で「1行目は列名」を外します。';
    for (const s of SIDES) {
      const headers = this.sides[s]!.shown!.headers;
      const empty = headers.indexOf('');
      if (empty >= 0) return `${SIDE_SHORT[s]}のファイルの${empty + 1}列目は、列名が空です。${fix}`;
      const first = new Map<string, number>();
      for (const [i, h] of headers.entries()) {
        const j = first.get(h);
        if (j !== undefined) return `${SIDE_SHORT[s]}のファイルに、同じ列名「${h}」が2つあります（${j + 1}列目と${i + 1}列目）。${fix}`;
        first.set(h, i);
      }
    }
    if (names.length) return null;
    const [l, r] = SIDES.map((s) => this.sides[s]!.options!.has_header);
    return l !== r
      ? `左右のファイルに、同じ名前の列がありません。「1行目は列名」が${l ? '右' : '左'}のファイルだけオフになっています。左右でそろえてください。`
      : '左右のファイルに、同じ名前の列がありません。列名の行と、読み込みの設定（文字コード・区切り）を確かめてください。';
  }

  /** What the files show of the keys chosen that keeps them from telling
   *  records apart: a value on two records (for a key of one column), or no
   *  value. `null` if nothing shows. */
  private keyIssue(keys: string[]): string | null {
    for (const k of keys) {
      for (const s of SIDES) {
        const p = this.sides[s]!.shown!;
        const issue = p.key_issues[p.headers.indexOf(k)];
        if (issue?.kind === 'empty')
          return `キーの列「${k}」の値が、${SIDE_SHORT[s]}のファイルの${fmt(issue.line)}行目で空です。キーには、値が空にならない列を選んでください。`;
        if (issue?.kind === 'duplicate' && keys.length === 1)
          return (
            `キーの列「${k}」の値「${issue.value}」が、${SIDE_SHORT[s]}のファイルの${fmt(issue.lines[0])}行目と${fmt(issue.lines[1])}行目にあります。` +
            'キーには、1件ずつ値が違う列を選んでください。1つの列で区別できないときは、キーの列を増やして組み合わせます。'
          );
      }
    }
    return null;
  }

  private showKeyProblem(text: string | null): void {
    this.keyProblem.hidden = !text;
    this.keyProblem.textContent = text ?? '';
  }

  /** In place of the columns, while there are none to show: what they wait for. */
  private columnsWaitingFor(): string {
    const files = SIDES.map((s) => this.sides[s]);
    if (files.some((st) => !st)) return '左右のファイルを選ぶと、両方にある列をここに表示します。';
    if (files.some((st) => st!.error)) return '別のファイルを選ぶと、両方にある列をここに表示します。';
    if (files.some((st) => !st!.shown)) return '読み込んでいます…';
    return '読み込みの設定を終えると、両方にある列をここに表示します。';
  }

  /** Why the files cannot be compared as on screen yet; `null` if they can. */
  private blocker(): string | null {
    const { left: l, right: r } = this.sides;
    if (!l || !r) return '左右のファイルを選んでください。';
    if (l.reading || r.reading) return '読み込んでいます…';
    const unread = SIDES.filter((s) => this.sides[s]!.error);
    if (unread.length) return `${unread.map((s) => SIDE_SHORT[s]).join('と')}のファイルを読めません。別のファイルを選んでください。`;
    const unset = SIDES.flatMap((s) => {
      const why = notReady(this.sides[s]!);
      return why ? [`${SIDE_SHORT[s]}のファイルの${why}`] : [];
    });
    if (unset.length) return unset.join('');
    const wrong = SIDES.filter((s) => this.sides[s]!.shown?.error);
    if (wrong.length) return `${wrong.map((s) => SIDE_SHORT[s]).join('と')}のファイルの読み込みの設定を直してください。`;
    const c = this.common();
    if (c && this.columnProblem(c.names)) return '列名を直してください。';
    const keys = c ? this.selection(c.names).keys : [];
    if (!keys.length) return 'キーにする列を選んでください。';
    if (this.keyIssue(keys)) return 'キーにする列を選び直してください。';
    return null;
  }

  private updateGo(): void {
    const why = this.blocker();
    this.hint.textContent = why ?? '';
    this.go.disabled = why !== null;
  }

  /** Files and keys given at start: once both files are read, compare, or say why not. */
  private startIfAsked(): void {
    const { left: l, right: r } = this.sides;
    if (!this.autoStart || !l || !r || l.reading || r.reading) return;
    this.autoStart = false;
    const unread = SIDES.filter((s) => this.sides[s]!.error || this.sides[s]!.shown?.error);
    if (!this.blocker()) this.submit();
    else if (unread.length)
      toast(`${unread.map((s) => SIDE_SHORT[s]).join('と')}のファイルを読み込めないため、比較を始めませんでした。`, 'error');
    else {
      const c = this.common();
      const problem = c && (this.columnProblem(c.names) ?? this.keyIssue(this.selection(c.names).keys));
      toast(
        problem
          ? `比較を始めませんでした。${problem}`
          : '指定されたキーの列が両方のファイルにないため、比較を始めませんでした。キーを選んでください。',
        problem ? 'error' : 'info',
      );
    }
  }

  private submit(): void {
    const { left: l, right: r } = this.sides;
    const c = this.common();
    if (this.blocker() || !l?.options || !r?.options || !c) return;
    const sel = this.selection(c.names);
    const side = (s: FileState): SideInput => ({ path: s.path, options: s.options! });
    this.onCompare({
      left: side(l),
      right: side(r),
      keys: [...sel.keys],
      excluded: sel.excluded,
      trim: this.trim ? 'ascii_space_tab' : 'none',
    });
  }
}
