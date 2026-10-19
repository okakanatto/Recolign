// Context menus, popovers and short notices.

import { el } from './util';

export interface MenuItem {
  label: string;
  shortcut?: string;
  title?: string;
  disabled?: boolean;
  action: () => void;
}

let openMenuEl: HTMLElement | null = null;
let restoreFocus: HTMLElement | null = null;

export function closeMenu(): void {
  if (!openMenuEl) return;
  openMenuEl.remove();
  openMenuEl = null;
  restoreFocus?.focus({ preventScroll: true });
  restoreFocus = null;
}

/** Opens a menu at (x, y) in viewport coordinates. `null` items are separators. */
export function openMenu(x: number, y: number, items: (MenuItem | null)[]): void {
  closeMenu();
  restoreFocus = document.activeElement as HTMLElement | null;
  const m = el('div', 'menu');
  m.setAttribute('role', 'menu');
  for (const it of items) {
    if (!it) {
      m.append(el('div', 'sep'));
      continue;
    }
    const b = el('button', 'mi');
    b.type = 'button';
    b.setAttribute('role', 'menuitem');
    b.disabled = !!it.disabled;
    if (it.title) b.title = it.title;
    b.append(el('span', '', it.label));
    if (it.shortcut) b.append(el('span', 'sc', it.shortcut));
    b.addEventListener('click', () => {
      closeMenu();
      it.action();
    });
    m.append(b);
  }
  m.addEventListener('keydown', (e) => {
    const list = [...m.querySelectorAll<HTMLButtonElement>('.mi:not(:disabled)')];
    const i = list.indexOf(document.activeElement as HTMLButtonElement);
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      const n = list.length;
      list[(i + (e.key === 'ArrowDown' ? 1 : n - 1)) % n]?.focus();
    } else if (e.key === 'Escape' || e.key === 'Tab') {
      e.preventDefault();
      closeMenu();
    }
  });
  document.body.append(m);
  const r = m.getBoundingClientRect();
  m.style.left = Math.max(0, Math.min(x, window.innerWidth - r.width - 4)) + 'px';
  m.style.top = Math.max(0, Math.min(y, window.innerHeight - r.height - 4)) + 'px';
  openMenuEl = m;
  m.querySelector<HTMLButtonElement>('.mi:not(:disabled)')?.focus();
}

window.addEventListener('pointerdown', (e) => {
  if (openMenuEl && !openMenuEl.contains(e.target as Node)) closeMenu();
});
window.addEventListener('blur', closeMenu);

/** A panel that opens under a button and closes on outside click or Escape. */
export class Popover {
  readonly panel: HTMLDivElement;
  private onOpen: (() => void) | null = null;
  private outside = (e: PointerEvent) => {
    if (this.panel.hidden) return;
    const t = e.target as Node;
    if (!this.panel.contains(t) && !this.anchor.contains(t)) this.close(false);
  };

  constructor(
    readonly anchor: HTMLButtonElement,
    label: string,
  ) {
    this.panel = el('div', 'popover');
    this.panel.setAttribute('role', 'dialog');
    this.panel.setAttribute('aria-label', label);
    this.panel.hidden = true;
    anchor.setAttribute('aria-haspopup', 'dialog');
    anchor.setAttribute('aria-expanded', 'false');
    anchor.addEventListener('click', () => this.toggle());
    this.panel.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        this.close(true);
      }
    });
    window.addEventListener('pointerdown', this.outside);
    document.body.append(this.panel);
  }

  whenOpened(f: () => void): this {
    this.onOpen = f;
    return this;
  }

  get isOpen(): boolean {
    return !this.panel.hidden;
  }

  toggle(): void {
    if (this.isOpen) this.close(true);
    else this.open();
  }

  open(): void {
    this.onOpen?.();
    this.panel.hidden = false;
    this.anchor.setAttribute('aria-expanded', 'true');
    const a = this.anchor.getBoundingClientRect();
    const r = this.panel.getBoundingClientRect();
    this.panel.style.left = Math.max(4, Math.min(a.left, window.innerWidth - r.width - 4)) + 'px';
    // Below the button, or above it when it does not fit there (a button at
    // the bottom of the window); never past the top.
    const below = a.bottom + 4;
    const fitsBelow = below + r.height <= window.innerHeight - 4;
    this.panel.style.top = (fitsBelow ? below : Math.max(4, a.top - 4 - r.height)) + 'px';
    this.panel.querySelector<HTMLElement>('input, select, button')?.focus();
  }

  close(focusAnchor: boolean): void {
    if (!this.isOpen) return;
    this.panel.hidden = true;
    this.anchor.setAttribute('aria-expanded', 'false');
    if (focusAnchor) this.anchor.focus();
  }

  destroy(): void {
    window.removeEventListener('pointerdown', this.outside);
    this.panel.remove();
  }
}

let toastEl: HTMLDivElement | null = null;
let toastTimer = 0;

/** A short notice at the bottom of the window, optionally with one action (e.g. 元に戻す). */
export function toast(text: string, kind: 'info' | 'error' = 'info', action?: { label: string; run: () => void }): void {
  if (!toastEl) {
    toastEl = el('div', 'toast');
    toastEl.setAttribute('role', 'status');
    toastEl.setAttribute('aria-live', 'polite');
    document.body.append(toastEl);
  }
  const t = toastEl;
  t.replaceChildren(el('span', '', text));
  if (action) {
    const b = el('button', 'tact', action.label);
    b.type = 'button';
    b.addEventListener('click', () => {
      t.hidden = true;
      action.run();
    });
    t.append(b);
  }
  t.className = 'toast ' + kind;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (t.hidden = true), kind === 'error' ? 8000 : action ? 7000 : 4000);
}

/** A modal with a long, scrollable text and a close button (Escape closes it too). */
export function textDialog(title: string, text: string): void {
  infoDialog(title, el('pre', 'mtext', text), 'wide');
}

/** A modal showing `content` with a close button (Escape closes it too). */
export function infoDialog(title: string, content: HTMLElement, size: 'wide' | 'normal' = 'normal'): void {
  closeMenu();
  const back = el('div', 'modalback');
  const box = el('div', size === 'wide' ? 'modal wide' : 'modal');
  box.setAttribute('role', 'dialog');
  box.setAttribute('aria-modal', 'true');
  box.setAttribute('aria-label', title);
  content.tabIndex = 0;
  const close = el('button', '', '閉じる');
  close.type = 'button';
  const actions = el('div', 'mactions');
  actions.append(close);
  box.append(el('h2', '', title), content, actions);
  back.append(box);
  const prev = document.activeElement as HTMLElement | null;
  document.body.append(back);
  content.focus();
  const done = () => {
    back.remove();
    prev?.focus({ preventScroll: true });
  };
  close.addEventListener('click', done);
  back.addEventListener('pointerdown', (e) => {
    if (e.target === back) done();
  });
  box.addEventListener('keydown', (e) => {
    e.stopPropagation();
    if (e.key === 'Escape') {
      e.preventDefault();
      done();
    } else if (e.key === 'Tab') {
      e.preventDefault();
      (document.activeElement === close ? content : close).focus();
    }
  });
}

/** A modal question. Resolves true when confirmed (Enter), false when cancelled (Escape). */
export function confirmDialog(title: string, body: Node[], ok: string, cancel = 'やめる'): Promise<boolean> {
  closeMenu();
  const back = el('div', 'modalback');
  const box = el('div', 'modal');
  box.setAttribute('role', 'dialog');
  box.setAttribute('aria-modal', 'true');
  box.setAttribute('aria-label', title);
  const okB = el('button', 'primary', ok);
  okB.type = 'button';
  const noB = el('button', '', cancel);
  noB.type = 'button';
  const actions = el('div', 'mactions');
  actions.append(noB, okB);
  box.append(el('h2', '', title), ...body, actions);
  back.append(box);
  const prev = document.activeElement as HTMLElement | null;
  document.body.append(back);
  okB.focus();
  return new Promise((resolve) => {
    const done = (v: boolean) => {
      back.remove();
      prev?.focus({ preventScroll: true });
      resolve(v);
    };
    okB.addEventListener('click', () => done(true));
    noB.addEventListener('click', () => done(false));
    back.addEventListener('pointerdown', (e) => {
      if (e.target === back) done(false);
    });
    box.addEventListener('keydown', (e) => {
      e.stopPropagation();
      if (e.key === 'Escape') {
        e.preventDefault();
        done(false);
      } else if (e.key === 'Tab') {
        // Keep the focus inside the dialog.
        e.preventDefault();
        (document.activeElement === okB ? noB : okB).focus();
      }
    });
  });
}
