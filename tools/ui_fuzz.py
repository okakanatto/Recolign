#!/usr/bin/env python3
"""Random-operation (state-machine) test of the setup screen (app/src/setup.ts),
run in Chromium against the development harness.

    python3 tools/ui_fuzz.py --harness target/release/cdr-harness [--sequences N] [--seed S] [--steps 20-40] [--fast]
    python3 tools/ui_fuzz.py --harness target/release/cdr-harness --replay failure.json [more.json | DIR ...]

Each sequence starts from an empty setup screen and applies random steps:
choosing files (CSV, TSV, UTF-16, Shift_JIS, EUC-JP, fixed width, a missing
file), changing the encoding and the layout, typing a delimiter or widths and
applying them (Enter, Tab, a click elsewhere, Ctrl+Enter), changing the unit,
toggling the options, choosing keys and columns left out, swapping, and
comparing. Files are read asynchronously: some steps go on without waiting for
the reading (with --fast, none waits) and the readings can be slowed down.
Whenever no reading is in flight after a step, and at the end of each
sequence, the screen is checked:

  - no script errors on the page;
  - every card with a file shows its controls or an error, never an empty
    card or "読み込んでいます…";
  - 比較する is enabled exactly when both readings are usable and a key common
    to both files is chosen; when disabled, the hint names a real reason;
  - a comparison sends what the cards show (encoding, layout, widths, unit,
    quotes, header, keys, columns left out), and the setup shown again by
    条件を変更 has the same conditions;
  - the focus is not lost to the page after a keyboard operation on a card;
  - the 区切り select agrees with the controls shown;
  - each step does what it says (the control shows the value chosen, the
    other card is not touched).

A failure is shrunk to a minimal list of steps and saved as JSON in --out;
--replay runs such lists again (a directory: every JSON file in it).
tools/ui_fuzz_cases/ keeps the failures found so far, fixed since, as
regression cases. The exit status is 1 if any check failed.
"""

from __future__ import annotations

import argparse
import json
import math
import random
import re
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable

from playwright.sync_api import Error as PwError, Page, sync_playwright

sys.path.insert(0, str(Path(__file__).resolve().parent))
from ui_test import CHROMIUM, Harness, api  # noqa: E402

SIDES = ('left', 'right')
OTHER = {'left': 'right', 'right': 'left'}
JA = {'left': '左', 'right': '右'}
ENCODINGS = ['utf8', 'windows31j', 'utf16le', 'utf16be', 'euc_jp']
LAYOUTS = ['44', '9', '59', '124', 'other', 'fixed']
UNITS = ['bytes', 'chars']
STANDARD = {44, 9, 59, 124}
SELECTS = {'enc': ('文字コード', ENCODINGS), 'del': ('区切り', LAYOUTS), 'unit': ('幅の単位', UNITS)}
CHECKS = {'ftrim': 'trim', 'quoted': 'quoted', 'header': 'header'}  # op -> data-k of the checkbox
MAX_WIDTH = 1_000_000

# ---- test files ---------------------------------------------------------------

HEAD = ['番号', '氏名', '部署']
BASE = [['001', '山田 太郎', '営業部'], ['002', '佐藤 花子', '人事部'], ['003', '鈴木 一郎', '開発部'],
        ['004', '高橋 ポチ', 'ポスト課'], ['005', '田中 次郎', '総務部']]


def rows(change: dict[str, list[str]] | None = None, drop: tuple[str, ...] = (), add: tuple[list[str], ...] = ()) -> list[list[str]]:
    """BASE with some records changed (by number), dropped or added, under HEAD."""
    out = [change.get(r[0], r) if change else r for r in BASE if r[0] not in drop]
    return [HEAD, *out, *add]


def delimited(table: list[list[str]], sep: str) -> str:
    return ''.join(sep.join(r) + '\r\n' for r in table)


def fixed_width(table: list[list[str]], widths: list[int], encoding: str, unit: str) -> bytes:
    def pad(v: str, w: int) -> str:
        return v + ' ' * (w - (len(v.encode(encoding)) if unit == 'bytes' else len(v)))
    return ''.join(''.join(pad(v, w) for v, w in zip(r, widths)) + '\r\n' for r in table).encode(encoding)


def make_files(d: Path) -> dict[str, Path]:
    """Small files sharing the columns 番号, 氏名, 部署, with some records differing."""
    contents = {
        'csv_utf8.csv': delimited(rows(), ',').encode('utf-8'),
        # Excel's "Unicode text": UTF-16 LE with a BOM, tabs, no quotes.
        'unicode.txt': b'\xff\xfe' + delimited(rows({'002': ['002', '佐藤 花子', '経理部']}, drop=('005',),
                                                     add=(['006', '伊藤 三郎', '営業部'],)), '\t').encode('utf-16-le'),
        # Tab-separated without quotes, a `"` inside a value and a key twice.
        'quote.tsv': delimited(rows({'004': ['004', '5" 画面', 'ポスト課']}, add=(['003', '鈴木 一郎', '開発部'],)), '\t').encode('utf-8'),
        # Shift_JIS with `|`: ポ is 0x83 0x7C, its second byte is `|`.
        'pipe_sjis.txt': delimited(rows({'001': ['001', '山田 ポチ', '営業部']}), '|').encode('cp932'),
        'colon.txt': delimited(rows({'003': ['003', '鈴木 一郎', '総務部']}), ':').encode('utf-8'),
        'fixed_sjis.dat': fixed_width(rows({'002': ['002', '佐藤 花子', '経理部']}), [4, 10, 8], 'cp932', 'bytes'),
        'fixed_utf8.dat': fixed_width(rows(drop=('004',)), [4, 6, 5], 'utf-8', 'chars'),
        'euc.csv': delimited(rows({'005': ['005', '田中 次郎', 'ポスト課']}), ',').encode('euc_jp'),
        'extra.csv': delimited([r + ['役職' if i == 0 else '主任'] for i, r in enumerate(rows())], ';').encode('utf-8'),
    }
    files = {}
    for name, data in contents.items():
        (d / name).write_bytes(data)
        files[name] = d / name
    files['missing.csv'] = d / 'missing.csv'
    return files


FILE_WEIGHTS = {'csv_utf8.csv': 4, 'unicode.txt': 3, 'quote.tsv': 2, 'pipe_sjis.txt': 3, 'colon.txt': 2,
                'fixed_sjis.dat': 4, 'fixed_utf8.dat': 3, 'euc.csv': 2, 'extra.csv': 2, 'missing.csv': 1}

# ---- values typed ------------------------------------------------------------

ONE_CHARS = [':', ';', '|', ',', ' ', 'x', '#', '"', '：', '｜', '，', '；', '＃', 'あ', '・', '\u3000', '']
WIDTHS_VALID = ['4, 10, 8', '4,10,8', '4 10 8', '４，１０，８', '4、10、8', '4, 6, 5', '4,6,5', '3, 7', '1', '2, 2, 2, 2',
                '1000000', '4, 10, 4, 4', '4,,10,8']
WIDTHS_INVALID = ['0', 'a', '4, x', '-1', '4.5', '99999999999', '１０ｘ', '4; 10']
WIDTHS_HUGE = ', '.join(['1000000'] * 4300)  # each width allowed, the sum is not
THEN = ['Enter', 'Tab', 'click', 'CtrlEnter', 'none']
DELAYS = [0, 0, 0, 30, 100, 250]  # ms a reading is held back in steps that do not wait


def half_width(text: str) -> str:
    """As setup.ts halfWidth: full-width letters, digits and signs as half-width."""
    return ''.join(chr(ord(c) - 0xFEE0) if 0xFF01 <= ord(c) <= 0xFF5E else ' ' if c == '\u3000' else c for c in text)


def parse_widths(text: str | None) -> list[int] | None:
    """As setup.ts parseWidths."""
    parts = [x for x in re.split(r'[\s,、]+', half_width(text or '')) if x]
    if not parts or not all(re.fullmatch(r'[0-9]+', x) and 0 < int(x) <= MAX_WIDTH for x in parts):
        return None
    return [int(x) for x in parts]


def usable_char(text: str) -> bool:
    """Whether the one-character field holds a delimiter the screen accepts."""
    c = half_width(text)[:1]
    return bool(c) and ord(c) < 0x80 and c != '"'


def widths_shown(text: str | None) -> object:
    """A widths field's text for comparing: the widths when it reads as widths."""
    return parse_widths(text) or text


def describe(st: dict) -> str:
    """A step in words, for the report."""
    op, side = st['op'], JA.get(st.get('side', ''), '')
    how = st.get('how')
    s = {
        'pick': lambda: f'{side}: ファイルを選ぶ {st.get("file")}',
        'enc': lambda: f'{side}: 文字コード ' + (f'{st["key"]} key' if how == 'key' else f'= {st["value"]}'),
        'del': lambda: f'{side}: 区切り ' + (f'{st["key"]} key' if how == 'key' else f'= {st["value"]}'),
        'unit': lambda: f'{side}: 幅の単位 ' + (f'{st["key"]} key' if how == 'key' else f'= {st["value"]}'),
        'one': lambda: f'{side}: 区切り文字 ← {st["text"]!r}',
        'widths': lambda: f'{side}: 列の幅 ← {st["text"][:30]!r}{"…" if len(st["text"]) > 30 else ""} then {st["then"]}',
        'ftrim': lambda: f'{side}: 前後の空白を除く toggled ({how})',
        'quoted': lambda: f'{side}: 引用符 toggled ({how})',
        'header': lambda: f'{side}: 1行目は列名 toggled ({how})',
        'key': lambda: f'key chip {st["name"]!r} clicked',
        'exclude': lambda: f'比較しない列 chip {st["name"]!r} clicked',
        'trimrule': lambda: '値の前後の空白を無視 toggled',
        'swap': lambda: '左右を入れ替える',
        'ctrlenter': lambda: 'Ctrl+Enter',
        'go': lambda: '比較する clicked',
    }[op]()
    extra = [] if st.get('wait', 'idle') == 'idle' else ['no wait']
    if st.get('delay'):
        extra.append(f'readings +{st["delay"]} ms')
    return s + (f'  [{", ".join(extra)}]' if extra else '')


# ---- page instrumentation -----------------------------------------------------

# Installed before the app's own script: counts the readings in flight (and can
# slow them down), records what a comparison sends together with the screen at
# that moment, records notices, and describes the setup screen.
INSTRUMENT = r'''(() => {
  try { localStorage.removeItem('cdr-settings'); } catch {}
  const F = (window.__fuzz = { pending: 0, delay: 0, compares: [], toasts: [], bad: [] });
  // A text field typed into and not applied yet (its change event not fired).
  document.addEventListener('input', (e) => { if (e.target instanceof HTMLInputElement) e.target.__dirty = true; }, true);
  document.addEventListener('change', (e) => { if (e.target instanceof HTMLInputElement) e.target.__dirty = false; }, true);
  document.addEventListener('DOMContentLoaded', () => {
    new MutationObserver(() => {
      const t = document.querySelector('.toast');
      if (!t || t.hidden) return;
      const s = (t.classList.contains('error') ? 'error: ' : '') + t.textContent;
      if (F.toasts[F.toasts.length - 1] !== s) F.toasts.push(s);
    }).observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ['hidden'] });
  });

  const card = (side) => {
    const c = document.querySelector(`main.setup .card[data-side="${side}"]`);
    if (!c) return null;
    const kids = [...c.children].flatMap((e) => (e.matches('.below') ? [...e.children] : [e]));
    const q = (s) => c.querySelector(s);
    const val = (s) => (q(s) ? q(s).value : null);
    const chk = (k) => (q(`input[data-k="${k}"]`) ? q(`input[data-k="${k}"]`).checked : null);
    const muted = kids.filter((e) => e.matches('div.muted')).map((e) => e.textContent);
    const err = kids.find((e) => e.matches('div.error'));
    const w = q('input[aria-label="列の幅"]');
    return {
      file: !q('.dropzone'),
      path: q('.fname') ? q('.fname').title : null,
      loading: muted.some((t) => t.startsWith('読み込んでいます')),
      draft: muted.some((t) => t.startsWith('列の幅を入力し')) ? 'fixed' : muted.some((t) => t.startsWith('区切り文字を入力すると')) ? 'other' : null,
      error: err ? err.textContent : null,
      invalid: q('[aria-invalid="true"]') ? q('[aria-invalid="true"]').dataset.k : null,
      controls: !!q('select[aria-label="文字コード"]'),
      enc: val('select[aria-label="文字コード"]'),
      del: val('select[aria-label="区切り"]'),
      one: val('input[aria-label="区切り文字"]'),
      widths: w ? w.value : null,
      // Typed and not used yet; a redraw carries such text into a new box (its data-v is the text last used).
      widthsDirty: !!w && (w.dataset.v === undefined ? !!w.__dirty : w.value !== w.dataset.v),
      unit: val('select[aria-label="幅の単位"]'),
      ftrim: chk('trim'),
      quoted: chk('quoted'),
      header: chk('header'),
      headers: q('.pv') ? [...c.querySelectorAll('.pv th')].map((e) => e.textContent) : null,
      children: kids.length,
    };
  };
  const chips = (label) =>
    [...document.querySelectorAll(`main.setup [aria-label="${label}"] .chip`)].map((b) => ({
      name: b.dataset.name,
      on: b.getAttribute('aria-pressed') === 'true',
      ord: b.querySelector('.ord') ? Number(b.querySelector('.ord').textContent) : 0,
    }));
  const keyBlock = () => document.querySelector('main.setup [aria-label="キーにする列"]')?.parentElement;
  F.snap = () => {
    const m = document.querySelector('main');
    if (!m || !m.classList.contains('setup')) return { screen: m ? m.className : null };
    const keys = chips('キーにする列');
    const ex = chips('比較しない列');
    const a = document.activeElement;
    const ac = a && a.closest ? a.closest('.card') : null;
    return {
      screen: 'setup',
      left: card('left'),
      right: card('right'),
      keyChips: keys.map((c) => c.name),
      keys: keys.filter((c) => c.on).sort((x, y) => x.ord - y.ord).map((c) => c.name),
      // A key guessed from the files (the heading says 推定), and a problem shown before comparing.
      keyGuessed: (keyBlock()?.querySelector('h2')?.textContent || '').includes('推定'),
      keyProblem: keyBlock()?.querySelector(':scope > .error:not([hidden])')?.textContent || '',
      exChips: ex.map((c) => c.name),
      excluded: ex.filter((c) => c.on).map((c) => c.name),
      trim: document.querySelector('main.setup details.rules input[type=checkbox]').checked,
      go: !document.getElementById('compareBtn').disabled,
      hint: document.querySelector('main.setup .actions .hint').textContent,
      focus: !a || a === document.body ? 'body' : (ac ? ac.dataset.side + ':' : '') + (a.dataset.k || a.getAttribute('aria-label') || a.textContent.slice(0, 20) || a.tagName),
      focusInSetup: !!(a && a !== document.body && m.contains(a)),
    };
  };

  const orig = window.fetch.bind(window);
  window.fetch = async (url, init) => {
    const cmd = String(url).split('api/').pop();
    if (cmd === 'memory_check') F.compares.push({ snap: F.snap(), paths: JSON.parse(init.body) });
    const entry = cmd === 'compare' ? F.compares[F.compares.length - 1] : null;
    if (entry) entry.body = JSON.parse(init.body);
    const reading = cmd === 'preview';
    const delay = F.delay;
    if (reading) F.pending++;
    let r;
    try {
      r = await orig(url, init);
    } catch (e) {
      if (reading) F.pending--;
      throw e;
    }
    if (!r.ok) F.bad.push({ cmd, status: r.status, body: init && init.body });
    if (entry) entry.status = r.status;
    if (reading) {
      if (delay) await new Promise((res) => setTimeout(res, delay));
      // Done once the app has the reply and has drawn what follows from it.
      const json = r.json.bind(r);
      r.json = () => json().finally(() => setTimeout(() => F.pending--, 0));
    }
    return r;
  };

  F.settle = async (wait) => {
    let timeout = false;
    if (wait) {
      const end = performance.now() + 15000;
      for (let calm = 0; calm < 3; ) {
        await new Promise((res) => setTimeout(res, calm ? 0 : 5));
        calm = F.pending === 0 ? calm + 1 : 0;
        if (performance.now() > end) { timeout = true; break; }
      }
      await new Promise((res) => requestAnimationFrame(() => res()));
    }
    // A notice is recorded, then put away so that it covers no button.
    const t = document.querySelector('.toast');
    if (t) t.hidden = true;
    return { timeout, pending: F.pending, snap: F.snap(), toasts: F.toasts.splice(0), bad: F.bad.splice(0), compares: F.compares.length };
  };
})()'''


# ---- checks ---------------------------------------------------------------------

@dataclass
class Failure:
    code: str  # names the check; failures with the same code are taken as the same problem
    severity: str  # high, medium, low
    text: str
    step: int = 0  # 1-based index of the step after which it was seen (0: at the start)


CARD_FIELDS = ('path', 'enc', 'del', 'one', 'widths', 'unit', 'ftrim', 'quoted', 'header', 'draft', 'error', 'invalid', 'headers')
# One sentence per side whose layout is not set: a field to fill in, or text to correct.
UNSET_HINT = re.compile(r'(?:(左|右)のファイルの(列の幅|区切り文字)を(入力して|直して)ください。){1,2}')
UNSET_PART = re.compile(r'(左|右)のファイルの(列の幅|区切り文字)を(入力して|直して)ください。')
UNREAD_HINT = re.compile(r'(左|右|左と右)のファイルを読めません。別のファイルを選んでください。')


def usable(c: dict) -> bool:
    return c['file'] and c['controls'] and not c['loading'] and not c['draft'] and not c['error'] and not c['invalid']


def hint_is_real(hint: str, s: dict) -> bool:
    """Whether the hint under a disabled 比較する names something true of the screen."""
    cards = [s['left'], s['right']]
    if hint == '左右のファイルを選んでください。':
        return not all(c['file'] for c in cards)
    if UNSET_HINT.fullmatch(hint):
        for side, field, verb in UNSET_PART.findall(hint):
            c = s['left' if side == '左' else 'right']
            if verb == '入力して' and c['draft'] != ('fixed' if field == '列の幅' else 'other'):
                return False
            if verb == '直して' and c['invalid'] != ('widths' if field == '列の幅' else 'one'):
                return False
        return True
    m = UNREAD_HINT.fullmatch(hint)
    if m:
        named = [c for side, c in zip('左右', cards) if side in m.group(1)]
        return all(c['file'] and c['error'] and not c['controls'] for c in named)
    m = re.fullmatch(r'(左|右|左と右)のファイルの読み込みの設定を直してください。', hint)
    if m:
        named = [c for side, c in zip('左右', cards) if side in m.group(1)]
        return all(c['controls'] and c['error'] for c in named)
    if hint == 'キーにする列を選んでください。':
        return not s['keys']
    if hint == '列名を直してください。':
        return '列名' in s['keyProblem']
    if hint == 'キーにする列を選び直してください。':
        return 'キーの列' in s['keyProblem']
    return False


def check_screen(s: dict) -> list[Failure]:
    """What must hold on the setup screen whenever no reading is in flight."""
    if s.get('screen') != 'setup':
        return []
    out = []
    for side in SIDES:
        c, ja = s[side], JA[side]
        if not c['file']:
            continue
        if c['loading']:
            out.append(Failure('card:loading', 'high', f'{ja}: the card still shows 読み込んでいます… with no reading in flight'))
        elif not c['controls'] and not c['error'] or c['children'] < 3:
            out.append(Failure('card:empty', 'high', f'{ja}: the card shows neither the reading controls nor an error'))
        if c['controls']:
            want = {'fixed': (False, True), 'other': (True, False)}.get(c['del'], (False, False))
            have = (c['one'] is not None, c['widths'] is not None)
            if have != want or (c['quoted'] is None) != (c['del'] == 'fixed'):
                out.append(Failure('layout:controls', 'medium',
                                   f'{ja}: 区切り shows {c["del"]!r} but the one-character field is {"" if have[0] else "not "}shown, '
                                   f'the widths are {"" if have[1] else "not "}shown, the quote option is {"not " if c["quoted"] is None else ""}shown'))
        if c['draft'] == 'fixed' and not c['widthsDirty'] and parse_widths(c['widths']):
            out.append(Failure('draft:widths-shown', 'medium',
                               f'{ja}: the card says 列の幅を入力すると… and the hint asks for widths, while the field shows '
                               f'{c["widths"]!r} (applied: Enter does nothing)'))
    common = set(s['left']['headers'] or []) & set(s['right']['headers'] or [])
    expected = (usable(s['left']) and usable(s['right']) and bool(s['keys']) and all(k in common for k in s['keys'])
                and not s['keyProblem'])
    if s['go'] != expected:
        why = ', '.join(f'{JA[x]} usable={usable(s[x])}' for x in SIDES) + f', keys={s["keys"]}, common={sorted(common)}'
        out.append(Failure('go:' + ('enabled' if s['go'] else 'disabled'), 'high',
                           f'比較する is {"enabled" if s["go"] else "disabled"} but should not be ({why}; hint {s["hint"]!r})'))
    if not s['go'] and not s['hint']:
        out.append(Failure('hint:empty', 'medium', '比較する is disabled with no hint'))
    elif not s['go'] and not hint_is_real(s['hint'], s):
        out.append(Failure('hint:unreal', 'medium', f'the hint {s["hint"]!r} does not match the screen '
                           f'({", ".join(f"{JA[x]} draft={s[x]["draft"]} error={bool(s[x]["error"])}" for x in SIDES)}, keys={s["keys"]})'))
    elif s['go'] and s['hint']:
        out.append(Failure('hint:while-enabled', 'low', f'比較する is enabled and the hint says {s["hint"]!r}'))
    return out


def diff_request(s: dict, req: dict) -> list[tuple[str, str]]:
    """Where the conditions on the screen `s` differ from the comparison request `req`."""
    out = []
    for side in SIDES:
        c, o, ja = s[side], req[side]['options'], JA[side]
        if c['path'] != req[side]['path']:
            out.append(('path', f'{ja} file: screen {c["path"]!r}, request {req[side]["path"]!r}'))
            continue
        if not usable(c):
            out.append(('unusable', f'{ja} card is not ready (draft={c["draft"]}, error={c["error"]!r}, loading={c["loading"]})'))
        if c['enc'] != o['encoding']:
            out.append(('encoding', f'{ja} 文字コード: screen {c["enc"]!r}, request {o["encoding"]!r}'))
        f = o.get('fixed')
        if f:
            shown = {'del': c['del'], 'widths': parse_widths(c['widths']), 'unit': c['unit'], 'trim': c['ftrim']}
            sent = {'del': 'fixed', 'widths': f['widths'], 'unit': f['unit'], 'trim': f['trim']}
            for k in shown:
                if shown[k] != sent[k]:
                    text = c['widths'] if k == 'widths' else shown[k]
                    out.append((k, f'{ja} {k}: screen {text!r}, request {sent[k]!r}'))
        else:
            d = o['delimiter']
            shown = c['del'] if c['del'] != 'other' else 'other ' + half_width(c['one'] or '')
            sent = str(d) if d in STANDARD else 'other ' + chr(d)
            if shown != sent:
                out.append(('delimiter', f'{ja} 区切り: screen {shown!r}, request {sent!r}'))
            if c['quoted'] != (o.get('quoted') is not False):
                out.append(('quoted', f'{ja} 引用符: screen {c["quoted"]}, request {o.get("quoted")}'))
        if c['header'] != o['has_header']:
            out.append(('header', f'{ja} 1行目は列名: screen {c["header"]}, request {o["has_header"]}'))
    if s['keys'] != req['keys']:
        out.append(('keys', f'keys: screen {s["keys"]}, request {req["keys"]}'))
    if set(s['excluded']) != set(req['excluded']):
        out.append(('excluded', f'比較しない列: screen {s["excluded"]}, request {req["excluded"]}'))
    if s['trim'] != (req['trim'] != 'none'):
        out.append(('trim', f'空白を無視: screen {s["trim"]}, request {req["trim"]}'))
    return out


def chosen(st: dict, before: str | None) -> str | None:
    """The value a select step picks: the one given, or the next one up or down for a key."""
    if st.get('how') != 'key':
        return st['value']
    values = SELECTS[st['op']][1]
    if before not in values:
        return before
    i = values.index(before) + (1 if st['key'] == 'ArrowDown' else -1)
    return values[max(0, min(len(values) - 1, i))]


def is_keyboard(st: dict) -> bool:
    op = st['op']
    return (op == 'one' or (op == 'widths' and st['then'] in ('Enter', 'Tab'))
            or (op in SELECTS and st['how'] == 'key') or (op in CHECKS and st['how'] == 'space'))


Fail = Callable[..., None]


def check_step(st: dict, b: dict, a: dict, toasts: list[str], files: dict[str, Path]) -> list[Failure]:
    """What the step itself should have done; `b` and `a` are the screens
    before and after it, `toasts` the notices it caused."""
    if b.get('screen') != 'setup' or a.get('screen') != 'setup':
        return []
    op = st['op']
    out: list[Failure] = []

    def fail(code: str, text: str, severity: str = 'medium') -> None:
        out.append(Failure(code, severity, f'{text}  (step: {describe(st)})'))

    if is_keyboard(st) and a['focus'] == 'body':
        fail('focus:lost', 'the focus went to <body> after a keyboard operation on a card control', 'low')
    # Keys are used only all together: while a key's column is away, the others wait too.
    dropped = [k for k in b['keys'] if k not in a['keys'] and k in a['keyChips']]
    if op != 'key' and dropped and all(k in a['keyChips'] for k in b['keys']) and not (b['keyGuessed'] or a['keyGuessed']):
        fail('keys:replaced', f'keys {b["keys"]} -> {a["keys"]}: {dropped} still offered but no longer chosen')
    if st.get('side'):
        check_card_step(st, b, a, toasts, files, fail)
    elif op == 'key':
        n = st['name']
        if b['keyGuessed']:
            # A guess is replaced by the column pressed, or taken off by pressing it.
            if a['keys'] != ([] if n in b['keys'] else [n]):
                fail('post:key', f'keys (guessed) {b["keys"]} -> {a["keys"]}')
        elif (n in b['keys']) == (n in a['keys']) or set(b['keys']) ^ set(a['keys']) != {n}:
            fail('post:key', f'keys {b["keys"]} -> {a["keys"]}')
        if n in a['keys'] and n in a['excluded']:
            fail('post:key-excluded', f'{n!r} is both a key and left out of the comparison')
    elif op == 'exclude':
        n = st['name']
        if set(b['excluded']) ^ set(a['excluded']) != {n} or b['keys'] != a['keys']:
            fail('post:exclude', f'excluded {b["excluded"]} -> {a["excluded"]}, keys {b["keys"]} -> {a["keys"]}')
    elif op == 'trimrule':
        if a['trim'] == b['trim']:
            fail('post:trimrule', 'the checkbox did not change')
    elif op == 'swap':
        check_swap(b, a, fail)
    elif op in ('go', 'ctrlenter') and b['go']:
        focused = op == 'go' or b['focusInSetup']
        if focused and not any(b[x]['widthsDirty'] for x in SIDES):
            fail(f'{op}:no-start', '比較する was enabled but no comparison started', 'high')
    return out


def same_shown(f: str, was: object, now: object) -> bool:
    """Whether a card field shows the same thing: widths as numbers, a
    delimiter as half-width; text that was refused may go when the card is
    drawn again."""
    if f == 'widths' and was and not parse_widths(was):
        return True
    if f == 'one' and was:
        return not usable_char(was) or half_width(was) == half_width(now or '')
    if f == 'widths':
        return widths_shown(was) == widths_shown(now)
    return was == now


def check_card_step(st: dict, b: dict, a: dict, toasts: list[str], files: dict[str, Path], fail: Fail) -> None:
    """A step on one card: the control shows what was chosen, the card's other
    controls stay, and the other card is not touched."""
    op, side = st['op'], st['side']
    o = OTHER[side]
    moved = [f for f in CARD_FIELDS if b[o][f] != a[o][f]]
    if moved and not b[o]['widthsDirty']:  # moving the focus out of its widths applies them
        fail('post:other-card', f'the {JA[o]} card changed: ' + ', '.join(f'{f} {b[o][f]!r} -> {a[o][f]!r}' for f in moved), 'high')
    bc, ac = b[side], a[side]
    if not bc['controls'] and op != 'pick':
        return
    code = 'widths-ctrlenter' if op == 'widths' and st['then'] == 'CtrlEnter' else op

    def expect(field: str, want: object, got: object = None) -> None:
        got = ac[field] if got is None else got
        if got != want:
            fail(f'post:{code}', f'{JA[side]} {field} shows {got!r} after the step; expected {want!r}')

    def keep(*fields: str) -> None:
        for f in fields:
            if bc['widthsDirty'] and f in ('widths', 'unit', 'ftrim'):
                continue  # leaving the field applied what was typed
            if bc[f] is not None and ac[f] is not None and not same_shown(f, bc[f], ac[f]):
                fail(f'post:{code}-changed-{f}', f'{JA[side]} {f} changed from {bc[f]!r} to {ac[f]!r} although the step did not touch it')

    if op == 'pick':
        expect('path', str(files[st['file']]))
    elif op in SELECTS:
        if ac[op] is not None:
            expect(op, chosen(st, bc[op]))
        if op == 'enc':
            # Before widths are entered, the unit follows the encoding.
            keep('del', 'one', 'header', 'quoted', 'ftrim', 'widths', *(() if bc['draft'] == 'fixed' else ('unit',)))
        elif op == 'del':
            keep('enc', 'header', 'quoted')
        else:
            keep('enc', 'del', 'header', 'ftrim', 'widths')
    elif op == 'one':
        c = half_width(st['text'])
        if not c:
            expect('draft', 'other')
        elif not usable_char(c):
            if ac['invalid'] != 'one' or '区切り文字に使えません' not in (ac['error'] or ''):
                fail('post:one-invalid-silent', f'the delimiter {st["text"]!r} was refused without a word')
        elif ord(c) in STANDARD:
            expect('del', str(ord(c)))
        else:
            expect('del', 'other')
            expect('one', c, half_width(ac['one'] or ''))
            expect('draft', None)
        keep('enc', 'header', 'quoted')
    elif op == 'widths' and st['then'] != 'none':
        w = parse_widths(st['text'])
        if w:
            expect('draft', None)
            expect('widths', w, parse_widths(ac['widths']))
        elif not st['text'].strip():
            expect('draft', 'fixed')
        elif ac['invalid'] != 'widths' or '列の幅を読み取れません' not in (ac['error'] or ''):
            fail('post:widths-invalid-silent', f'the widths {st["text"]!r} were refused without a word')
        keep('enc', 'header', 'unit', 'ftrim')
    elif op in CHECKS:
        if ac[op] is not None:
            expect(op, not bc[op])
        keep(*(f for f in ('enc', 'del', 'header', 'one', 'unit', 'ftrim', 'quoted', 'widths') if f != op))


def check_swap(b: dict, a: dict, fail: Fail) -> None:
    """左右を入れ替える: each card now shows what the other one showed; keys stay."""
    moved = []
    for x in SIDES:
        for f in CARD_FIELDS:
            if b[x]['widthsDirty'] and f in ('widths', 'draft', 'error', 'invalid', 'headers'):
                continue  # the widths typed were applied on leaving the field
            if not same_shown(f, b[x][f], a[OTHER[x]][f]):
                moved.append(f'{f}: {JA[x]} had {b[x][f]!r}, {JA[OTHER[x]]} now has {a[OTHER[x]][f]!r}')
    if moved:
        fail('post:swap', 'the cards did not trade places: ' + '; '.join(moved))
    if (b['keys'] != a['keys'] and not (b['keyGuessed'] or a['keyGuessed'])) or set(b['excluded']) != set(a['excluded']):
        fail('post:swap-keys', f'keys {b["keys"]} -> {a["keys"]}, excluded {b["excluded"]} -> {a["excluded"]}')


# ---- driving the page ---------------------------------------------------------------


class Session:
    """One page on the harness, reset to an empty setup screen for each sequence."""

    def __init__(self, page: Page, url: str, files: dict[str, Path]):
        self.page, self.url, self.files = page, url, files
        self.answers: list[str] = []
        self.errors: list[str] = []
        self.compares = {'compare': 0, 'failed': 0, 'rejected': 0}
        page.add_init_script(INSTRUMENT)
        page.on('dialog', self.on_dialog)
        page.on('pageerror', lambda e: self.errors.append(f'page error: {e}'))
        page.on('console', self.on_console)

    def on_console(self, m) -> None:
        # A refused request is logged by the browser too; check_bad tells the expected ones apart.
        if m.type == 'error' and 'Failed to load resource' not in m.text:
            self.errors.append(f'console error: {m.text}')

    def on_dialog(self, d) -> None:
        if self.answers:
            d.accept(self.answers.pop(0))
        else:
            d.dismiss()

    def fresh(self) -> None:
        if self.page.url == self.url:
            api(self.page, 'reset')  # a comparison that failed is still the service's state
            self.page.reload()  # the same address: goto would not load the page again
        else:
            self.page.goto(self.url)
        self.page.wait_for_selector('main.setup')
        self.errors.clear()

    def settle(self, wait: bool) -> dict:
        return self.page.evaluate('w => __fuzz.settle(w)', wait)

    def apply(self, st: dict) -> bool:
        """Performs a step; False when what it acts on is not on the screen."""
        p = self.page
        p.evaluate('d => { __fuzz.delay = d; }', st.get('delay', 0))
        self.answers.clear()
        op = st['op']
        card = p.locator(f'main.setup .card[data-side="{st.get("side")}"]')

        def find(sel: str):
            loc = card.locator(sel) if st.get('side') else p.locator(sel)
            return loc.first if loc.count() else None

        if op == 'pick':
            b = find('.chead button')
            if not b:
                return False
            self.answers.append(str(self.files[st['file']]))
            b.click()
        elif op in SELECTS:
            s = find(f'select[aria-label="{SELECTS[op][0]}"]')
            if not s:
                return False
            s.focus()
            if st['how'] == 'key':
                p.keyboard.press(st['key'])
            elif s.input_value() != st['value']:
                s.select_option(st['value'])
        elif op == 'one':
            i = find('input[aria-label="区切り文字"]')
            if not i:
                return False
            i.click()
            p.keyboard.press('Control+a')
            if st['text']:
                p.keyboard.type(st['text'])
            else:
                p.keyboard.press('Backspace')
        elif op == 'widths':
            i = find('input[aria-label="列の幅"]')
            if not i:
                return False
            i.click()
            p.keyboard.press('Control+a')
            if len(st['text']) > 40:
                i.fill(st['text'])
            elif st['text']:
                p.keyboard.type(st['text'])
            else:
                p.keyboard.press('Backspace')
            if st['then'] == 'Enter':
                p.keyboard.press('Enter')
            elif st['then'] == 'Tab':
                p.keyboard.press('Tab')
            elif st['then'] == 'click':
                card.locator('.chead h2').click()
            elif st['then'] == 'CtrlEnter':
                p.keyboard.press('Control+Enter')
        elif op in CHECKS:
            c = find(f'input[data-k="{CHECKS[op]}"]')
            if not c:
                return False
            if st['how'] == 'space':
                c.focus()
                p.keyboard.press('Space')
            else:
                c.click()
        elif op in ('key', 'exclude'):
            group = 'キーにする列' if op == 'key' else '比較しない列'
            i = p.evaluate('([g, n]) => [...document.querySelectorAll(`main.setup [aria-label="${g}"] .chip`)].findIndex(b => b.dataset.name === n)',
                           [group, st['name']])
            if i < 0:
                return False
            if op == 'exclude' and not p.evaluate("document.querySelector('main.setup details.rules').open"):
                p.locator('main.setup details.rules summary').click()
            p.locator(f'main.setup [aria-label="{group}"] .chip').nth(i).click()
        elif op == 'trimrule':
            if not p.evaluate("document.querySelector('main.setup details.rules').open"):
                p.locator('main.setup details.rules summary').click()
            p.locator('main.setup details.rules input[type=checkbox]').click()
        elif op == 'swap':
            p.locator('main.setup .actions button', has_text='左右を入れ替える').click()
        elif op == 'ctrlenter':
            p.keyboard.press('Control+Enter')
        elif op == 'go':
            p.locator('#compareBtn').click(force=True)
        return True

    def finish_compare(self) -> list[Failure]:
        """A comparison was started: checks what was sent, then goes back to the setup and checks it again."""
        p = self.page
        try:
            p.wait_for_function('__fuzz.compares.every(e => e.status !== undefined)', timeout=15000)
            p.wait_for_function("window.__cdr.ready || document.querySelector('main.failed') || "
                                "(document.querySelector('main.setup') && __fuzz.compares.some(e => e.status !== 200))", timeout=30000)
        except PwError:
            return [Failure('compare:timeout', 'high', 'a comparison was started and did not end: screen '
                            + str(p.evaluate("document.querySelector('main') && document.querySelector('main').className")))]
        out = []
        entries = p.evaluate('__fuzz.compares.splice(0)')
        for e in entries:
            if e['status'] != 200:
                self.compares['rejected'] += 1
                out.append(Failure('compare:rejected', 'high', f'the service refused the comparison request ({e["status"]}): {json.dumps(e["body"], ensure_ascii=False)[:300]}'))
            for field, text in diff_request(e['snap'], e['body']):
                out.append(Failure('sent:' + field, 'high', 'a comparison started with conditions other than shown: ' + text))
        if p.evaluate('window.__cdr.ready'):
            self.compares['compare'] += 1
            p.locator('main.compare .toolbar button').filter(has_text=re.compile('^条件を変更$')).click()
        elif p.locator('main.failed').count():
            self.compares['failed'] += 1
            p.locator('main.failed button', has_text='条件を変更する').click()
        p.wait_for_selector('main.setup')
        back = self.settle(True)['snap']
        for field, text in diff_request(back, entries[-1]['body']):
            out.append(Failure('roundtrip:' + field, 'medium', 'after 条件を変更, the setup shows other conditions than compared: ' + text))
        return out

    def run(self, steps: list[dict] | None, rng: random.Random | None = None, length: int = 0, fast: bool = False,
            stop=lambda f: True) -> tuple[list[dict], list[Failure]]:
        """Runs the given steps, or `length` random ones; stops at the first failure for which `stop` is true."""
        self.fresh()
        res = self.settle(True)
        snap, settled = res['snap'], True
        done: list[dict] = []
        failures: list[Failure] = []
        while len(done) < (len(steps) if steps is not None else length):
            st = steps[len(done)] if steps is not None else gen_step(rng, snap, fast, done[-1] if done else None)
            done.append(st)
            try:
                found, snap, settled = self.step(st, snap, settled)
            except PwError as e:
                found = [Failure('driver:error', 'high', f'the test could not go on: {str(e).splitlines()[0]}')]
            for f in found:
                f.step = len(done)
            failures += found
            if any(stop(f) for f in found) or any(f.code == 'driver:error' for f in found):
                return done, failures
        if not settled:
            res = self.settle(True)
            found = check_screen(res['snap']) + self.drain(res)
            for f in found:
                f.step = len(done)
            failures += found
        return done, failures

    def step(self, st: dict, before: dict, settled: bool) -> tuple[list[Failure], dict, bool]:
        """One step and the checks after it. `settled`: no reading was in flight before it."""
        try:
            acted = self.apply(st)
        except PwError as e:
            if 'closed' in str(e):
                raise
            acted = False  # the control went away while acting on it
        wait = st.get('wait', 'idle') == 'idle'
        res = self.settle(wait)
        found = []
        compared = bool(res['compares'])
        if compared:
            found += self.finish_compare()
            if settled and st['op'] in ('go', 'ctrlenter') and before.get('go') is False:
                found.append(Failure(f'{st["op"]}:disabled-started', 'high', f'a comparison started while 比較する was disabled ({describe(st)})'))
            res = self.settle(True)
        found += self.drain(res)
        if wait or compared or res['pending'] == 0:  # with nothing in flight, the screen must be whole
            found += check_screen(res['snap'])
            if settled and acted and not compared:
                found += check_step(st, before, res['snap'], res['toasts'], self.files)
        return found, res['snap'], wait or compared

    def drain(self, res: dict) -> list[Failure]:
        """Script errors, readings that never ended and requests the service refused since the last call."""
        found = [Failure('page:error', 'high', e) for e in self.errors]
        self.errors.clear()
        if res['timeout']:
            found.append(Failure('reading:timeout', 'high', 'a reading did not end within 15 s'))
        return found + self.check_bad(res['bad'])

    def check_bad(self, bad: list[dict]) -> list[Failure]:
        """Requests the service refused; reading the missing file is expected to fail."""
        out = []
        for b in bad:
            body = json.loads(b['body'] or '{}')
            if b['cmd'] == 'preview' and body.get('path') == str(self.files['missing.csv']):
                continue
            out.append(Failure(f'api:{b["cmd"]}', 'high', f'/api/{b["cmd"]} answered {b["status"]} to {json.dumps(body, ensure_ascii=False)[:300]}'))
        return out


# ---- random steps ------------------------------------------------------------------


def gen_step(rng: random.Random, s: dict, fast: bool, last: dict | None = None) -> dict:
    """A random step that the screen `s` offers (a skipped step is harmless).
    Steps on the card the `last` step acted on are likelier: one change made
    while the previous one is still being read is where races are."""
    choices: list[tuple[float, dict]] = []
    if s.get('screen') == 'setup':
        for side in SIDES:
            c = s[side]
            choices.append((25 if not c['file'] else 3, {'op': 'pick', 'side': side,
                                                         'file': rng.choices(list(FILE_WEIGHTS), list(FILE_WEIGHTS.values()))[0]}))
            if not c['controls']:
                continue
            for op, w in (('enc', 5), ('del', 9), ('header', 4)) + ((('quoted', 4),) if c['quoted'] is not None else ()):
                choices.append((w, {'op': op, 'side': side}))
            if c['one'] is not None:
                choices.append((8, {'op': 'one', 'side': side, 'text': rng.choice(ONE_CHARS)}))
            if c['widths'] is not None:
                r = rng.random()
                text = (rng.choice(WIDTHS_VALID) if r < 0.6 else rng.choice(WIDTHS_INVALID) if r < 0.85 else '' if r < 0.98 else WIDTHS_HUGE)
                choices += [(12, {'op': 'widths', 'side': side, 'text': text, 'then': rng.choices(THEN, [4, 2, 2, 2, 1])[0]}),
                            (4, {'op': 'unit', 'side': side}), (3, {'op': 'ftrim', 'side': side})]
        # Readings that can be compared are rare among random settings: make the most of them.
        if s.get('keyChips'):
            choices.append((14 if s['keys'] else 30, {'op': 'key', 'name': rng.choice(s['keyChips'])}))
        if s.get('exChips'):
            choices.append((3, {'op': 'exclude', 'name': rng.choice(s['exChips'])}))
        choices += [(1, {'op': 'trimrule'}), (3, {'op': 'swap'}),
                    (12 if s['go'] else 4, {'op': 'ctrlenter'}), (20 if s['go'] else 5, {'op': 'go'})]
    else:
        choices.append((1, {'op': 'swap'}))
    near = last.get('side') if last else None
    weights = [w * (3 if near and c.get('side') == near else 1) for w, c in choices]
    st = rng.choices([c for _, c in choices], weights)[0]
    if st['op'] in SELECTS:
        if rng.random() < 0.3:
            st.update(how='key', key=rng.choice(['ArrowDown', 'ArrowUp']))
        else:
            st.update(how='mouse', value=rng.choice(SELECTS[st['op']][1]))
    elif st['op'] in CHECKS:
        st['how'] = rng.choice(['click', 'space'])
    st['wait'] = 'none' if fast or rng.random() < 0.2 else 'idle'
    st['delay'] = rng.choice(DELAYS) if st['wait'] == 'none' else 0
    return st


# ---- shrinking ---------------------------------------------------------------------


def reproduces(session: Session, steps: list[dict], code: str, tries: int) -> bool:
    for _ in range(tries):
        _, failures = session.run(steps, stop=lambda f: f.code == code)
        if any(f.code == code for f in failures):
            return True
    return False


def flipped(st: dict) -> dict:
    """The same step on the other card."""
    return {**st, 'side': OTHER[st['side']]} if st.get('side') else st


def shrink(session: Session, steps: list[dict], code: str, budget_s: float = 300) -> list[dict]:
    """A shorter list of steps that still fails with `code`: first every step
    waiting for the readings where the failure stays, then delta debugging,
    then single steps out (a swap together with the sides after it)."""
    end = time.monotonic() + budget_s
    tries = lambda c: 3 if any(x.get('wait') == 'none' or x.get('delay') for x in c) else 1  # noqa: E731
    still = lambda c: reproduces(session, c, code, tries(c))  # noqa: E731
    if not still(steps):
        return steps
    calm = [{**st, 'wait': 'idle', 'delay': 0} for st in steps]
    if still(calm):
        steps = calm
    else:
        for i, st in enumerate(steps):
            if (st.get('wait') == 'none' or st.get('delay')) and time.monotonic() < end:
                cand = steps[:i] + [{**st, 'wait': 'idle', 'delay': 0}] + steps[i + 1:]
                if still(cand):
                    steps = cand
    n = 2
    while len(steps) >= 2 and time.monotonic() < end:
        size = math.ceil(len(steps) / n)
        for i in range(0, len(steps), size):
            cand = steps[:i] + steps[i + size:]
            if still(cand):
                steps, n = cand, max(n - 1, 2)
                break
        else:
            if n >= len(steps):
                break
            n = min(len(steps), n * 2)
    changed = True
    while changed and time.monotonic() < end:
        changed = False
        for i in range(len(steps)):
            later = [flipped(x) for x in steps[i + 1:]] if steps[i]['op'] == 'swap' else steps[i + 1:]
            cand = steps[:i] + later
            if still(cand):
                steps, changed = cand, True
                break
    # The run stops at the failure: steps after it are not needed.
    _, failures = session.run(steps, stop=lambda f: f.code == code)
    hit = [f.step for f in failures if f.code == code]
    return steps[:hit[0]] if hit else steps


# ---- main ----------------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--harness', type=Path, required=True)
    ap.add_argument('--sequences', type=int, default=50)
    ap.add_argument('--seed', type=int, default=None, help='seed of the first sequence (each next one adds 1)')
    ap.add_argument('--steps', default='20-40', help='steps per sequence, as MIN-MAX')
    ap.add_argument('--fast', action='store_true', help='no step waits for the readings to end')
    ap.add_argument('--replay', type=Path, nargs='+', help='run the steps saved in these JSON files (or every JSON file in a directory)')
    ap.add_argument('--out', type=Path, default=Path(tempfile.gettempdir()) / 'ui_fuzz', help='where minimal failures are saved')
    ap.add_argument('--no-shrink', action='store_true')
    a = ap.parse_args()
    lo, hi = (int(x) for x in a.steps.split('-'))
    seed = a.seed if a.seed is not None else random.randrange(1_000_000)

    distinct: dict[str, dict] = {}
    counts: dict[str, int] = {}
    clean = total_steps = 0
    with sync_playwright() as pw, tempfile.TemporaryDirectory() as tmp:
        files = make_files(Path(tmp))
        browser = pw.chromium.launch(executable_path=CHROMIUM if Path(CHROMIUM).exists() else None, args=['--no-sandbox'])
        h = Harness(a.harness, [])
        try:
            page = browser.new_context(viewport={'width': 1366, 'height': 768}, bypass_csp=True).new_page()
            page.set_default_timeout(5000)
            session = Session(page, h.url, files)
            if a.replay:
                paths = [q for r in a.replay for q in (sorted(r.glob('*.json')) if r.is_dir() else [r])]
                failed = 0
                for path in paths:
                    saved = json.loads(path.read_text(encoding='utf-8'))
                    done, failures = session.run(saved['steps'], stop=lambda f: False)
                    print(f'{path.name}: {len(done)} steps, ' + ('clean' if not failures else f'{len(failures)} failures'), flush=True)
                    if failures:
                        failed += 1
                        for i, st in enumerate(done, 1):
                            print(f'  {i:3}. {describe(st)}')
                        for f in failures:
                            print(f'  after step {f.step}: [{f.severity}] {f.code}: {f.text}')
                return 1 if failed else 0
            for n in range(a.sequences):
                s = seed + n
                rng = random.Random(s)
                done, failures = session.run(None, rng, rng.randint(lo, hi), a.fast, stop=lambda f: f.code not in distinct)
                total_steps += len(done)
                for f in failures:
                    counts[f.code] = counts.get(f.code, 0) + 1
                new = [f for f in failures if f.code not in distinct]
                print(f'sequence {n + 1} seed {s}: {len(done)} steps, '
                      + ('clean' if not failures else ', '.join(sorted({f.code for f in failures}))), flush=True)
                if not failures:
                    clean += 1
                for f in new[:1]:
                    steps = done[:f.step]
                    if not a.no_shrink:
                        steps = shrink(session, steps, f.code)
                    _, again = session.run(steps, stop=lambda g: g.code == f.code)
                    seen = next((g for g in again if g.code == f.code), f)
                    distinct[f.code] = {'seed': s, 'fast': a.fast, 'failure': asdict(seen), 'steps': steps,
                                        'reproduced': any(g.code == f.code for g in again)}
                    a.out.mkdir(parents=True, exist_ok=True)
                    path = a.out / f'{re.sub(r"[^a-z0-9]+", "_", f.code)}.json'
                    path.write_text(json.dumps(distinct[f.code], ensure_ascii=False, indent=1), encoding='utf-8')
                    print(f'  new failure {f.code}, {len(steps)} steps, saved to {path}', flush=True)
        finally:
            h.close()
            browser.close()

    print(f'\n{a.sequences} sequences from seed {seed}{" (fast)" if a.fast else ""}: {clean} clean, {total_steps} steps; '
          f'comparisons {session.compares}')
    for code, d in distinct.items():
        f = d['failure']
        print(f'\n[{f["severity"]}] {code} (seen {counts[code]} times; first in seed {d["seed"]}'
              f'{"" if d["reproduced"] else "; not reproduced after shrinking"})')
        print(f'  {f["text"]}')
        for i, st in enumerate(d['steps'], 1):
            print(f'  {i:3}. {describe(st)}')
    return 1 if distinct else 0


if __name__ == '__main__':
    sys.exit(main())
