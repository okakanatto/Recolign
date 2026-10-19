// Settings that are not about how the tables look: shown in the settings
// panel of the comparison screen and on the setup screen.

import type { Backend } from './api';
import { textDialog, toast } from './menu';
import type { Settings } from './settings';
import { button, el } from './util';

export function check(label: string, title: string, on: boolean, f: (v: boolean) => void): HTMLLabelElement {
  const l = el('label', 'check');
  const i = el('input');
  i.type = 'checkbox';
  i.checked = on;
  i.addEventListener('change', () => f(i.checked));
  l.append(i, document.createTextNode(label));
  l.title = title;
  return l;
}

export function otherSettings(api: Backend, st: Settings, save: () => void, onReview?: () => void): Node[] {
  const nodes: Node[] = [
    check(
      '確認状況とメモを使う',
      '差分のレコードに「確認済み」「要調査」とメモを付けられるようにします。\n付けた内容は、同じファイルと同じ条件で比較し直したときに自動で読み込みます（このPCのユーザーフォルダに保存）。',
      st.reviewEnabled,
      (v) => {
        st.reviewEnabled = v;
        save();
        onReview?.();
      },
    ),
    check(
      '最近の比較を記録する',
      'ファイルの場所とキーの列名を、このPCのユーザーフォルダに記録し、ファイル選択の画面に出します。\n外すと、記録してあるものも消します。',
      st.keepHistory,
      (v) => {
        st.keepHistory = v;
        if (!v) st.recent = [];
        save();
      },
    ),
  ];
  if (api.kind === 'desktop') {
    nodes.push(
      check(
        'コピーした内容をクリップボードの履歴と同期に残さない',
        'Windowsのクリップボード履歴（Win+V）や、ほかの端末との同期に残らないようにします。機密データを扱うときに使います。',
        st.clipboardExclude,
        (v) => {
          st.clipboardExclude = v;
          save();
        },
      ),
    );
  }
  const version = el('div', 'muted small');
  api
    .call<{ version: string }>('app_info')
    .then((i) => (version.textContent = `Recolign ${i.version}`))
    .catch(() => undefined);
  nodes.push(version);
  nodes.push(
    button('ライセンス情報', () => showLicenses(), {
      cls: 'plain',
      title: 'このアプリと、このアプリに含まれているソフトウェアのライセンスを表示します',
    }),
  );
  return nodes;
}

async function showLicenses(): Promise<void> {
  let text: string;
  try {
    const r = await fetch('./third-party-licenses.txt');
    if (!r.ok) throw new Error(String(r.status));
    text = await r.text();
  } catch {
    toast('ライセンス情報を読み込めませんでした。', 'error');
    return;
  }
  textDialog('ライセンス情報', text);
}
