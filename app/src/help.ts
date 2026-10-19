// The list of keyboard operations (F1). Kept off the main screen; the
// buttons' tooltips name their keys too.

import { infoDialog } from './menu';
import { el } from './util';

type Row = [string, string];

const SECTIONS: [string, Row[], boolean?][] = [
  [
    '移動',
    [
      ['J ／ K', '次の差分 ／ 前の差分'],
      ['矢印キー、PageUp ／ PageDown', 'セルを移動する'],
      ['Ctrl+矢印キー', '値が続く範囲の端へ（空のセルでは、次に値のあるセルへ）'],
      ['Enter ／ Shift+Enter', '下 ／ 上のセルへ'],
      ['Home ／ End', '行の先頭 ／ 末尾の列へ'],
      ['Ctrl+Home ／ Ctrl+End', '最初 ／ 最後のセルへ'],
      ['F6、Ctrl+PageUp ／ Ctrl+PageDown', '左右（上下）の表を切り替える'],
      ['Ctrl+G', 'キーの値や行番号で移動する'],
    ],
  ],
  [
    '選択とコピー',
    [
      ['Shift+矢印キー、Shift+クリック', '範囲を選ぶ'],
      ['Ctrl+Shift+矢印キー', '値が続く範囲の端まで選ぶ'],
      ['Shift+Space、行番号を押す', '行を選ぶ'],
      ['Ctrl+Space、列名を押す', '列を選ぶ（Shift+クリックで、そこまでの列）'],
      ['Ctrl+A、Ctrl+Shift+Space、表の左上を押す', 'すべて選ぶ'],
      ['Ctrl+C', '選んだ範囲をコピーする'],
      ['Ctrl+Shift+C', '列名を付けてコピーする'],
      ['アプリケーションキー、Shift+F10', 'セルのメニューを開く'],
    ],
  ],
  [
    '検索',
    [
      ['Ctrl+F', '検索欄へ'],
      ['Enter ／ Shift+Enter（検索欄）', '次 ／ 前を探す'],
      ['F3 ／ Shift+F3', '次 ／ 前を探す'],
    ],
  ],
  [
    '確認状況（設定でオンのとき）',
    [
      ['Space', '確認済みにして、次の未確認へ'],
      ['2 ／ 3 ／ 1', '確認済み ／ 要調査 ／ 未確認にする'],
      ['N ／ Shift+N', '次 ／ 前の未確認へ'],
      ['M', 'メモを書く（Escで表へ戻る）'],
      ['Ctrl+Z ／ Ctrl+Y', '元に戻す ／ やり直す'],
    ],
    true,
  ],
  [
    '表示',
    [
      ['Ctrl+プラス ／ Ctrl+マイナス、Ctrl+ホイール', '拡大 ／ 縮小'],
      ['Ctrl+0', '拡大率を100%に戻す'],
      ['F1', 'この一覧を開く'],
    ],
  ],
];

export function showKeys(reviewEnabled: boolean): void {
  // One table, so the key column lines up across the sections.
  const box = el('div', 'keys');
  const t = el('table', 'keytable');
  for (const [title, rows, review] of SECTIONS) {
    if (review && !reviewEnabled) continue;
    const head = el('tr', 'ksec');
    const th = el('th', '', title);
    th.colSpan = 2;
    th.scope = 'colgroup';
    head.append(th);
    t.append(head);
    for (const [k, what] of rows) {
      const tr = el('tr');
      const kh = el('th', '', k);
      kh.scope = 'row';
      tr.append(kh, el('td', '', what));
      t.append(tr);
    }
  }
  box.append(t);
  infoDialog('キー操作', box);
}
