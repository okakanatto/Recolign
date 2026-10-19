// Screens: choosing files → progress → comparison (or the reason it failed).

import './style.css';
import { createBackend, type Backend } from './api';
import { CompareScreen } from './compare';
import { metrics, type Side } from './grid';
import { closeMenu, confirmDialog, openMenu, toast } from './menu';
import { mergeSettings, RECENT_MAX, ZOOM_STEPS, type Settings } from './settings';
import { SetupScreen } from './setup';
import { requestOf, type CompareRequest, type FailedInfo, type KeyProblems, type Status, type Summary } from './types';
import { button, clamp, el, fmt, gib, mib } from './util';

declare global {
  interface Window {
    __cdr: any;
  }
}

const STAGE: Record<string, string> = {
  idle: '準備しています',
  reading: 'ファイルを読み込んでいます',
  validating_keys: 'キーを確認しています',
  comparing: '左右のレコードを比べています',
  patterns: '表示を準備しています',
  done: '表示を準備しています',
};

class App {
  private root = document.getElementById('app')!;
  private screen: { root: HTMLElement; destroy?: () => void } | null = null;
  private setup: SetupScreen | null = null;
  private saveTimer = 0;

  constructor(
    private api: Backend,
    readonly settings: Settings,
  ) {}

  private show(s: { root: HTMLElement; destroy?: () => void }): void {
    closeMenu();
    this.screen?.destroy?.();
    this.screen = s;
    this.setup = s instanceof SetupScreen ? s : null;
    this.root.replaceChildren(s.root);
  }

  /** Saves shortly after the last change (zooming or resizing changes settings many times). */
  saveSettings(): void {
    clearTimeout(this.saveTimer);
    this.saveTimer = window.setTimeout(() => this.flushSettings(), 300);
  }

  flushSettings(): void {
    clearTimeout(this.saveTimer);
    this.saveTimer = 0;
    this.api.saveSettings(this.settings);
  }

  async setZoom(z: number): Promise<void> {
    this.settings.zoom = z;
    const handled = await this.api.setZoom(z);
    if (!handled) return;
    this.saveSettings();
    toast(`拡大率 ${Math.round(z * 100)}%`);
  }

  showSetup(initial: CompareRequest | null): void {
    window.__cdr.ready = false;
    this.api.setTitle('Recolign');
    this.show(
      new SetupScreen(
        this.api,
        this.settings,
        (req) => this.run(req),
        initial,
        () => this.saveSettings(),
      ),
    );
  }

  async run(req: CompareRequest): Promise<void> {
    if (!(await this.memoryOk(req))) return;
    // Remember the comparison for the setup screen's list.
    const same = (a: CompareRequest) => a.left.path === req.left.path && a.right.path === req.right.path;
    if (this.settings.keepHistory) {
      this.settings.recent = [{ ...structuredClone(req), at: Date.now() }, ...this.settings.recent.filter((r) => !same(r))].slice(
        0,
        RECENT_MAX,
      );
    }
    this.flushSettings();
    try {
      await this.api.call('compare', req);
    } catch (e) {
      toast((e as Error).message, 'error');
      return;
    }
    this.progress(req);
  }

  /**
   * Before reading, compares the memory the files are expected to need with
   * what is available, and asks when it may not fit (U-13).
   */
  private async memoryOk(req: CompareRequest): Promise<boolean> {
    type Check = { needed_bytes: number; available_bytes: number | null; enough: boolean };
    let c: Check;
    try {
      c = await this.api.call<Check>('memory_check', { left: req.left.path, right: req.right.path });
    } catch {
      return true;
    }
    if (c.enough || c.available_bytes === null) return true;
    return confirmDialog(
      'メモリが足りない可能性があります',
      [
        el(
          'p',
          '',
          `この比較には、約${gib(c.needed_bytes)}のメモリが必要と見込まれます。いま空いているメモリは約${gib(c.available_bytes)}です。`,
        ),
        el(
          'p',
          '',
          'ほかのアプリを閉じると、空きが増えます。このまま比較すると、パソコン全体が遅くなったり、途中で止まったりすることがあります。',
        ),
      ],
      'このまま比較する',
    );
  }

  /** Waits for the comparison started by `run` (or by the harness). */
  async progress(req: CompareRequest | null): Promise<void> {
    const root = el('main', 'progress');
    const title = el('h1', '', STAGE.idle);
    const rows = el('div', 'prows');
    const cancel = button('中止する', () => this.api.call('cancel').catch(() => undefined));
    root.append(title, rows, cancel);
    root.hidden = true;
    this.show({ root });
    const shownAt = performance.now() + 300; // fast comparisons skip this screen
    const bar = (label: string, done: number, total: number, text: string) => {
      const r = el('div', 'prow');
      const b = el('div', 'bar');
      const f = el('div', 'fill');
      f.style.width = total ? `${clamp((done / total) * 100, 0, 100)}%` : '0';
      b.append(f);
      r.append(el('span', 'plabel', label), b, el('span', 'ptext', text));
      return r;
    };
    for (;;) {
      let st: Status;
      try {
        st = await this.api.call<Status>('status');
      } catch (e) {
        toast((e as Error).message, 'error');
        await new Promise((r) => setTimeout(r, 500));
        continue;
      }
      if (this.screen?.root !== root) return;
      const ph = st.phase;
      if (ph.state === 'ready') return this.showCompare(ph.summary);
      if (ph.state === 'failed') {
        if (ph.error.code === 'cancelled') {
          toast('比較を中止しました。');
          return this.showSetup(req);
        }
        return this.showFailed(ph.error, req);
      }
      if (ph.state === 'idle') return this.showSetup(req);
      title.textContent = STAGE[st.stage] ?? STAGE.idle;
      const name = (s: Side) => req?.[s].path.split(/[\\/]/).pop() ?? (s === 'left' ? '左' : '右');
      const items = (['left', 'right'] as Side[]).map((s) => {
        const p = st[s];
        return bar(name(s), p.bytes_read, p.bytes_total, `${mib(p.bytes_read)} / ${mib(p.bytes_total)}`);
      });
      if (st.units_total)
        items.push(bar('比較', st.units_compared, st.units_total, `${fmt(st.units_compared)} / ${fmt(st.units_total)}件`));
      rows.replaceChildren(...items);
      if (performance.now() > shownAt) root.hidden = false;
      await new Promise((r) => setTimeout(r, 100));
    }
  }

  /**
   * Why the comparison could not be made: the reason in one sentence, where
   * it shows (records, columns), and how to put it right. The engine's
   * message gives them as its first line, the lines between, and its last
   * line; a message of one line, as its first sentence and the rest.
   */
  private showFailed(err: FailedInfo, req: CompareRequest | null): void {
    window.__cdr.ready = false;
    const root = el('main', 'failed');
    root.append(el('h1', '', '比較できませんでした'));
    const lines = err.message.split('\n');
    let reason = lines[0];
    let fix = lines.length > 1 ? lines[lines.length - 1] : '';
    if (lines.length === 1) {
      const end = sentenceEnd(reason);
      if (end > 0 && end < reason.length) [reason, fix] = [reason.slice(0, end), reason.slice(end)];
    }
    root.append(el('p', 'reason', reason));
    if (err.code === 'key_validation' && err.detail) {
      const d = err.detail;
      for (const [label, name, p] of [
        ['左', d.left_name, d.left],
        ['右', d.right_name, d.right],
      ] as [string, string, KeyProblems][]) {
        if (!p.duplicate_groups_total && !p.empty_total) continue;
        root.append(this.keyProblems(`${label}のファイル（${name}）`, p));
      }
    } else if (lines.length > 2) {
      const list = el('ul', 'where');
      list.append(...lines.slice(1, -1).map((l) => el('li', '', l.trim())));
      root.append(list);
    }
    if (fix) root.append(el('h2', '', '直し方'), el('p', 'fix', fix));
    const back = button('条件を変更する', () => this.showSetup(req), { cls: 'primary' });
    root.append(back);
    this.show({ root });
    back.focus();
  }

  private keyProblems(title: string, p: KeyProblems): HTMLElement {
    const box = el('section', 'kp');
    box.append(el('h2', '', title));
    if (p.duplicate_groups_total) {
      box.append(el('h3', '', `同じキーのレコード ${fmt(p.duplicate_groups_total)}組（${fmt(p.duplicate_records_total)}件）`));
      const t = el('table');
      const h = el('tr');
      h.append(el('th', '', 'キーの値'), el('th', '', '行番号'));
      t.append(h);
      for (const g of p.duplicate_groups) {
        const tr = el('tr');
        tr.append(el('td', '', g.key.join(' / ')), el('td', 'num', g.records.map((r) => fmt(r.line_start)).join('、')));
        t.append(tr);
      }
      box.append(t);
      const rest = p.duplicate_groups_total - p.duplicate_groups.length;
      if (rest > 0) box.append(el('p', 'muted', `ほかに${fmt(rest)}組あります。`));
    }
    if (p.empty_total) {
      box.append(el('h3', '', `キーが空のレコード ${fmt(p.empty_total)}件`));
      box.append(el('p', 'num', '行番号 ' + p.empty.map((r) => fmt(r.line_start)).join('、')));
      const rest = p.empty_total - p.empty.length;
      if (rest > 0) box.append(el('p', 'muted', `ほかに${fmt(rest)}件あります。`));
    }
    return box;
  }

  private async showCompare(summary: Summary): Promise<void> {
    const host = {
      settings: this.settings,
      saveSettings: () => this.saveSettings(),
      setZoom: (z: number) => this.setZoom(z),
      onChangeConditions: () => {
        const req = requestOf(summary);
        this.api.call('reset').catch(() => undefined);
        this.showSetup(req);
      },
    };
    const c = new CompareScreen(this.api, host, summary);
    this.api.setTitle(`${summary.left.name} と ${summary.right.name} - Recolign`);
    this.show(c);
    window.__cdr.state = c;
    await c.start();
    window.__cdr.ready = true;
  }

  /** Files dropped on the window (desktop). */
  /** Files the app was started with. */
  openFiles(paths: string[], keys: string[]): void {
    this.setup?.open(paths, keys);
  }

  onDrop(paths: string[], x: number, y: number): void {
    if (this.setup) this.setup.drop(paths, document.elementFromPoint(x, y));
    else toast('ファイルを替えるときは「条件を変更」を押してから、ファイルを選ぶかドロップしてください。');
  }

  bindZoomKeys(): void {
    if (this.api.kind !== 'desktop') return;
    const step = (dir: number) => {
      const i = ZOOM_STEPS.indexOf(this.settings.zoom);
      this.setZoom(ZOOM_STEPS[clamp((i < 0 ? ZOOM_STEPS.indexOf(1) : i) + dir, 0, ZOOM_STEPS.length - 1)]);
    };
    window.addEventListener('keydown', (e) => {
      if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
      if (e.key === '+' || e.key === '=' || e.key === ';' || e.code === 'NumpadAdd') {
        e.preventDefault();
        step(1);
      } else if (e.key === '-' || e.code === 'NumpadSubtract') {
        e.preventDefault();
        step(-1);
      } else if (e.key === '0' || e.code === 'Numpad0') {
        e.preventDefault();
        this.setZoom(1);
      }
    });
    let acc = 0;
    window.addEventListener(
      'wheel',
      (e) => {
        if (!e.ctrlKey) return;
        e.preventDefault();
        acc += e.deltaY;
        if (Math.abs(acc) >= 50) {
          step(acc < 0 ? 1 : -1);
          acc = 0;
        }
      },
      { passive: false },
    );
  }
}

/** Keeps the page from behaving like a web page: no browser menu, reload or print. */
function bindShellBehaviour(api: Backend): void {
  document.addEventListener('contextmenu', (e) => {
    if (e.defaultPrevented) return;
    const t = e.target as HTMLElement;
    if (t.closest('input, textarea')) return; // the editing menu (cut, copy, paste)
    e.preventDefault();
    const text = window.getSelection()?.toString() ?? '';
    if (text) {
      openMenu(e.clientX, e.clientY, [
        {
          label: 'コピー',
          shortcut: 'Ctrl+C',
          action: () => {
            navigator.clipboard?.writeText(text).catch(() => document.execCommand('copy'));
          },
        },
      ]);
    }
  });
  if (api.kind !== 'desktop') return;
  window.addEventListener(
    'keydown',
    (e) => {
      const k = e.key;
      const ctrl = e.ctrlKey || e.metaKey;
      const browserKey =
        k === 'F1' ||
        k === 'F5' ||
        k === 'F7' ||
        k === 'F12' ||
        (ctrl && ['r', 'R', 'p', 'P', 's', 'S', 'u', 'U', 'j', 'J', 'h', 'H', 'n', 'N', 'o', 'O', 'w', 'W'].includes(k)) ||
        (ctrl && e.shiftKey && ['i', 'I'].includes(k)) ||
        (e.altKey && (k === 'ArrowLeft' || k === 'ArrowRight' || k === 'Home'));
      if (browserKey) e.preventDefault();
    },
    true,
  );
}

/** Where the first sentence of `text` ends: after its first 。 outside
 *  brackets (an OS message quoted in （） has its own). 0 if none. */
export function sentenceEnd(text: string): number {
  let depth = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === '（' || c === '(') depth++;
    else if ((c === '）' || c === ')') && depth > 0) depth--;
    else if (c === '。' && depth === 0) return i + 1;
  }
  return 0;
}

/**
 * A bug in the screen's own code: logged next to the service's crash log and
 * told once, instead of leaving a half-working screen without a word.
 */
function catchScreenErrors(api: Backend): void {
  let told = false;
  const report = (text: string) => {
    api
      .call<{ crash_log: string | null }>('log_error', { text })
      .catch(() => ({ crash_log: null }))
      .then((r) => {
        if (told) return;
        told = true;
        const where = r.crash_log ? `記録は ${r.crash_log} にあります。` : '';
        toast(`画面の内部エラーが起きました。表示がおかしいときは、アプリを開き直してください。${where}`, 'error');
      });
  };
  window.addEventListener('error', (e) => report(e.error instanceof Error ? where(e.error) : `${e.filename}:${e.lineno}:${e.colno}`));
  window.addEventListener('unhandledrejection', (e) => report(e.reason instanceof Error ? where(e.reason) : typeof e.reason));
}

/** The kind of an error and the code it happened in, without its message:
 *  a message can quote values from the files (a key, a searched text). */
function where(e: Error): string {
  return [e.name, ...(e.stack ?? '').split('\n').filter((l) => /^\s+at /.test(l))].join('\n');
}

async function boot(): Promise<void> {
  window.__cdr = { metrics, ready: false, state: null };
  const api = createBackend();
  catchScreenErrors(api);
  const settings = mergeSettings(await api.loadSettings());
  const app = new App(api, settings);
  if (settings.zoom !== 1) await api.setZoom(settings.zoom);
  app.bindZoomKeys();
  window.addEventListener('pagehide', () => app.flushSettings());
  bindShellBehaviour(api);
  api.onFileDrop((paths, x, y) => app.onDrop(paths, x, y));

  // Test hooks used by tools/ui_bench.py and the UI tests.
  const cmp = () => window.__cdr.state as CompareScreen;
  window.__cdr.pane = (side: Side) => cmp().panes[side];
  window.__cdr.jumpTo = (row: number, side: Side = 'left') => cmp().panes[side].setTop(row);
  window.__cdr.measureJump = (row: number, side: Side = 'left') =>
    new Promise<number>((resolve) => {
      const p = cmp().panes[side];
      const t0 = performance.now();
      p.waiter = () => resolve(performance.now() - t0);
      p.setTop(row);
    });
  window.__cdr.visible = (side: Side = 'left') =>
    [...cmp().panes[side].rowsEl.querySelectorAll<HTMLElement>('.row')].map((r) => ({
      row: Number(r.dataset.row),
      unit: r.dataset.unit === undefined ? null : Number(r.dataset.unit),
      pending: r.classList.contains('pending'),
      text: [...r.children].map((c) => c.textContent),
    }));

  let st: Status | null = null;
  try {
    st = await api.call<Status>('status');
  } catch (e) {
    toast((e as Error).message, 'error');
  }
  if (!st || st.phase.state === 'idle') {
    app.showSetup(null);
    const start = await api.startupArgs();
    if (start.files.length) app.openFiles(start.files, start.keys);
  } else app.progress(null);
}

boot();
