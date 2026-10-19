#!/usr/bin/env python3
"""Draws the app's icons from their SVG sources (app/src-tauri/icons/icon.svg,
and icon-small.svg for 16 to 40 pixels, where the full drawing blurs) with
Chromium, and writes the PNG files and the Windows icon (.ico) Tauri uses.

    python3 tools/make_icons.py

Needs Playwright and Chromium, as the UI tests do.
"""

import struct
import sys
import tempfile
from pathlib import Path

from playwright.sync_api import sync_playwright

sys.path.insert(0, str(Path(__file__).resolve().parent))
from ui_test import CHROMIUM  # noqa: E402

ICONS = Path(__file__).resolve().parent.parent / 'app' / 'src-tauri' / 'icons'
SMALL = 40  # up to this size, icon-small.svg
ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
PNGS = {'32x32.png': 32, '128x128.png': 128, '128x128@2x.png': 256, 'icon.png': 512}


def draw(page, size: int) -> bytes:
    svg = (ICONS / ('icon-small.svg' if size <= SMALL else 'icon.svg')).read_text(encoding='utf-8')
    svg = svg.replace('<svg ', f'<svg width="{size}" height="{size}" ', 1)
    page.set_content(f'<html><body style="margin:0;background:transparent">{svg}</body></html>')
    return page.locator('svg').screenshot(omit_background=True)


def ico(images: dict[int, bytes]) -> bytes:
    """A .ico holding each size as PNG (Windows Vista and later read these)."""
    sizes = sorted(images)
    head = struct.pack('<HHH', 0, 1, len(sizes))
    entries, data = b'', b''
    offset = 6 + 16 * len(sizes)
    for s in sizes:
        png = images[s]
        side = 0 if s >= 256 else s
        entries += struct.pack('<BBBBHHII', side, side, 0, 0, 1, 32, len(png), offset + len(data))
        data += png
    return head + entries + data


def main() -> int:
    with sync_playwright() as pw, tempfile.TemporaryDirectory():
        browser = pw.chromium.launch(executable_path=CHROMIUM if Path(CHROMIUM).exists() else None, args=['--no-sandbox'])
        page = browser.new_page(viewport={'width': 600, 'height': 600}, device_scale_factor=1)
        for name, size in PNGS.items():
            (ICONS / name).write_bytes(draw(page, size))
        (ICONS / 'icon.ico').write_bytes(ico({s: draw(page, s) for s in ICO_SIZES}))
        browser.close()
    print(f'{", ".join(PNGS)} and icon.ico ({", ".join(map(str, ICO_SIZES))}) -> {ICONS}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
