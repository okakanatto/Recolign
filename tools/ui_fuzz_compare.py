#!/usr/bin/env python3
"""Random-operation test of the comparison screen (app/src/compare.ts,
app/src/grid.ts), run in Chromium against the development harness.

    python3 tools/ui_fuzz_compare.py --harness target/release/cdr-harness [--sequences N] [--seed S] [--steps 20-40] [--no-shrink]
    python3 tools/ui_fuzz_compare.py --harness target/release/cdr-harness --replay failure.json [more.json | DIR ...]

Each sequence compares generated files (empty cells, records and a column
on one side only, a value too long for a cell) and applies random steps:
keys (arrows and Ctrl+arrows, with and without Shift, PageUp/PageDown,
Home/End, Ctrl+Home/End, Enter, Shift+Space, Ctrl+Space, Ctrl+Shift+Space,
Ctrl+A, J/K, F6, Ctrl+PageUp/PageDown, Ctrl+C), clicks (a cell, a line
number, a column name, with and without Shift; the table's corner), the
kind buttons, 全行／差分のみ, キー整列／元ファイル順, 左右／上下, sorting
and hiding from a column name's menu, showing the hidden columns, the
difference buttons and searching. After each step:

  - no script error on the page and no error notice;
  - each table's selection lies inside the table;
  - after a key that moves the cursor, the cursor's row is on screen;
  - Ctrl+arrow (and Ctrl+Shift+arrow, which keeps the anchor) lands where a
    spreadsheet would, worked out here from the values the engine returns;
  - a copy has as many rows and columns as the selection, with the column
    names when whole columns or the whole table are selected;
  - the selection size shown above the table agrees with the selection;
  - the detail panel shows the record selected;
  - the counts in the toolbar stay as they were.

A failure is shrunk to fewer steps and saved as JSON in --out; --replay runs
such files again. The exit status is 1 if any check failed.
"""

from __future__ import annotations

import argparse
import json
import random
import sys
import tempfile
import time
from pathlib import Path

from playwright.sync_api import Page, sync_playwright

sys.path.insert(0, str(Path(__file__).resolve().parent))
from ui_test import API, CHROMIUM, Harness  # noqa: E402

SIDES = ('left', 'right')
PANE = {'left': '#paneL', 'right': '#paneR'}

IDLE = '''() => new Promise((res) => {
  let calm = 0;
  const chk = () => {
    const s = window.__cdr && window.__cdr.state;
    const ok = s && s.panes && !s.busy && !s.edge && !(s.pressed && s.pressed.length) && ['left', 'right'].every((k) => {
      const p = s.panes[k];
      return p.model && p.complete && !p.model.inflight;
    });
    calm = ok ? calm + 1 : 0;
    if (calm >= 3) res(true); else requestAnimationFrame(chk);
  };
  requestAnimationFrame(chk);
})'''

STATE = '''() => {
  const s = __cdr.state;
  const P = (k) => {
    const p = s.panes[k];
    if (!p.model) return null;
    const sel = p.sel;
    return { total: p.model.total, cols: p.colCount, top: p.top, vis: p.visibleRows, view: p.model.id,
             order: [...p.order], nslots: p.model.slots.length, active: sel.active,
             anchor: { ...sel.anchor }, focus: { ...sel.focus }, whole: sel.whole, range: sel.active ? sel.range : null,
             info: document.querySelector(`.pwrap[data-side="${k}"] .csel`)?.textContent ?? '' };
  };
  return { active: s.active, order: s.order, counts: s.summary.counts, left: P('left'), right: P('right'),
           sel: s.sel ? { unit: s.sel.unit, rows: { ...s.sel.rows } } : null, detail: s.detail.unit,
           focusIn: document.activeElement?.closest?.('#paneL, #paneR')?.id ?? null };
}'''

MOVES = ['ArrowDown', 'ArrowUp', 'ArrowLeft', 'ArrowRight', 'PageDown', 'PageUp', 'Home', 'End', 'Control+Home', 'Control+End',
         'Enter', 'Shift+Enter']
EDGES = ['Control+ArrowDown', 'Control+ArrowUp', 'Control+ArrowLeft', 'Control+ArrowRight']
OTHER_KEYS = ['Shift+Space', 'Control+Space', 'Control+Shift+Space', 'Control+a', 'j', 'k', 'F6', 'Control+PageDown',
              'Control+PageUp']


# ---- data ----------------------------------------------------------------------

def make_files(d: Path, seed: int) -> tuple[Path, Path]:
    """Two files keyed by id: empty cells, records on one side only, a column
    on one side only, changed values, one long value."""
    rng = random.Random(seed)
    n = rng.randint(30, 160)
    cols = ['c1', 'c2', 'c3', 'c4', 'c5']
    blank = rng.choice([0.1, 0.3, 0.6])

    def val() -> str:
        if rng.random() < blank:
            return ''
        return rng.choice(['a', 'b', 'x', '001', '東京', '大阪', '2026-10-01', 'yes', 'no', ' ', '-5'])

    left_rows, right_rows = [], []
    for i in range(n):
        key = f'{i:04d}'
        row = [val() for _ in cols]
        if rng.random() < 0.9:
            left_rows.append([key, *row, val()])
        if rng.random() < 0.9:
            r = [v if rng.random() > 0.2 else val() for v in row]
            right_rows.append([key, *r, val()])
    for i in range(rng.randint(0, 5)):
        right_rows.append([f'9{i:03d}', *(val() for _ in cols), val()])
    if left_rows:
        left_rows[rng.randrange(len(left_rows))][1] = 'L' * 400
    rng.shuffle(right_rows)
    left, right = d / f'left_{seed}.csv', d / f'right_{seed}.csv'
    left.write_text('\n'.join(','.join(r) for r in [['id', *cols, 'lx'], *left_rows]) + '\n', encoding='utf-8')
    right.write_text('\n'.join(','.join(r) for r in [['id', *cols, 'rx'], *right_rows]) + '\n', encoding='utf-8')
    return left, right


# ---- steps ---------------------------------------------------------------------

def gen_step(rng: random.Random) -> dict:
    r = rng.random()
    side = rng.choice(SIDES)
    f = lambda: round(rng.random(), 3)  # noqa: E731 - a position, as a fraction of what is on screen
    if r < 0.22:
        return {'op': 'key', 'key': rng.choice(MOVES + ['Shift+' + k for k in ('ArrowDown', 'ArrowUp', 'ArrowLeft', 'ArrowRight', 'PageDown')])}
    if r < 0.42:
        return {'op': 'key', 'key': rng.choice(EDGES + ['Control+Shift+' + k.split('+')[1] for k in EDGES])}
    if r < 0.52:
        return {'op': 'key', 'key': rng.choice(OTHER_KEYS)}
    if r < 0.58:
        return {'op': 'copy'}
    if r < 0.70:
        return {'op': 'cell', 'side': side, 'r': f(), 'c': f(), 'shift': rng.random() < 0.3}
    if r < 0.75:
        return {'op': 'num', 'side': side, 'r': f(), 'shift': rng.random() < 0.4}
    if r < 0.81:
        return {'op': 'header', 'side': side, 'c': f(), 'shift': rng.random() < 0.4}
    if r < 0.83:
        return {'op': 'corner', 'side': side}
    if r < 0.86:
        return {'op': 'menu', 'side': side, 'c': f(), 'item': rng.choice(['昇順で並べ替え', '降順で並べ替え', '並べ替えを解除', 'この列を隠す', 'この列を選択'])}
    if r < 0.87:
        return {'op': 'showall'}
    if r < 0.90:
        return {'op': 'kind', 'kind': rng.choice(['changed', 'deleted', 'added', 'same'])}
    if r < 0.92:
        return {'op': 'scope', 'v': rng.choice(['all', 'diff'])}
    if r < 0.94:
        return {'op': 'order', 'v': rng.choice(['aligned', 'source'])}
    if r < 0.95:
        return {'op': 'layout', 'v': rng.choice(['side', 'stack'])}
    if r < 0.97:
        return {'op': 'diffbtn', 'next': rng.random() < 0.6}
    return {'op': 'search', 'text': rng.choice(['a', '東京', '00', 'zz']), 'narrow': rng.random() < 0.3}


def pick(items: list, frac: float):
    return items[min(len(items) - 1, int(frac * len(items)))] if items else None


class Session:
    def __init__(self, browser, harness: Path, data: Path):
        self.browser, self.harness, self.data = browser, harness, data
        self.errors: list[str] = []
        # How many times each check ran.
        self.checked = {'edge': 0, 'copy': 0, 'cursor on screen': 0, 'detail': 0}

    def run(self, data_seed: int, steps: list[dict], stop_at_first: bool = True) -> tuple[list[str], int]:
        """Runs the steps; returns the failures and the index of the step that failed."""
        left, right = make_files(self.data, data_seed)
        h = Harness(self.harness, ['--left', str(left), '--right', str(right), '--key', 'id'])
        ctx = self.browser.new_context(viewport={'width': 1366, 'height': 768}, bypass_csp=True)
        page = ctx.new_page()
        self.errors = []
        page.on('pageerror', lambda e: self.errors.append(str(e)))
        page.on('dialog', lambda d: d.dismiss())
        page.add_init_script('window.__copies = 0')
        try:
            page.goto(h.url)
            page.wait_for_function('window.__cdr && window.__cdr.ready', timeout=30000)
            page.evaluate(IDLE)
            counts = page.evaluate(STATE)['counts']
            for i, st in enumerate(steps):
                fails = self.step(page, st, counts)
                if fails:
                    return fails, i
            return [], -1
        except Exception as e:  # noqa: BLE001 - a step that cannot run is a failure too
            return [f'step raised: {type(e).__name__}: {str(e).splitlines()[0][:200]}'], len(steps)
        finally:
            ctx.close()
            h.close()

    def values(self, page: Page, p: dict, side: str) -> list[list[bool] | None]:
        """For each row of the pane, whether each slot's cell on `side` holds a value (None: no record)."""
        out: list = []
        for start in range(0, p['total'], 200):
            r = page.evaluate(API, ['rows', {'view': p['view'], 'start': start, 'count': min(200, p['total'] - start), 'col_start': 0,
                                             'col_count': p['nslots'], 'pinned': [], 'max_chars': 4}])
            for row in r['rows']:
                rec = row.get(side)
                out.append(None if rec is None else [c.get('v') not in (None, '') for c in rec['cells']])
        return out

    def step(self, page: Page, st: dict, counts: dict) -> list[str]:
        before = page.evaluate(STATE)
        page.evaluate("document.querySelector('.toast')?.setAttribute('hidden', '')")
        page.evaluate('window.__lastCopy = null')
        op = st['op']
        side = before['active']
        if op in ('key', 'copy'):
            if before['focusIn'] is None:
                page.locator(f"{PANE[side]} .scroller").focus()
            key = 'Control+c' if op == 'copy' else st['key']
            page.keyboard.press(key)
        elif op in ('cell', 'num'):
            side = st['side']
            cells = page.evaluate(f'''[...document.querySelectorAll('{PANE[side]} .row:not(.pending)')].map(r => [Number(r.dataset.row),
                [...r.querySelectorAll('.c[data-col]')].map(c => Number(c.dataset.col))])''')
            cells = [c for c in cells if c[1]]
            row = pick(cells, st['r'])
            if row is None:
                return []
            target = f'.c.num' if op == 'num' else f'.c[data-col="{pick(row[1], st["c"])}"]'
            page.locator(f'{PANE[side]} .row[data-row="{row[0]}"] {target}').click(modifiers=['Shift'] if st['shift'] else [])
        elif op in ('header', 'menu'):
            side = st['side']
            heads = page.locator(f'{PANE[side]} .hrow .c[data-slot]')
            n = heads.count()
            if not n:
                return []
            h = heads.nth(min(n - 1, int(st['c'] * n)))
            if op == 'header':
                h.click(modifiers=['Shift'] if st['shift'] else [])
            else:
                h.click(button='right')
                item = page.locator('.menu .mi', has_text=st['item'])
                if item.count() and item.first.is_enabled():
                    item.first.click()
                else:
                    page.keyboard.press('Escape')
        elif op == 'corner':
            page.locator(f"{PANE[st['side']]} .hrow .c.num.corner").click()
        elif op == 'showall':
            b = page.locator('.toolbar button.note')
            if b.count() and b.first.is_visible():
                b.first.click()
        elif op == 'kind':
            page.locator(f".kindbtn[data-kind=\"{st['kind']}\"]").click()
        elif op in ('scope', 'order', 'layout'):
            page.locator(f"button[data-{op}=\"{st['v']}\"]:visible").first.click()
        elif op == 'diffbtn':
            page.locator('.toolbar button', has_text='次の差分' if st['next'] else '前の差分').click()
        elif op == 'search':
            page.fill('.toolbar input.search', st['text'])
            page.keyboard.press('Enter')
            if st['narrow']:
                page.locator('.toolbar button', has_text='絞り込む').click()
            page.evaluate(IDLE)
            page.locator('.toolbar input.search').press('Escape')
        page.evaluate(IDLE)
        after = page.evaluate(STATE)
        return self.check(page, st, before, after, counts)

    def check(self, page: Page, st: dict, b: dict, a: dict, counts: dict) -> list[str]:
        fails: list[str] = []
        if self.errors:
            fails.append('script error: ' + self.errors[0][:200])
        err = page.evaluate("(() => { const t = document.querySelector('.toast.error'); return t && !t.hidden ? t.textContent : null })()")
        if err:
            fails.append(f'error notice: {err[:160]}')
        if a['counts'] != counts:
            fails.append(f"the counts changed: {a['counts']}")
        for k in SIDES:
            p = a[k]
            if p and not p['active'] and p['info']:
                fails.append(f"{k}: nothing is selected but the selection size reads {p['info']!r}")
            if p and p['active'] and p['total']:
                g = p['range']
                if not (0 <= g['r0'] <= g['r1'] < p['total'] and 0 <= g['c0'] <= g['c1'] < max(1, p['cols'])):
                    fails.append(f"{k}: the selection {g} is outside the table ({p['total']} rows, {p['cols']} columns)")
                f = p['focus']
                if not (0 <= f['row'] < p['total'] and 0 <= f['col'] < max(1, p['cols'])):
                    fails.append(f'{k}: the cursor {f} is outside the table')
                rows, cols = g['r1'] - g['r0'] + 1, g['c1'] - g['c0'] + 1
                want = f'選択 {rows:,}行×{cols:,}列' if rows > 1 or cols > 1 else ''
                if p['info'] != want:
                    fails.append(f"{k}: the selection size reads {p['info']!r}, not {want!r}")
        side = b['active']
        p0, p1 = b[side], a[side]
        key = st.get('key', '')
        moved = st['op'] == 'key' and (key in MOVES or key in EDGES or key.startswith('Shift+Arrow') or key.startswith('Control+Shift+Arrow')
                                       or key == 'Shift+PageDown')
        if moved and p1 and p1['total'] and a['active'] == side:
            self.checked['cursor on screen'] += 1
            r = p1['focus']['row']
            if not (int(p1['top']) <= r <= int(p1['top']) + p1['vis']):
                fails.append(f"{side}: after {key} the cursor's row {r} is off screen (rows {int(p1['top'])}+{p1['vis']})")
        if st['op'] == 'key' and 'Arrow' in key and key.startswith('Control+') and p0 and p0['total'] and p0['cols'] and p1:
            fails += self.check_edge(page, key, side, p0, p1, (b['sel'] or {}).get('rows', {}).get(side))
        if st['op'] == 'copy' and p0 and p0['active'] and p0['total'] and b['focusIn'] in (None, PANE[side][1:]):
            fails += self.check_copy(page, side, p0)
        if a['sel'] and a['sel']['unit'] >= 0:
            self.checked['detail'] += 1
        if a['sel'] and a['sel']['unit'] >= 0 and a['detail'] != a['sel']['unit']:
            try:
                page.wait_for_function('__cdr.state.sel && __cdr.state.detail.unit === __cdr.state.sel.unit', timeout=3000)
            except Exception:  # noqa: BLE001
                fails.append(f"the detail panel shows record {page.evaluate('__cdr.state.detail.unit')}, not the selected {a['sel']['unit']}")
        return fails

    def check_edge(self, page: Page, key: str, side: str, p0: dict, p1: dict, record_row: int | None) -> list[str]:
        if p0['view'] != p1['view']:
            return []
        self.checked['edge'] += 1
        # Without a cursor in the table, from the row of the record selected.
        start = p0['focus'] if p0['active'] else {'row': int(p0['top']) if record_row is None else record_row, 'col': 0}
        start = {'row': min(start['row'], p0['total'] - 1), 'col': min(start['col'], p0['cols'] - 1)}
        vals = self.values(page, p0, side)
        filled = lambda row, col: vals[row] is not None and vals[row][p0['order'][col]]  # noqa: E731
        d = key.split('+')[-1]
        if d in ('ArrowDown', 'ArrowUp'):
            line = [filled(r, start['col']) for r in range(p0['total'])]
            want = {'row': spreadsheet_edge(line, start['row'], d == 'ArrowDown'), 'col': start['col']}
        else:
            line = [filled(start['row'], c) for c in range(p0['cols'])]
            want = {'row': start['row'], 'col': spreadsheet_edge(line, start['col'], d == 'ArrowRight')}
        out = []
        if p1['focus'] != want:
            out.append(f'{side}: {key} from {start} went to {p1["focus"]}, a spreadsheet goes to {want}')
        shift = 'Shift' in key
        if shift and p0['active'] and p1['anchor'] != p0['anchor']:
            out.append(f'{side}: {key} moved the anchor from {p0["anchor"]} to {p1["anchor"]}')
        if not shift and p1['range'] != {'r0': want['row'], 'r1': want['row'], 'c0': want['col'], 'c1': want['col']}:
            out.append(f'{side}: {key} left the selection {p1["range"]}')
        return out

    def check_copy(self, page: Page, side: str, p: dict) -> list[str]:
        self.checked['copy'] += 1
        try:
            page.wait_for_function('window.__lastCopy !== null', timeout=5000)
        except Exception:  # noqa: BLE001
            return ['Ctrl+C copied nothing']
        text = page.evaluate('window.__lastCopy')
        g = p['range']
        lines = text.split('\r\n')
        if lines and lines[-1] == '':
            lines.pop()
        header = p['whole'] in ('cols', 'all')
        rows, cols = g['r1'] - g['r0'] + 1, g['c1'] - g['c0'] + 1
        out = []
        if len(lines) != rows + header:
            out.append(f'{side}: the copy has {len(lines)} lines for {rows} rows' + (' and the column names' if header else ''))
        bad = [i for i, ln in enumerate(lines) if ln.count('\t') != cols - 1]
        if bad:
            out.append(f'{side}: copied line {bad[0] + 1} has {lines[bad[0]].count(chr(9)) + 1} values for {cols} columns')
        return out


def spreadsheet_edge(filled: list[bool], at: int, forward: bool) -> int:
    """Where a spreadsheet's Ctrl+arrow goes along `filled` from `at`."""
    step = 1 if forward else -1
    end = len(filled) - 1 if forward else 0
    if at == end:
        return at
    if filled[at] and filled[at + step]:
        i = at + step
        while i != end and filled[i + step]:
            i += step
        return i
    i = at + step
    while i != end and not filled[i]:
        i += step
    return i


def shrink(session: Session, data_seed: int, steps: list[dict], budget_s: float = 240) -> list[dict]:
    t0 = time.time()
    fails, at = session.run(data_seed, steps)
    steps = steps[: at + 1]
    i = 0
    while i < len(steps) and time.time() - t0 < budget_s:
        trial = steps[:i] + steps[i + 1:]
        f, k = session.run(data_seed, trial)
        if f:
            steps = trial[: k + 1]
        else:
            i += 1
    return steps


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--harness', type=Path, required=True)
    ap.add_argument('--sequences', type=int, default=20)
    ap.add_argument('--seed', type=int, default=1)
    ap.add_argument('--steps', default='20-40')
    ap.add_argument('--out', type=Path, default=Path('ui_fuzz_compare_failures'))
    ap.add_argument('--replay', nargs='*', type=Path)
    ap.add_argument('--no-shrink', action='store_true', help='save failures as they are (faster, for CI)')
    a = ap.parse_args()
    lo, hi = (int(x) for x in a.steps.split('-'))
    failed = 0
    with sync_playwright() as pw, tempfile.TemporaryDirectory() as tmp:
        browser = pw.chromium.launch(executable_path=CHROMIUM if Path(CHROMIUM).exists() else None, args=['--no-sandbox'])
        session = Session(browser, a.harness, Path(tmp))
        if a.replay is not None:
            files = [f for p in a.replay for f in (sorted(p.glob('*.json')) if p.is_dir() else [p])]
            for f in files:
                case = json.loads(f.read_text(encoding='utf-8'))
                fails, at = session.run(case['data_seed'], case['steps'])
                print(f'{f.name}: ' + (f'FAIL at step {at + 1}: {fails}' if fails else f"{len(case['steps'])} steps, clean"))
                failed += bool(fails)
        else:
            total = 0
            for n in range(a.sequences):
                seed = a.seed + n
                rng = random.Random(seed)
                steps = [gen_step(rng) for _ in range(rng.randint(lo, hi))]
                fails, at = session.run(seed, steps)
                total += len(steps) if not fails else at + 1
                if not fails:
                    print(f'sequence {n + 1} seed {seed}: {len(steps)} steps, clean')
                    continue
                failed += 1
                small = steps[: at + 1] if a.no_shrink else shrink(session, seed, steps)
                f2, _ = session.run(seed, small)
                a.out.mkdir(parents=True, exist_ok=True)
                path = a.out / f'seed{seed}.json'
                path.write_text(json.dumps({'data_seed': seed, 'steps': small, 'failure': f2 or fails}, ensure_ascii=False, indent=1),
                                encoding='utf-8')
                print(f'sequence {n + 1} seed {seed}: FAIL at step {at + 1}: {fails}\n  shrunk to {len(small)} steps -> {path}')
            print(f'\n{a.sequences} sequences from seed {a.seed}: {a.sequences - failed} clean, {total} steps; checks run: {session.checked}')
        browser.close()
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
