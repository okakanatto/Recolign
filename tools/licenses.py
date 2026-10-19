"""Collects the license texts of everything built into the Windows exe,
after this program's own license (LICENSE).

The exe contains the Rust crates the desktop app depends on (normal
dependencies for x86_64-pc-windows-msvc, from `cargo metadata`) and the npm
packages bundled into the screens (`dependencies` of app/package.json).
For each package, the license files shipped in its source (LICENSE*,
COPYING*, NOTICE*, COPYRIGHT*) are copied; identical texts are written once
with the list of packages that use them. A package without a license file
is listed with its license expression and repository.

  python tools/licenses.py [--out app/public/third-party-licenses.txt]

The output is placed in app/public, so Vite copies it into the screens that
the exe embeds, and the app can show it (設定 → ライセンス情報).
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TARGET = 'x86_64-pc-windows-msvc'
LICENSE_FILE = re.compile(r'^(licen[cs]e|copying|notice|copyright)([-._].*)?$', re.I)


# For the few packages whose source has no license file: the standard texts
# of the licenses that need the copyright notice reproduced, filled in with
# the package's authors. (BSL-1.0 and MPL-2.0 texts come from other packages
# in the list; MPL-2.0 sources are available at the repository shown.)
MIT = """Copyright (c) {holders}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE."""

BSD3 = """Copyright (c) {holders}

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived from
   this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE."""


def fallback_text(p: dict) -> str | None:
    holders = ', '.join(p['authors']) or f"the authors of {p['repository'] or p['name']}"
    ids = re.split(r'\s+(?:OR|AND)\s+|[()/]', p['license'])
    if 'MIT' in ids:
        return MIT.format(holders=holders)
    if 'BSD-3-Clause' in ids:
        return BSD3.format(holders=holders)
    return None


def license_files(d: Path) -> list[Path]:
    if not d.is_dir():
        return []
    return sorted(p for p in d.iterdir() if p.is_file() and LICENSE_FILE.match(p.name))


def rust_packages() -> list[dict]:
    meta = json.loads(subprocess.run(
        ['cargo', 'metadata', '--format-version', '1', '--locked', '--manifest-path', str(ROOT / 'app' / 'src-tauri' / 'Cargo.toml'),
         '--filter-platform', TARGET],
        check=True, capture_output=True, text=True, encoding='utf-8').stdout)
    pkgs = {p['id']: p for p in meta['packages']}
    nodes = {n['id']: n for n in meta['resolve']['nodes']}
    root = meta['resolve']['root']
    # Normal (not build or dev) dependencies, transitively: what is linked into the exe.
    seen, todo = set(), [root]
    while todo:
        i = todo.pop()
        if i in seen:
            continue
        seen.add(i)
        for d in nodes[i]['deps']:
            if any(k['kind'] is None for k in d['dep_kinds']):
                todo.append(d['pkg'])
    out = []
    for i in seen:
        p = pkgs[i]
        if p['source'] is None:
            continue  # this project's own crates
        out.append({'name': p['name'], 'version': p['version'], 'license': p.get('license') or '', 'authors': p.get('authors') or [],
                    'repository': p.get('repository') or '', 'dir': Path(p['manifest_path']).parent})
    return out


def npm_packages() -> list[dict]:
    pkg = json.loads((ROOT / 'app' / 'package.json').read_text(encoding='utf-8'))
    out = []
    for name in pkg.get('dependencies', {}):
        d = ROOT / 'app' / 'node_modules' / name
        m = json.loads((d / 'package.json').read_text(encoding='utf-8'))
        repo = m.get('repository')
        out.append({'name': name, 'version': m['version'], 'license': m.get('license', ''), 'authors': [],
                    'repository': repo.get('url', '') if isinstance(repo, dict) else (repo or ''), 'dir': d})
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--out', type=Path, default=ROOT / 'app' / 'public' / 'third-party-licenses.txt')
    a = ap.parse_args()
    pkgs = sorted(rust_packages() + npm_packages(), key=lambda p: (p['name'].lower(), p['version']))
    texts: dict[str, list[str]] = {}
    missing = []
    for p in pkgs:
        label = f"{p['name']} {p['version']}"
        files = license_files(p['dir'])
        if not files:
            missing.append(p)
            t = fallback_text(p)
            if t:
                texts.setdefault(t, []).append(f'{label} (standard text of {p["license"]}; no license file in the source)')
        for f in files:
            t = f.read_text(encoding='utf-8', errors='replace').replace('\r\n', '\n').strip()
            texts.setdefault(t, []).append(f'{label} ({f.name})')
    own = (ROOT / 'LICENSE').read_text(encoding='utf-8').strip()
    lines = [
        'Recolign — license / このプログラムのライセンス',
        '',
        own,
        '',
        '=' * 78,
        '',
        'Recolign — third-party software / 使用しているソフトウェアのライセンス',
        '',
        'This program includes the following software. Their license texts follow.',
        'このプログラムには、次のソフトウェアが含まれています。各ライセンスの全文は後に続きます。',
        '',
    ]
    for p in pkgs:
        lines.append(f"{p['name']} {p['version']}  {p['license']}  {p['repository']}".rstrip())
    if missing:
        lines += ['', 'No license file in the package source. The standard text of the license is used below,',
                  'or for BSL-1.0 and MPL-2.0 the full text included for other packages; sources are at the repository:']
        lines += [f"  {p['name']} {p['version']}  {p['license']}  {p['repository']}" for p in missing]
    for t, users in sorted(texts.items(), key=lambda kv: kv[1][0].lower()):
        lines += ['', '=' * 78, 'Used by: ' + ', '.join(users), '=' * 78, '', t]
    a.out.parent.mkdir(parents=True, exist_ok=True)
    a.out.write_text('\n'.join(lines) + '\n', encoding='utf-8')
    print(f'{len(pkgs)} packages, {len(texts)} distinct license texts, {len(missing)} without a license file -> {a.out}'
          f' ({a.out.stat().st_size // 1024} KB)')
    return 0


if __name__ == '__main__':
    sys.exit(main())
