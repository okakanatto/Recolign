// Small DOM and text helpers. Values from CSV files are always set with
// textContent, never as HTML.

export function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

export function button(text: string, onClick: (e: MouseEvent) => void, opts: { title?: string; cls?: string } = {}): HTMLButtonElement {
  const b = el('button', opts.cls, text);
  b.type = 'button';
  if (opts.title) b.title = opts.title;
  b.addEventListener('click', onClick);
  return b;
}

export const fmt = (n: number): string => Number(n).toLocaleString('ja-JP');

export function mib(bytes: number): string {
  // A no-break space keeps the number and the unit on one line.
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))}\u00a0KB`;
  return `${(bytes / 1048576).toFixed(bytes < 100 * 1048576 ? 1 : 0)}\u00a0MB`;
}

/** Memory sizes: GB with one decimal from 1 GB. */
export function gib(bytes: number): string {
  return bytes < 1024 ** 3 ? mib(bytes) : `${(bytes / 1024 ** 3).toFixed(1)}\u00a0GB`;
}

/** One-line rendering for grid cells: line breaks and tabs become visible marks. */
export function cellText(v: string, showSpaces: boolean): string {
  let s = v.replace(/\r\n|\r|\n/g, '⏎').replace(/\t/g, '→');
  if (showSpaces) s = s.replace(/ /g, '·').replace(/　/g, '□');
  return s;
}

/** Multi-line rendering for the detail panel. */
export function blockText(v: string, showSpaces: boolean): string {
  if (!showSpaces) return v;
  return v.replace(/ /g, '·').replace(/　/g, '□').replace(/\t/g, '→').replace(/\r/g, '␍').replace(/\n/g, '␊\n');
}

/** Measures texts in the page's own font with one layout pass. */
export function measureTexts(texts: string[], bold: boolean): number[] {
  const box = el('div', 'measure');
  if (bold) box.style.fontWeight = '600';
  const spans = texts.map((t) => box.appendChild(el('span', '', t)));
  document.body.append(box);
  const widths = spans.map((sp) => Math.ceil(sp.getBoundingClientRect().width) + 2);
  box.remove();
  return widths;
}

export function clamp(v: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, v));
}
