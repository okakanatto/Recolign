"""Differential test: random small table pairs through `cdr` and tools/oracle.py.

Two kinds of case (--kind; both by default):
- csv: two CSV files with random values (quotes, delimiters, CR/LF inside
  values, empty strings, leading zeros, full-width characters), random row
  and column order, added/deleted/changed records, sometimes added or removed
  columns, duplicate or empty keys, Windows-31J, a BOM, and comparison rules.
- formats: the reading options beyond CSV. One layout for both
  files (cdr takes one layout for both): a delimiter (comma, tab, semicolon,
  `|` or another ASCII character) with or without quotes, or fixed width in
  bytes or characters with or without the padding removed. Each file has its
  own encoding (UTF-8 with or without BOM, Windows-31J, UTF-16 LE/BE with or
  without BOM, EUC-JP), line terminators and padding. Values hold only what
  the layout can hold (no delimiter or line break without quotes, no line
  break in fixed width) and characters that Python and the engine read alike
  (see oracle.py), among them Windows-31J characters whose second byte is the
  delimiter and other bytes for the same Windows-31J character (NEC and IBM
  duplicates). Some files are made to fail on purpose: a line longer than the
  widths, a byte width that splits a character, a wrong BOM, bytes the
  encoding does not allow, byte widths with UTF-16.

Both implementations must either produce identical canonical output or
reject the input for the same reason. The files of a failing case are kept
in a directory named in the report, with the options in opts.json.

  python tools/fuzz_oracle.py --cdr target/release/cdr --cases 500 --seed 1
"""
from __future__ import annotations

import argparse
import functools
import json
import random
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
CHARS = ['a', 'b', 'Z', '0', '1', ' ', ',', '"', '\r', '\n', '\t', ';', 'あ', '髙', '﨑', '①', '～', '∵', 'Ａ', '　']
ENGINE_TO_ORACLE = {
    'unclosed_quote': 'csv', 'quote_in_unquoted_field': 'csv', 'text_after_closing_quote': 'csv',
    'blank_line': 'field_count',
}


def value(rng: random.Random) -> str:
    r = rng.random()
    if r < 0.1:
        return ''
    if r < 0.15:
        return 'NULL'
    if r < 0.2:
        return '00' + str(rng.randint(0, 99))
    return ''.join(rng.choice(CHARS) for _ in range(rng.randint(1, 6)))


def quote(v: str, delim: str, force: bool) -> str:
    if force or any(c in v for c in (delim, '"', '\r', '\n')) or v == '':
        return '"' + v.replace('"', '""') + '"'
    return v


def write(path: Path, headers, rows, delim, enc, bom, term, trailing, rng):
    lines = [delim.join(quote(h, delim, rng.random() < 0.3) for h in headers)]
    lines += [delim.join(quote(v, delim, rng.random() < 0.3) for v in row) for row in rows]
    text = term.join(lines) + (term if trailing else '')
    data = text.encode('utf-8' if enc == 'utf8' else 'cp932')
    path.write_bytes((b'\xef\xbb\xbf' if bom else b'') + data)


def make_tables(rng: random.Random, value=value, key_suffixes=('', ' ', '"', ',', '\n'), reshape=True):
    """Left and right tables with columns c0, c1, ...: the right one has some
    records changed, deleted and added, and (with `reshape`) columns added,
    removed and reordered. Returns the column names, the number of key
    columns (the first ones), and each side's header and rows."""
    ncols = rng.randint(2, 6)
    headers = [f'c{i}' for i in range(ncols)]
    nkeys = 1 if rng.random() < 0.6 else 2
    n = rng.randint(0, 40)
    left = []
    used = set()
    for i in range(n):
        key = tuple(f'k{rng.randint(0, 60)}' + rng.choice(key_suffixes) for _ in range(nkeys))
        if key in used:
            continue
        used.add(key)
        left.append(list(key) + [value(rng) for _ in range(ncols - nkeys)])
    right = []
    for row in left:
        r = rng.random()
        if r < 0.15:
            continue
        row = row[:]
        if r < 0.5:
            for c in range(nkeys, ncols):
                if rng.random() < 0.4:
                    row[c] = rng.choice([value(rng), row[c] + ' ', ' ' + row[c], row[c].upper()])
        right.append(row)
    for _ in range(rng.randint(0, 5)):
        key = tuple(f'n{rng.randint(0, 99)}' for _ in range(nkeys))
        if key not in used:
            used.add(key)
            right.append(list(key) + [value(rng) for _ in range(ncols - nkeys)])
    rng.shuffle(right)
    if rng.random() < 0.05 and left:
        left.append(left[rng.randrange(len(left))][:])  # duplicate key
    if rng.random() < 0.05 and right:
        right.append([''] * nkeys + right[0][nkeys:])  # empty key
    rheaders = headers[:]
    if rng.random() < 0.15 and reshape:  # added / removed column
        rheaders.append('extra')
        right = [r + [value(rng)] for r in right]
    if rng.random() < 0.15 and ncols > nkeys + 1 and reshape:
        drop = ncols - 1
        rheaders.pop(drop)
        right = [r[:drop] + r[drop + 1:] for r in right]
    lheaders = headers[:]
    if rng.random() < 0.03:
        lheaders[-1] = lheaders[0]  # duplicate column name
    perm = list(range(len(rheaders)))
    if reshape:
        rng.shuffle(perm)
    rheaders = [rheaders[i] for i in perm]
    right = [[r[i] for i in perm] for r in right]
    return headers, nkeys, lheaders, left, rheaders, right


def compare_options(rng: random.Random, names, nkeys):
    """Key columns (the first `nkeys` names), sometimes an excluded column and the trim rule."""
    opts = ['--key', names[0]] + (['--key', names[1]] if nkeys == 2 else [])
    if rng.random() < 0.2 and len(names) > nkeys + 1:
        opts += ['--exclude', names[nkeys]]
    if rng.random() < 0.2:
        opts += ['--trim', 'ascii']
    return opts


def make_case(rng: random.Random, d: Path):
    headers, nkeys, lheaders, left, rheaders, right = make_tables(rng)
    delim = rng.choice([',', ',', ',', '\t', ';'])
    enc = 'windows31j' if rng.random() < 0.25 else 'utf8'
    bom = enc == 'utf8' and rng.random() < 0.3
    term = rng.choice(['\r\n', '\n', '\r'])
    write(d / 'l.csv', lheaders, left, delim, enc, bom, term, rng.random() < 0.8, rng)
    write(d / 'r.csv', rheaders, right, delim, enc, bom and rng.random() < 0.5, term, rng.random() < 0.8, rng)
    opts = compare_options(rng, headers, nkeys)
    opts += ['--encoding', enc, '--delimiter', {'\t': 'tab'}.get(delim, delim)]
    return opts


# ---- formats cases ----

ENCODINGS = ['utf8', 'windows31j', 'eucjp', 'utf16le', 'utf16be']
CODECS = {'utf8': 'utf-8', 'windows31j': 'cp932', 'eucjp': 'euc_jp', 'utf16le': 'utf-16-le', 'utf16be': 'utf-16-be'}
BOMS = {'utf8': b'\xef\xbb\xbf', 'utf16le': b'\xff\xfe', 'utf16be': b'\xfe\xff'}
WRONG_BOMS = {'utf8': BOMS['utf16le'], 'utf16le': BOMS['utf16be'], 'utf16be': BOMS['utf16le'],
              'windows31j': BOMS['utf8'], 'eucjp': BOMS['utf8']}
# Bytes the encoding does not allow, put at the start of a line.
BAD_BYTES = {'utf8': b'\xff', 'windows31j': b'\x81\x20', 'eucjp': b'\xa1\x20'}
# ASCII punctuation, controls and line breaks, kana, kanji, full-width and
# half-width forms, NEC and IBM extensions (Windows-31J), JIS X 0212 (three
# bytes in EUC-JP), a character outside the BMP (a surrogate pair in UTF-16),
# C1 controls and line separators other than CR and LF. Each case uses those
# both files' encodings and the layout can hold.
FORMAT_CHARS = list('abZ01 ,";|\t\r\n~\\#@\'') + [
    '\x0b', '\x0c', '\x1f', '\x80', '\x85', '\u2028', 'あ', 'ポ', 'ソ', '表', '十', 'ｱ', 'ﾟ', 'Ａ', '\u3000',
    '∵', '～', '①', 'Ⅰ', 'ⅰ', '￢', '髙', '﨑', '¦', '丂', 'é', '𠮷', '€']
# Characters Python writes as bytes that the engine reads as another
# character or not at all (oracle.py lists the differences).
UNREADABLE = {'windows31j': set('\uf8f0\uf8f1\uf8f2\uf8f3'), 'eucjp': set('\u301c\u2016\u2212\xa2\xa3\xac')}
# Other bytes for the same character in Windows-31J (NEC and IBM duplicates):
# the engine must compare the characters, not the bytes.
W31J_ALTERNATES = {'∵': [b'\x87\x9a', b'\xfa\x5b'], '髙': [b'\xfb\xfc'], '﨑': [b'\xfa\xb1'],
                   '￢': [b'\xee\xf9', b'\xfa\x54'], 'Ⅰ': [b'\xfa\x4a'], 'ⅰ': [b'\xfa\x40']}
OTHER_DELIMITERS = [chr(c) for c in range(1, 128) if chr(c) not in '"\r\n,\t;|']
PADDING = ' \u3000'


@functools.lru_cache(maxsize=None)
def readable(ch: str, enc: str) -> bool:
    """`ch` written by Python in `enc` reads back as `ch`, in Python and in the engine."""
    try:
        same = ch.encode(CODECS[enc]).decode(CODECS[enc]) == ch
    except UnicodeError:
        return False
    return same and ch not in UNREADABLE.get(enc, ())


def second_byte_chars(delim: str):
    """Windows-31J characters whose second byte is the delimiter (0x40 and up)."""
    out = []
    for lead in (0x81, 0x83, 0x88, 0x95, 0xe0, 0xfa):
        try:
            ch = bytes([lead, ord(delim)]).decode('cp932')
        except UnicodeDecodeError:
            continue
        if len(ch) == 1 and readable(ch, 'windows31j'):
            out.append(ch)
    return out


def encode(text: str, enc: str, bom: bool, rng: random.Random) -> bytes:
    if enc == 'windows31j' and rng.random() < 0.3:
        data = b''.join(rng.choice([c.encode('cp932')] + W31J_ALTERNATES.get(c, [])) for c in text)
    else:
        data = text.encode(CODECS[enc])
    return (BOMS[enc] if bom else b'') + data


def spoil(data: bytes, enc: str, rng: random.Random) -> bytes:
    """Rarely, a wrong BOM or bytes the encoding does not allow."""
    r = rng.random()
    if r < 0.02:
        return WRONG_BOMS[enc] + data.removeprefix(BOMS.get(enc, b''))
    if r < 0.04:
        if enc.startswith('utf16'):  # an odd number of bytes, or a lone surrogate at the end
            if len(data) % 2 == 0 and rng.random() < 0.5:
                return data[:-1]
            return data + '\ud800'.encode(CODECS[enc], 'surrogatepass')
        starts = [0] + [i + 1 for i, b in enumerate(data) if b in b'\r\n']
        at = rng.choice(starts)
        return data[:at] + BAD_BYTES[enc] + data[at:]
    return data


def join_lines(lines, rng: random.Random) -> str:
    """Lines ended by CRLF, LF or CR (sometimes mixed); the last one sometimes by nothing."""
    term = rng.choice(['\r\n', '\n', '\r'])
    mixed = rng.random() < 0.1
    ends = [rng.choice(['\r\n', '\n', '\r']) if mixed else term for _ in lines]
    if ends and rng.random() < 0.2:
        ends[-1] = ''
    return ''.join(line + end for line, end in zip(lines, ends))


def make_format_case(rng: random.Random, d: Path):
    encs = [rng.choice(ENCODINGS), rng.choice(ENCODINGS)]
    fixed = rng.random() < 0.45
    no_header = rng.random() < 0.1
    if fixed:
        unit = rng.choice(['bytes', 'chars'])
        trim = rng.random() < 0.7
        if unit == 'bytes' and rng.random() < 0.97:  # UTF-16 takes widths in characters only
            encs = [rng.choice(['utf8', 'windows31j', 'eucjp']) if e.startswith('utf16') else e for e in encs]
        delim, forbidden, suffixes = None, '\r\n', ('', '"', ',', 'ポ')
    else:
        quoted = rng.random() < 0.5
        delim = rng.choice([',', '\t', ';', '|', 'other'])
        if 'windows31j' in encs and rng.random() < 0.4:
            delim = rng.choice(['|', '\\', 'other'])  # can be the second byte of a character
        if delim == 'other':
            # Without quotes, names and keys (letters and digits) must not hold the delimiter.
            delim = rng.choice([c for c in OTHER_DELIMITERS if quoted or not c.isalnum()])
        forbidden = '' if quoted else '\r\n' + delim
        suffixes = ('', ' ', '"', delim, '\n', 'ポ') if quoted else ('', ' ', '"', 'ポ')
    lenc, renc = encs
    pool = [c for c in FORMAT_CHARS if readable(c, lenc) and readable(c, renc)]
    if 'windows31j' in encs and delim is not None and ord(delim) >= 0x40:
        pool += [c for c in second_byte_chars(delim) if readable(c, lenc) and readable(c, renc)]

    def fit(v: str) -> str:
        """`v` with only characters the layout and both encodings can hold."""
        v = ''.join(c if c not in forbidden and readable(c, lenc) and readable(c, renc) else 'x' for c in v)
        return v.strip(PADDING) if fixed and trim else v

    def format_value(rng: random.Random) -> str:
        r = rng.random()
        if r < 0.1:
            return ''
        if r < 0.15:
            return 'NULL'
        if r < 0.2:
            return '00' + str(rng.randint(0, 99))
        return fit(''.join(rng.choice(pool) for _ in range(rng.randint(1, 6))))

    headers, nkeys, lheaders, left, rheaders, right = make_tables(rng, format_value, suffixes, reshape=not no_header)
    left = [[fit(v) for v in row] for row in left]
    right = [[fit(v) for v in row] for row in right]
    sides = [(lheaders, left, lenc), (rheaders, right, renc)]
    if fixed:
        # A fixed-width file has a value for every width: give the file with
        # fewer columns columns of its own.
        for tag, (heads, rows, _) in zip('lr', sides):
            while len(heads) < max(len(lheaders), len(rheaders)):
                heads.append(f'{tag}x{len(heads)}')
                for row in rows:
                    row.append(format_value(rng))
        lines, names, opts = fixed_lines(rng, sides, unit, trim, no_header)
    else:
        lines = []
        for heads, rows, _ in sides:
            rows = rows if no_header else [heads] + rows
            if quoted:
                lines.append([delim.join(quote(v, delim, rng.random() < 0.3) for v in row) for row in rows])
            else:
                lines.append([delim.join(row) for row in rows])
        names = lheaders
        opts = ['--delimiter', {'\t': 'tab'}.get(delim, delim)] + ([] if quoted else ['--no-quotes'])
    if no_header:
        names = [f'列{i}' for i in range(1, len(lheaders) + 1)]
    for name, enc, side_lines in zip(('l.csv', 'r.csv'), encs, lines):
        bom = enc in BOMS and rng.random() < (0.5 if enc.startswith('utf16') else 0.3)
        (d / name).write_bytes(spoil(encode(join_lines(side_lines, rng), enc, bom, rng), enc, rng))
    opts = compare_options(rng, names, nkeys) + ['--left-encoding', lenc, '--right-encoding', renc] + opts
    return opts + (['--no-header'] if no_header else [])


def fixed_lines(rng: random.Random, sides, unit: str, trim: bool, no_header: bool):
    """Each side's lines in fixed width, the left column names as the engine
    will read them, and the layout options."""
    def size(v: str, enc: str) -> int:
        return len(v) if unit == 'chars' else len(v.encode(CODECS[enc] if not enc.startswith('utf16') else 'utf-8'))

    widths = [1] * len(sides[0][0])
    for heads, rows, enc in sides:
        for row in rows if no_header else [heads] + rows:
            for p, v in enumerate(row):
                widths[p] = max(widths[p], size(v, enc))
    widths = [w + rng.choice([0, 0, 1, 2, 3]) for w in widths]
    if not trim and not no_header:
        widths = [max(widths)] * len(widths)  # padded column names match only in columns of one width

    def pad(v: str, w: int, enc: str, right_align: bool) -> str:
        n = w - size(v, enc)
        full = rng.randint(0, n // size('\u3000', enc)) if rng.random() < 0.2 else 0
        space = '\u3000' * full + ' ' * (n - full * size('\u3000', enc))
        return space + v if right_align else v + space

    lines = []
    for heads, rows, enc in sides:
        align = rng.choice(['left', 'right', 'mixed'])
        drop_trailing = rng.random() < 0.3  # as tools that remove trailing spaces
        side = [] if no_header else [''.join(h.ljust(w) for h, w in zip(heads, widths))]
        for row in rows:
            right_align = [align == 'right' or (align == 'mixed' and rng.random() < 0.5) for _ in row]
            line = ''.join(pad(v, w, enc, r) for v, w, r in zip(row, widths, right_align))
            side.append(line.rstrip(' ') if drop_trailing else line)
        lines.append(side)

    s = rng.randrange(2)
    if rng.random() < 0.06 and lines[s]:  # a line longer than the widths
        i = rng.randrange(len(lines[s]))
        lines[s][i] += 'X' * (sum(widths) - size(lines[s][i], sides[s][2]) + rng.randint(1, 3))
    if rng.random() < (0.15 if unit == 'bytes' else 0.05) and len(widths) > 1:
        # Move one column boundary: in bytes it may split a character.
        k = rng.randrange(len(widths) - 1)
        delta = rng.choice([-2, -1, 1, 2])
        if widths[k] + delta >= 1 and widths[k + 1] - delta >= 1:
            widths[k] += delta
            widths[k + 1] -= delta

    names = []
    if not no_header and lines[0]:  # the left header (ASCII) as cut by the widths
        header, pos = lines[0][0], 0
        for w in widths:
            names.append(header[pos:pos + w].strip(PADDING) if trim else header[pos:pos + w])
            pos += w
    opts = ['--fixed', ','.join(map(str, widths)), '--fixed-unit', unit] + ([] if trim else ['--keep-padding'])
    return lines, names, opts


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument('--cdr', type=Path, required=True)
    ap.add_argument('--cases', type=int, default=300)
    ap.add_argument('--seed', type=int, default=1)
    ap.add_argument('--kind', choices=['csv', 'formats', 'both'], default='both')
    a = ap.parse_args()
    rng = random.Random(a.seed)
    tally = {'match': 0, 'both_rejected': 0}
    kinds = {}
    failures = []
    with tempfile.TemporaryDirectory() as tmp:
        d = Path(tmp)
        for case in range(a.cases):
            kind = a.kind if a.kind != 'both' else ('csv' if rng.random() < 0.4 else 'formats')
            opts = make_case(rng, d) if kind == 'csv' else make_format_case(rng, d)
            kinds[kind] = kinds.get(kind, 0) + 1
            files = ['--left', str(d / 'l.csv'), '--right', str(d / 'r.csv')]
            (d / 'c.jsonl').unlink(missing_ok=True)  # never check the output of an earlier case
            eng = subprocess.run([str(a.cdr), 'compare', *files, *opts, '--canonical', str(d / 'c.jsonl')],
                                 capture_output=True, text=True, encoding='utf-8')
            ok = eng.returncode == 0
            ecode = None
            if eng.returncode == 2:
                ecode = json.loads(eng.stdout)['error']['code']
                ecode = ENGINE_TO_ORACLE.get(ecode, ecode)
            orc = subprocess.run([sys.executable, '-I', str(ROOT / 'oracle.py'), *files, *opts,
                                  '--canonical', str(d / 'c.jsonl')], capture_output=True, text=True, encoding='utf-8')
            res = json.loads(orc.stdout) if orc.stdout else {'oracle': 'crash', 'stderr': orc.stderr[-500:]}
            if ok and res['oracle'] == 'match':
                tally['match'] += 1
            elif ecode and res['oracle'] == 'rejected' and res['code'] == ecode:
                tally['both_rejected'] += 1
                tally[ecode] = tally.get(ecode, 0) + 1
            else:
                keep = Path(tmp).parent / f'cdr-fuzz-fail-{a.seed}-{case}'
                keep.mkdir(exist_ok=True)
                for f in ('l.csv', 'r.csv'):
                    (keep / f).write_bytes((d / f).read_bytes())
                (keep / 'opts.json').write_text(json.dumps(opts, ensure_ascii=False), encoding='utf-8')
                failures.append({'case': case, 'kind': kind, 'kept': str(keep), 'opts': opts,
                                 'engine': (eng.stdout + eng.stderr)[-800:], 'oracle': res})
    print(json.dumps({'seed': a.seed, 'cases': a.cases, 'kinds': kinds, 'tally': tally, 'failures': failures[:5],
                      'failure_count': len(failures)}, ensure_ascii=False, indent=1))
    return 0 if not failures else 1


if __name__ == '__main__':
    sys.exit(main())
