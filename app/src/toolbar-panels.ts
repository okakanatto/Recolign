// The panels opened from the comparison toolbar: 移動 (go to a key value or
// a line of a file), 設定 (how the tables look, and the other settings) and
// 書き出す (export to Excel or CSV). Each is filled from the screen's current
// state when it opens.

import type { Backend } from './api';
import type { Scope } from './compare';
import { KIND, type Pane, type Side, type Slot } from './grid';
import { showKeys } from './help';
import { toast } from './menu';
import { otherSettings } from './prefs';
import type { Settings, SortBy, SortSpec } from './settings';
import type { GroupRef, ReviewStatus } from './sidebar';
import type { Summary } from './types';
import { button, el, fmt } from './util';

const SIDE_NAME: Record<Side, string> = { left: '左', right: '右' };

/** What the panels read from and do on the comparison screen. */
export interface PanelHost {
  readonly api: Backend;
  readonly settings: Settings;
  readonly summary: Summary;
  readonly panes: Record<Side, Pane>;
  readonly active: Side;
  readonly order: 'aligned' | 'source';
  readonly scope: Scope;
  readonly text: string;
  readonly sort: SortSpec | null;
  readonly hidden: Set<string>;
  readonly reviewFilter: ReviewStatus | null;
  readonly group: GroupRef | null;
  readonly reviewEnabled: boolean;
  saveSettings(): void;
  setZoom(z: number): void;
  zoomStep(dir: number): void;
  slotName(s: Slot): string;
  setSort(s: SortSpec | null): void;
  relayoutColumns(): void;
  repaint(): void;
  showAllColumns(): void;
  onReviewToggle(): void;
  findKey(values: string[]): void;
  findLine(side: Side, text: string): void;
  closeExport(): void;
}

export function fillGo(p: HTMLElement, h: PanelHost): void {
  const form = el('form', 'goform');
  const keyInputs = h.summary.keys.map((k) => {
    const i = el('input');
    i.type = 'text';
    i.setAttribute('aria-label', k);
    const l = el('label', 'field', k);
    l.append(i);
    return { l, i };
  });
  const kb = button('キーで移動', () => undefined);
  kb.type = 'submit';
  const kf = el('div', 'gorow');
  kf.append(...keyInputs.map((x) => x.l), kb);
  form.append(el('h3', '', 'キーの値'), kf);
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    h.findKey(keyInputs.map((x) => x.i.value));
  });

  const lform = el('form', 'goform');
  const sideSel = el('select');
  sideSel.setAttribute('aria-label', '左右');
  for (const s of ['left', 'right'] as Side[]) {
    const o = el('option', '', SIDE_NAME[s]);
    o.value = s;
    o.selected = s === h.active;
    sideSel.append(o);
  }
  const line = el('input');
  line.type = 'text';
  line.inputMode = 'numeric';
  line.setAttribute('aria-label', '行番号');
  line.size = 10;
  const lb = button('行で移動', () => undefined);
  lb.type = 'submit';
  const lr = el('div', 'gorow');
  lr.append(sideSel, line, lb);
  lform.append(el('h3', '', 'ファイルの行番号'), lr);
  lform.addEventListener('submit', (e) => {
    e.preventDefault();
    h.findLine(sideSel.value as Side, line.value);
  });
  p.replaceChildren(form, lform);
}

export function fillView(p: HTMLElement, h: PanelHost): void {
  const st = h.settings;
  const aligned = h.order === 'aligned';

  const sortSel = el('select');
  sortSel.setAttribute('aria-label', '並び順');
  sortSel.disabled = !aligned;
  const opts: [string, string, string][] = [
    ['unit', '標準', '左のファイルの順。右のファイルだけにあるレコード（右のみ）は最後に、右のファイルの順で並びます。'],
    ['key', 'キーの値', 'キーの値の文字列順（複数のキーは選んだ順に比べます）'],
    ['kind', '差分の種類', '変更・左のみ・右のみ・一致の順'],
    ['changed_count', '変更した列の数', '変更した列の数の順'],
  ];
  const cur = h.sort;
  if (cur?.by === 'column' && cur.slot !== undefined) {
    const s = h.panes.left.model?.slots[cur.slot];
    opts.push([
      'column',
      `列「${s ? h.slotName(s) : ''}」（${SIDE_NAME[cur.side ?? 'left']}）`,
      '列の値の文字列順。列名を右クリックして選びます。',
    ]);
  }
  for (const [v, label, title] of opts) {
    const o = el('option', '', label);
    o.value = v;
    o.title = title;
    o.selected = (cur?.by ?? 'unit') === v;
    sortSel.append(o);
  }
  const desc = el('input');
  desc.type = 'checkbox';
  desc.checked = !!cur?.desc;
  desc.disabled = !aligned;
  const apply = () => {
    const by = sortSel.value as SortBy;
    if (by === 'unit' && !desc.checked) h.setSort(null);
    else if (by === 'column' && cur) h.setSort({ ...cur, desc: desc.checked });
    else h.setSort({ by, desc: desc.checked });
  };
  sortSel.addEventListener('change', () => {
    if (sortSel.value === 'changed_count') desc.checked = true;
    apply();
  });
  desc.addEventListener('change', apply);
  const dl = el('label', 'check');
  dl.append(desc, document.createTextNode('降順'));
  const sortRow = el('div', 'vrow');
  const sl = el('label', 'field', '並び順');
  sl.append(sortSel);
  sortRow.append(sl, dl);
  if (!aligned) sortRow.title = '並べ替えはキー整列で使えます';

  const check = (label: string, title: string, on: boolean, f: (v: boolean) => void) => {
    const l = el('label', 'check');
    const i = el('input');
    i.type = 'checkbox';
    i.checked = on;
    i.addEventListener('change', () => f(i.checked));
    l.append(i, document.createTextNode(label));
    l.title = title;
    return l;
  };
  const changedOnly = check(
    '変更のある列だけ表示',
    '値が変わったレコードが1件以上ある列と、キーの列、片側だけの列を表示します。',
    st.changedColumnsOnly,
    (v) => {
      st.changedColumnsOnly = v;
      h.saveSettings();
      h.relayoutColumns();
    },
  );
  const spaces = check(
    '空白とタブを記号で表示',
    '半角スペースを「·」、全角スペースを「□」、タブを「→」で表示します。',
    st.showSpaces,
    (v) => {
      st.showSpaces = v;
      h.saveSettings();
      h.repaint();
    },
  );
  const nodes: Node[] = [sortRow, changedOnly, spaces];

  if (h.hidden.size) {
    const hr = el('div', 'vrow');
    hr.append(
      el('span', '', `隠した列 ${h.hidden.size}`),
      button('すべて表示する', () => h.showAllColumns()),
    );
    hr.title = [...h.hidden].join('\n');
    nodes.push(hr);
  }

  if (h.api.kind === 'desktop') {
    const z = el('div', 'vrow');
    const label = el('span', 'zoomval', `${Math.round(st.zoom * 100)}%`);
    z.append(
      el('span', '', '拡大率'),
      button('縮小', () => h.zoomStep(-1), { title: 'Ctrl+マイナス、またはCtrl+マウスホイール' }),
      label,
      button('拡大', () => h.zoomStep(1), { title: 'Ctrl+プラス、またはCtrl+マウスホイール' }),
      button('100%に戻す', () => h.setZoom(1), { title: 'Ctrl+0' }),
    );
    nodes.push(z);
  }
  nodes.unshift(el('h3', '', '表示'));
  nodes.push(
    el('h3', '', 'そのほか'),
    ...otherSettings(
      h.api,
      st,
      () => h.saveSettings(),
      () => h.onReviewToggle(),
    ),
    button('キー操作の一覧', () => showKeys(h.reviewEnabled), { cls: 'plain', title: 'F1' }),
  );
  p.replaceChildren(...nodes);
}

export function fillExport(p: HTMLElement, h: PanelHost): void {
  const pane = h.panes[h.active];
  const m = pane.model;
  if (!m) return;
  const o = h.settings.exportOptions;
  const aligned = h.order === 'aligned';
  const radio = (name: string, value: string, label: string, checked: boolean, title = '', disabled = false) => {
    const l = el('label', 'check');
    const i = el('input');
    i.type = 'radio';
    i.name = name;
    i.value = value;
    i.checked = checked;
    i.disabled = disabled;
    l.append(i, document.createTextNode(label));
    if (title) l.title = title;
    return l;
  };
  const checkbox = (label: string, on: boolean, title = '') => {
    const l = el('label', 'check');
    const i = el('input');
    i.type = 'checkbox';
    i.checked = on;
    l.append(i, document.createTextNode(label));
    if (title) l.title = title;
    return { l, i };
  };
  const row = (label: string, ...nodes: Node[]) => {
    const r = el('div', 'vrow');
    r.append(el('span', 'vlabel', label), ...nodes);
    return r;
  };
  const g = pane.sel.active ? pane.sel.range : null;
  const tooBig = m.total + 1 > 1_048_576;
  const fmtChoice = tooBig ? 'csv' : o.format;
  const formatRow = row(
    '形式',
    radio(
      'xformat',
      'xlsx',
      'Excel（差分に色を付ける）',
      fmtChoice === 'xlsx',
      tooBig
        ? 'Excelの1つのシートに入るのは1,048,575行までです。CSVを選ぶか、行を絞ってください。'
        : '変更したセルを黄色、左のみのレコードを赤、右のみのレコードを緑で塗ります。比較の条件は別のシートに書きます。',
      tooBig,
    ),
    radio('xformat', 'csv', 'CSV', fmtChoice === 'csv'),
  );
  const enc = el('select');
  enc.setAttribute('aria-label', '文字コード');
  for (const [v, label] of [
    ['utf8_bom', 'UTF-8（BOM付き）'],
    ['utf8', 'UTF-8（BOMなし）'],
    ['windows31j', 'Shift_JIS（Windows-31J）'],
  ]) {
    const op = el('option', '', label);
    op.value = v;
    op.selected = o.encoding === v;
    enc.append(op);
  }
  enc.title =
    'ExcelでCSVを開くときは「UTF-8（BOM付き）」か「Shift_JIS」を選びます。\nShift_JISで表せない文字（𠮷など）があるときは、書き出さずにお知らせします。';
  const encRow = row('文字コード', enc);
  const neutral = checkbox(
    "式として読まれる値の先頭に「'」を付ける",
    o.protectFormulas,
    "「=」「+」「-」「@」（全角を含む）で始まる値は、Excelで開くと式として扱われ、外部と通信するなどの動作をすることがあります。\n先頭に「'」を付けると、文字として表示されます。「-5」のような数には付けません。\nほかのシステムに読み込ませるためにCSVを書き出すときは、値が変わらないように外します。",
  );
  const rowsRow = row('行', radio('xrows', 'view', `表示中の行（${fmt(m.total)}行）`, true));
  if (g) rowsRow.append(radio('xrows', 'sel', `選んだ範囲（${fmt(g.r1 - g.r0 + 1)}行）`, false));
  const sideRow = aligned
    ? row(
        '値',
        radio('xside', 'both', '左右を並べる', o.side === 'both', '列ごとに、左の値の列と右の値の列を並べます'),
        radio('xside', 'left', '左だけ', o.side === 'left'),
        radio('xside', 'right', '右だけ', o.side === 'right'),
      )
    : row('値', el('span', '', `${SIDE_NAME[pane.side]}のファイルの値`));
  if (!aligned) sideRow.title = '元ファイル順では、選んでいる表の値を書き出します';
  const kind = checkbox('種類の列（変更・左のみ・右のみ・一致）を付ける', o.kind);
  const changed = checkbox('変更した列の名前を付ける', o.changed, '値が変わった列の名前を、1つの列にまとめて書きます');
  const lines = checkbox('行番号の列を付ける', o.lines, '左右それぞれのファイルでの行番号です');
  const all = checkbox('隠している列も含める', o.hidden, '「隠す」や「変更のある列だけ表示」で表示していない列も書き出します');
  const rev = checkbox('確認状況とメモの列を付ける', true);
  rev.l.hidden = !h.reviewEnabled;
  const which = (n: string) => p.querySelector<HTMLInputElement>(`input[name="${n}"]:checked`)?.value;
  const sync = () => {
    encRow.hidden = which('xformat') !== 'csv';
    neutral.l.hidden = which('xformat') !== 'csv';
  };
  p.onchange = sync;
  sync();
  const go = button(
    '保存先を選んで書き出す',
    async () => {
      const format = (which('xformat') ?? 'xlsx') as 'xlsx' | 'csv';
      const useSel = which('xrows') === 'sel' && g;
      const side = aligned ? ((which('xside') ?? 'both') as 'both' | 'left' | 'right') : pane.side;
      // Remember the choices for next time.
      Object.assign(o, {
        format: tooBig ? o.format : format,
        encoding: enc.value,
        side: aligned ? side : o.side,
        kind: kind.i.checked,
        changed: changed.i.checked,
        lines: lines.i.checked,
        hidden: all.i.checked,
        protectFormulas: neutral.i.checked,
      });
      h.saveSettings();
      // Every column: the key columns first (as on screen), then file order.
      const keyFirst = (i: number) => Number(aligned && m.slots[i].key);
      const slots = all.i.checked ? [...m.slots.keys()].sort((a, b) => keyFirst(b) - keyFirst(a)) : pane.order;
      const stem = (n: string) => n.replace(/\.[^.]*$/, '');
      const path = await h.api.pickSavePath(`比較_${stem(h.summary.left.name)}_${stem(h.summary.right.name)}.${format}`, format);
      if (!path) return;
      go.disabled = true;
      go.textContent = '書き出しています…';
      try {
        const r = await h.api.call<{ rows: number; path: string }>('export', {
          view: m.id,
          path,
          start: useSel ? g!.r0 : 0,
          end: useSel ? g!.r1 + 1 : null,
          slots,
          side,
          kind_column: kind.i.checked,
          changed_columns: changed.i.checked,
          line_numbers: lines.i.checked,
          format,
          encoding: enc.value,
          neutralize: format === 'csv' && neutral.i.checked,
          review: h.reviewEnabled && rev.i.checked,
          info: exportInfo(h, useSel ? g : null),
        });
        h.closeExport();
        toast(`${fmt(r.rows)}行を書き出しました：${r.path}`);
      } catch (e) {
        toast((e as Error).message, 'error');
      } finally {
        go.disabled = false;
        go.textContent = '保存先を選んで書き出す';
      }
    },
    { cls: 'primary' },
  );
  p.replaceChildren(formatRow, encRow, neutral.l, rowsRow, sideRow, kind.l, changed.l, lines.l, rev.l, all.l, go);
}

/** Lines describing the view, for the conditions sheet of an Excel export. */
function exportInfo(h: PanelHost, range: { r0: number; r1: number } | null): [string, string][] {
  const scope = h.scope === 'all' ? '全行' : h.scope === 'diff' ? '差分のみ（変更・左のみ・右のみ）' : `${KIND[h.scope]}の行だけ`;
  const order = h.order === 'aligned' ? 'キー整列' : `元ファイル順（${SIDE_NAME[h.active]}）`;
  const info: [string, string][] = [['表示', `${order}、${scope}`]];
  if (h.text) info.push(['検索で絞り込み', `「${h.text}」を含む行`]);
  if (h.group)
    info.push([
      '変更パターンで絞り込み',
      h.group.label + (h.group.skip.length ? `（まとめに使わない列：${h.group.skip.join('、')}）` : ''),
    ]);
  if (h.reviewFilter && h.reviewEnabled)
    info.push(['確認状況で絞り込み', { unchecked: '未確認', checked: '確認済み', investigate: '要調査' }[h.reviewFilter]]);
  if (h.order === 'aligned' && h.sort) {
    const s = h.sort;
    const by =
      s.by === 'column' && s.slot !== undefined
        ? `列「${h.slotName(h.panes.left.model!.slots[s.slot])}」（${SIDE_NAME[s.side ?? 'left']}）`
        : { unit: '標準', key: 'キーの値', kind: '差分の種類', changed_count: '変更した列の数', column: '列' }[s.by];
    info.push(['並び順', `${by}${s.desc ? '、降順' : ''}`]);
  }
  info.push(['書き出した範囲', range ? `選んだ範囲（表示の${fmt(range.r0 + 1)}〜${fmt(range.r1 + 1)}行目）` : '表示中のすべての行']);
  info.push(['書き出した日時', new Date().toLocaleString('ja-JP')]);
  return info;
}
