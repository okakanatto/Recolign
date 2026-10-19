"""Independent reference implementation used to check the engine.

Uses only the Python standard library (csv, codecs, hashlib). It shares no
code with the Rust engine. Given the same inputs it computes the units in
key-aligned order, their original positions, changed/ignored values and full
change patterns, then compares them with the engine's canonical JSON Lines
(`cdr compare --canonical FILE`).

Reading:
- Delimited text: the delimiter is one ASCII character other than `"`, CR,
  LF and NUL. With quotes (the default) records are read by Python's csv
  module. With --no-quotes every line is a record, `"` is an ordinary
  character and values are the line split at the delimiter.
- Fixed width (--fixed W,W,...): every line is a record. Columns are cut by
  widths in bytes of the file's encoding or in characters (--fixed-unit).
  Spaces (U+0020, U+3000) around each value are padding and removed, unless
  --keep-padding. A line longer than the widths in total and a byte width
  that ends inside a character are rejected; a short line leaves the last
  columns empty. Character boundaries are where Python's incremental decoder
  completes a character.
- Lines end with CRLF, LF or a lone CR. A terminator at the end of the file
  does not start another record.
- Encodings: utf8 (BOM EF BB BF skipped), windows31j (Python's cp932), eucjp
  (Python's euc_jp), utf16le and utf16be (BOM FF FE / FE FF skipped). A BOM
  of another encoding is rejected.

Byte positions ("l"/"r" byte_start and byte_end) are offsets in the file,
with one convention taken from the engine: UTF-16 files, and Windows-31J
files read with a delimiter of 0x40 or above (it can be the second byte of a
character), are converted to UTF-8 before they are parsed, and their
positions are offsets in that UTF-8 text (BOM excluded). The oracle computes
the positions of those files in the same converted text.

Exit status: 0 = identical, 1 = differences found, 2 = the oracle rejected
the input (the message says why).

Known semantic differences from the engine, by design of the reference
(found by decoding every 1-, 2- and 3-byte sequence with encoding_rs 0.8.42,
the engine's WHATWG decoders, and with Python's codecs):
- Windows-31J is decoded with Python's cp932 codec. It maps the single bytes
  0xA0 and 0xFD-0xFF to U+F8F0-U+F8F3 where the engine reports an error.
  Every other byte and two-byte sequence decodes the same in both (0x80 is
  U+0080 in both). Test data avoids those four bytes.
- EUC-JP is decoded with Python's euc_jp codec. It rejects the NEC special
  characters (first byte 0xAD, e.g. AD A1 = U+2460) and the IBM extensions
  (first byte 0xF9-0xFC), which the engine reads. Seven sequences decode to
  different characters (engine / Python): A1 C1 U+FF5E / U+301C,
  A1 C2 U+2225 / U+2016, A1 DD U+FF0D / U+2212, A1 F1 U+FFE0 / U+00A2,
  A1 F2 U+FFE1 / U+00A3, A2 CC U+FFE2 / U+00AC, 8F A2 B7 U+FF5E / U+007E.
  Test data avoids them.
- With quotes, a `"` inside an unquoted field is accepted by Python's csv
  module; the engine rejects it. Invalid-CSV handling is covered by the
  engine's tests.
"""
from __future__ import annotations

import argparse
import codecs
import csv
import hashlib
import io
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path

csv.field_size_limit(2**31 - 1)  # the largest a C long holds on Windows too

CODECS = {'utf8': 'utf-8', 'windows31j': 'cp932', 'eucjp': 'euc_jp',
          'utf16le': 'utf-16-le', 'utf16be': 'utf-16-be'}
BOMS = {'utf8': b'\xef\xbb\xbf', 'utf16le': b'\xff\xfe', 'utf16be': b'\xfe\xff'}
PADDING = ' \u3000'  # removed around fixed-width values


class Rejected(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


TERMINATOR = re.compile(rb'\r\n|\r|\n')
TEXT_TERMINATOR = re.compile(r'\r\n|\r|\n')


@dataclass
class Layout:
    """How a file is cut into records and values."""
    delimiter: str = ','
    quoted: bool = True
    widths: list[int] | None = None  # fixed width when set
    unit: str = 'bytes'  # of the widths: 'bytes' or 'chars'
    trim: bool = True  # fixed width: remove the padding around values

    @property
    def fixed(self) -> bool:
        return self.widths is not None

    @property
    def line_based(self) -> bool:
        """Every line is one record (fixed width, or delimited without quotes)."""
        return self.fixed or not self.quoted

    def problem(self, encoding: str) -> str | None:
        """Why these options cannot be used for this encoding, if they cannot."""
        if not self.fixed:
            d = self.delimiter
            if len(d) != 1 or not d.isascii() or d in '"\r\n\0':
                return 'the delimiter must be one ASCII character other than ", CR, LF and NUL'
        elif not self.widths or 0 in self.widths or sum(self.widths) > 0xFFFFFFFF:
            return 'fixed widths must be 1 or more'
        elif self.unit == 'bytes' and encoding.startswith('utf16'):
            return 'UTF-16 needs fixed widths in characters'
        return None

    def converts_to_utf8(self, encoding: str) -> bool:
        """The engine parses the file as UTF-8 converted from it (positions follow)."""
        if encoding.startswith('utf16'):
            return True
        return encoding == 'windows31j' and not self.fixed and ord(self.delimiter) >= 0x40


def line_spans(base: bytes, start: int):
    """Start and end (terminator excluded) of each physical line of base[start:].
    A terminator at the very end leaves an empty last span that is no line."""
    starts, ends = [start], []
    for m in TERMINATOR.finditer(base, start):
        ends.append(m.start())
        starts.append(m.end())
    ends.append(len(base))
    return starts, ends


def read_csv(text: str, delimiter: str, name: str):
    """Records of quoted delimited text: (values, first line, last line)."""
    reader = csv.reader(io.StringIO(text, newline=''), delimiter=delimiter, strict=True)
    prev = 0
    try:
        for rec in reader:
            yield rec or [''], prev + 1, reader.line_num
            prev = reader.line_num
    except csv.Error as e:
        raise Rejected('csv', f'{name}: line {reader.line_num}: {e}') from None


def char_sizes(data: bytes, codec: str) -> list[int]:
    """Bytes of each character of valid `data`: a character ends at the byte
    where Python's incremental decoder outputs it."""
    decoder = codecs.getincrementaldecoder(codec)()
    sizes, pending = [], 0
    for i in range(len(data)):
        pending += 1
        out = decoder.decode(data[i:i + 1])
        if out:
            assert len(out) == 1, (data, codec)
            sizes.append(pending)
            pending = 0
    return sizes


def cut_fixed(line: bytes, codec: str, layout: Layout, where: str) -> list[str]:
    """Values of one fixed-width line."""
    text = line.decode(codec)
    sizes = [1] * len(text) if layout.unit == 'chars' else char_sizes(line, codec)
    length, total = sum(sizes), sum(layout.widths)
    if length > total:
        raise Rejected('fixed_too_long', f'{where}: {length} {layout.unit}, the widths total {total}')
    values = []
    i = used = end = 0  # characters taken, units taken, end of the column
    for column, width in enumerate(layout.widths, 1):
        end += width
        first = i
        while i < len(text) and used + sizes[i] <= end:
            used += sizes[i]
            i += 1
        if i < len(text) and used < end:
            raise Rejected('fixed_splits_character', f'{where}: column {column} ends inside {text[i]!r}')
        values.append(text[first:i])
    if layout.trim:
        values = [v.strip(PADDING) for v in values]
    return values


def read_lines(base: bytes, starts, ends, codec: str, layout: Layout, name: str):
    """Records of the line-based layouts (unquoted or fixed width): one per line."""
    # A terminator at the very end of the file does not start another record.
    count = len(starts) - (1 if starts[-1] == len(base) else 0)
    for i in range(count):
        line = base[starts[i]:ends[i]]
        if layout.fixed:
            values = cut_fixed(line, codec, layout, f'{name}: line {i + 1}')
        else:
            values = line.decode(codec).split(layout.delimiter)
        yield values, i + 1, i + 1


def load(path: Path, encoding: str, layout: Layout, has_header: bool):
    problem = layout.problem(encoding)
    if problem:
        raise Rejected('options', f'{path.name}: {problem}')
    raw = path.read_bytes()
    sha = hashlib.sha256(raw).hexdigest()
    bom = BOMS.get(encoding, b'')
    if not (bom and raw.startswith(bom)):
        bom = b''
        for other, mark in BOMS.items():
            if other != encoding and raw.startswith(mark):
                raise Rejected('bom_mismatch', f'{path.name}: {other} BOM with {encoding}')
    if len(raw) == len(bom):
        raise Rejected('empty_file', f'{path.name}: empty')
    try:
        text = raw[len(bom):].decode(CODECS[encoding])
    except UnicodeDecodeError as e:
        raise Rejected('decode', f'{path.name}: {e}') from None

    # The bytes record positions refer to (see the docstring), and their codec.
    if layout.converts_to_utf8(encoding):
        base, codec, start = text.encode('utf-8'), 'utf-8', 0
    else:
        base, codec, start = raw, CODECS[encoding], len(bom)
    starts, ends = line_spans(base, start)
    if layout.line_based:
        reader = read_lines(base, starts, ends, codec, layout, path.name)
    else:
        reader = read_csv(text, layout.delimiter, path.name)
    rows = []
    for rec, first, last in reader:
        if rows and len(rec) != len(rows[0][0]):
            raise Rejected('field_count', f'{path.name}: line {first}: {len(rec)} fields, expected {len(rows[0][0])}')
        rows.append((rec, first, last))
    if not rows:
        raise Rejected('empty_file', f'{path.name}: no records')
    width = len(rows[0][0])
    if has_header:
        headers = rows[0][0]
        data = rows[1:]
    else:
        headers = [f'列{i}' for i in range(1, width + 1)]
        data = rows
    positions = []
    for n, (rec, first, last) in enumerate(data, 1):
        positions.append([n, first, last, starts[first - 1], ends[last - 1]])
    return {'path': path, 'base': base, 'codec': codec, 'layout': layout, 'sha256': sha,
            'lines': TEXT_TERMINATOR.split(text) if layout.line_based else None,
            'headers': headers, 'rows': [r[0] for r in data], 'pos': positions, 'encoding': encoding}


def check_positions(t, sample_every: int = 1) -> int:
    """Each record's byte range, decoded on its own, must give the same record:
    the same values when re-parsed (quoted text), or the same line as the
    decoded file split into lines (line-based layouts)."""
    layout, lines = t['layout'], t['lines']
    bad = 0
    for i in range(0, len(t['rows']), sample_every):
        _, first, _, a, b = t['pos'][i]
        piece = t['base'][a:b].decode(t['codec'])
        if lines is not None:
            ok = piece == lines[first - 1]
        else:
            got = next(csv.reader(io.StringIO(piece, newline=''), delimiter=layout.delimiter, strict=True), [''])
            ok = (got or ['']) == t['rows'][i]
        if not ok:
            bad += 1
    return bad


def column_mapping(lh, rh):
    for side, hs in (('left', lh), ('right', rh)):
        if any(h == '' for h in hs) or len(set(hs)) != len(hs):
            raise Rejected('column_mapping', f'{side}: empty or duplicate column names')
    rpos = {h: j for j, h in enumerate(rh)}
    lset = set(lh)
    pairs = [(i, rpos[h]) for i, h in enumerate(lh) if h in rpos]
    left_only = [i for i, h in enumerate(lh) if h not in rpos]
    right_only = [j for j, h in enumerate(rh) if h not in lset]
    return pairs, left_only, right_only


def key_index(t, cols):
    index, dups, empty = {}, 0, 0
    for i, rec in enumerate(t['rows']):
        key = tuple(rec[c] for c in cols)
        if any(v == '' for v in key):
            empty += 1
        elif key in index:
            dups += 1
        else:
            index[key] = i
    return index, dups, empty


def compute(left, right, keys, excluded, trim):
    pairs, left_only, right_only = column_mapping(left['headers'], right['headers'])
    def resolve(names):
        out = []
        for name in names:
            ks = [k for k, (lc, _) in enumerate(pairs) if left['headers'][lc] == name]
            if not ks:
                raise Rejected('key_column', f'column {name!r} is not in both files')
            out.append(ks[0])
        return out
    key_pairs = resolve(keys)
    excl = set(resolve(excluded))
    if len(set(key_pairs)) != len(key_pairs) or not key_pairs or excl & set(key_pairs):
        raise Rejected('key_column', 'invalid key/excluded selection')
    lcols = [pairs[k][0] for k in key_pairs]
    rcols = [pairs[k][1] for k in key_pairs]
    lindex, ldup, lempty = key_index(left, lcols)
    rindex, rdup, rempty = key_index(right, rcols)
    if ldup or lempty or rdup or rempty:
        raise Rejected('key_validation', f'duplicates L{ldup} R{rdup}, empty L{lempty} R{rempty}')

    compared = [(k, lc, rc) for k, (lc, rc) in enumerate(pairs) if k not in set(key_pairs)]
    units = []
    matched = [False] * len(right['rows'])
    for i, rec in enumerate(left['rows']):
        key = tuple(rec[c] for c in lcols)
        j = rindex.get(key)
        if j is None:
            units.append(['deleted', i, None, list(key), [], []])
            continue
        matched[j] = True
        other = right['rows'][j]
        changed, ignored = [], []
        for k, lc, rc in compared:
            a, b = rec[lc], other[rc]
            if a == b:
                continue
            if k in excl or (trim == 'ascii' and a.strip(' \t') == b.strip(' \t')):
                ignored.append([lc, rc, a, b])
            else:
                changed.append([lc, rc, a, b])
        units.append(['changed' if changed else 'same', i, j, list(key), changed, ignored])
    for j, rec in enumerate(right['rows']):
        if not matched[j]:
            units.append(['added', None, j, [rec[c] for c in rcols], [], []])

    groups = {}
    for u, unit in enumerate(units):
        if unit[0] == 'changed':
            groups.setdefault(tuple(tuple(x) for x in unit[4]), []).append(u)
    ordered = sorted(groups.values(), key=lambda m: (-len(m), m[0]))
    pattern_of = {}
    for pid, members in enumerate(ordered):
        for u in members:
            pattern_of[u] = pid

    counts = {'left_records': len(left['rows']), 'right_records': len(right['rows']), 'units': len(units),
              'same': 0, 'same_with_ignored': 0, 'changed': 0, 'deleted': 0, 'added': 0}
    for unit in units:
        counts[unit[0]] += 1
        if unit[0] == 'same' and unit[5]:
            counts['same_with_ignored'] += 1
    head = {'pairs': [list(p) for p in pairs], 'left_only': left_only, 'right_only': right_only,
            'keys': key_pairs, 'counts': counts, 'patterns': len(ordered)}
    return head, units, pattern_of


def unit_line(u, unit, left, right, pattern_of):
    kind, i, j, key, changed, ignored = unit
    return {'u': u, 't': kind, 'k': key,
            'l': left['pos'][i] if i is not None else None,
            'r': right['pos'][j] if j is not None else None,
            'c': changed, 'i': ignored, 'p': pattern_of.get(u)}


def check_canonical(path: Path, left, right, head, units, pattern_of, sample_every: int = 1) -> list[str]:
    """Differences between the engine's canonical output and the oracle's result (at most about 20)."""
    problems = []
    for side, t in (('left', left), ('right', right)):
        bad = check_positions(t, sample_every)
        if bad:
            problems.append(f'{side}: {bad} records whose byte range does not re-parse to the same values')
    with path.open(encoding='utf-8') as f:
        eng_head = json.loads(f.readline())
        expect_head = dict(head)
        expect_head['left'] = {'headers': left['headers'], 'records': len(left['rows']), 'sha256': left['sha256']}
        expect_head['right'] = {'headers': right['headers'], 'records': len(right['rows']), 'sha256': right['sha256']}
        for k, v in expect_head.items():
            if eng_head.get(k) != v:
                problems.append(f'header field {k}: engine={eng_head.get(k)!r} oracle={v!r}')
        count = 0
        for n, line in enumerate(f):
            count += 1
            if n >= len(units):
                problems.append('engine has more units than the oracle')
                break
            try:
                got = json.loads(line)
            except json.JSONDecodeError as e:
                got = f'unparsable line: {e}'
            want = unit_line(n, units[n], left, right, pattern_of)
            if got != want:
                problems.append(f'unit {n}: engine={got} oracle={want}')
                if len(problems) > 20:
                    break
        if count < len(units) and len(problems) <= 20:
            problems.append(f'engine has {count} units, oracle {len(units)}')
    return problems


def main() -> int:
    # JSON out in UTF-8 whatever the console's code page (Windows: cp1252 or cp932).
    sys.stdout.reconfigure(encoding='utf-8')
    encodings = list(CODECS)
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--left', type=Path, required=True)
    ap.add_argument('--right', type=Path, required=True)
    ap.add_argument('--key', action='append', required=True)
    ap.add_argument('--exclude', action='append', default=[])
    ap.add_argument('--trim', choices=['none', 'ascii'], default='none')
    ap.add_argument('--encoding', choices=encodings)
    ap.add_argument('--left-encoding', choices=encodings, default='utf8')
    ap.add_argument('--right-encoding', choices=encodings, default='utf8')
    ap.add_argument('--delimiter', default=',', help='one ASCII character, or tab')
    ap.add_argument('--no-quotes', action='store_true', help='" is an ordinary character; every line is a record')
    ap.add_argument('--fixed', metavar='W,W,...', help='fixed-width columns of these widths')
    ap.add_argument('--fixed-unit', choices=['bytes', 'chars'], default='bytes')
    ap.add_argument('--keep-padding', action='store_true', help='keep the spaces around fixed-width values')
    ap.add_argument('--no-header', action='store_true')
    ap.add_argument('--canonical', type=Path, required=True, help='engine output to check')
    ap.add_argument('--position-sample-every', type=int, default=1)
    a = ap.parse_args()
    if a.encoding:
        a.left_encoding = a.right_encoding = a.encoding
    layout = Layout(delimiter='\t' if a.delimiter in ('tab', '\\t') else a.delimiter, quoted=not a.no_quotes)
    if a.fixed is not None:
        try:
            layout.widths = [int(w) for w in a.fixed.split(',')]
        except ValueError:
            ap.error(f'--fixed: not a list of numbers: {a.fixed!r}')
        layout.unit = a.fixed_unit
        layout.trim = not a.keep_padding
    try:
        left = load(a.left, a.left_encoding, layout, not a.no_header)
        right = load(a.right, a.right_encoding, layout, not a.no_header)
        head, units, pattern_of = compute(left, right, a.key, a.exclude, a.trim)
    except Rejected as e:
        print(json.dumps({'oracle': 'rejected', 'code': e.code, 'message': str(e)}, ensure_ascii=False))
        return 2

    problems = check_canonical(a.canonical, left, right, head, units, pattern_of, a.position_sample_every)
    result = {'oracle': 'match' if not problems else 'mismatch', 'counts': head['counts'],
              'patterns': head['patterns'], 'problems': problems[:20]}
    print(json.dumps(result, ensure_ascii=False))
    return 0 if not problems else 1


if __name__ == '__main__':
    sys.exit(main())
