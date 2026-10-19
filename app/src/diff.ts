// Character-level difference of two cell values, for the detail panel.
// Works on code points. The common prefix and suffix are removed first; the
// middle is compared with Myers' O(ND) algorithm and given up (shown as one
// changed block) when it would be too costly.

export interface Segment {
  kind: 'same' | 'removed' | 'added';
  text: string;
}

const MAX_D = 1000;
const MAX_LEN = 20000;

// trace[d] holds v[-d-1 .. d+1] as it was at the start of round d, so memory
// grows with D² instead of D·(N+M).
function myers(a: string[], b: string[]): Segment[] | null {
  const n = a.length;
  const m = b.length;
  const max = n + m;
  const offset = max + 1;
  const v = new Int32Array(2 * max + 3);
  const trace: Int32Array[] = [];
  for (let d = 0; d <= Math.min(max, MAX_D); d++) {
    trace.push(v.slice(offset - d - 1, offset + d + 2));
    for (let k = -d; k <= d; k += 2) {
      let x = k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1]) ? v[offset + k + 1] : v[offset + k - 1] + 1;
      let y = x - k;
      while (x < n && y < m && a[x] === b[y]) {
        x++;
        y++;
      }
      v[offset + k] = x;
      if (x >= n && y >= m) return backtrack(trace, a, b, d);
    }
  }
  return null;
}

function backtrack(trace: Int32Array[], a: string[], b: string[], dEnd: number): Segment[] {
  const out: Segment[] = [];
  let x = a.length;
  let y = b.length;
  const push = (kind: Segment['kind'], ch: string) => {
    const last = out[out.length - 1];
    if (last && last.kind === kind) last.text = ch + last.text;
    else out.push({ kind, text: ch });
  };
  for (let d = dEnd; d > 0; d--) {
    const t = trace[d];
    const at = (k: number) => t[k + d + 1];
    const k = x - y;
    const prevK = k === -d || (k !== d && at(k - 1) < at(k + 1)) ? k + 1 : k - 1;
    const prevX = at(prevK);
    const prevY = prevX - prevK;
    while (x > prevX && y > prevY) {
      push('same', a[x - 1]);
      x--;
      y--;
    }
    if (x === prevX) push('added', b[y - 1]);
    else push('removed', a[x - 1]);
    x = prevX;
    y = prevY;
  }
  while (x > 0 && y > 0) {
    push('same', a[x - 1]);
    x--;
    y--;
  }
  return out.reverse();
}

export function diffChars(before: string, after: string): Segment[] {
  const a = Array.from(before);
  const b = Array.from(after);
  let p = 0;
  while (p < a.length && p < b.length && a[p] === b[p]) p++;
  let s = 0;
  while (s < a.length - p && s < b.length - p && a[a.length - 1 - s] === b[b.length - 1 - s]) s++;
  const head = a.slice(0, p).join('');
  const tail = a.slice(a.length - s).join('');
  const ma = a.slice(p, a.length - s);
  const mb = b.slice(p, b.length - s);
  let middle: Segment[] | null = null;
  if (ma.length + mb.length <= MAX_LEN) middle = myers(ma, mb);
  if (!middle) {
    middle = [];
    if (ma.length) middle.push({ kind: 'removed', text: ma.join('') });
    if (mb.length) middle.push({ kind: 'added', text: mb.join('') });
  }
  const out: Segment[] = [];
  if (head) out.push({ kind: 'same', text: head });
  out.push(...middle);
  if (tail) out.push({ kind: 'same', text: tail });
  return out;
}
