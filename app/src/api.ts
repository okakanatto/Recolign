// Access to the engine. In the desktop app every command goes through Tauri
// IPC; in the development harness through HTTP on 127.0.0.1. Both end in the
// same `cdr-service` commands.

import type { Settings } from './settings';

export class ApiError extends Error {
  constructor(
    message: string,
    readonly detail: unknown = null,
  ) {
    super(message);
  }
}

export interface Backend {
  readonly kind: 'desktop' | 'harness';
  call<T = any>(cmd: string, args?: object): Promise<T>;
  /** Native file dialog. `null` when cancelled. */
  pickFile(): Promise<string | null>;
  /** Native save dialog for an Excel or CSV file. `null` when cancelled. */
  pickSavePath(defaultName: string, kind: 'xlsx' | 'csv'): Promise<string | null>;
  /** Copies a range to the clipboard; returns what was copied. */
  copy(args: object, excludeHistory: boolean): Promise<{ rows: number; cells: number }>;
  loadSettings(): Promise<Partial<Settings>>;
  saveSettings(s: Settings): Promise<void>;
  /** Window title. */
  setTitle(title: string): void;
  /** Whole-window zoom. Returns false if the shell zooms by itself. */
  setZoom(factor: number): Promise<boolean>;
  /** Files dropped on the window; x and y are in CSS pixels. */
  onFileDrop(cb: (paths: string[], x: number, y: number) => void): void;
  /** Files and keys the app was started with. */
  startupArgs(): Promise<{ files: string[]; keys: string[] }>;
}

function toError(e: unknown): ApiError {
  if (e instanceof ApiError) return e;
  if (e && typeof e === 'object' && 'error' in e) {
    const o = e as { error: unknown; detail?: unknown };
    return new ApiError(String(o.error), o.detail ?? null);
  }
  return new ApiError(String(e));
}

class DesktopBackend implements Backend {
  readonly kind = 'desktop' as const;
  private core = import('@tauri-apps/api/core');
  private webview = import('@tauri-apps/api/webview');

  async call<T>(cmd: string, args: object = {}): Promise<T> {
    const { invoke } = await this.core;
    try {
      return await invoke<T>('call', { cmd, args });
    } catch (e) {
      throw toError(e);
    }
  }
  async pickFile(): Promise<string | null> {
    const { invoke } = await this.core;
    return invoke<string | null>('pick_file');
  }
  async pickSavePath(defaultName: string, kind: 'xlsx' | 'csv'): Promise<string | null> {
    const { invoke } = await this.core;
    return invoke<string | null>('save_file', { defaultName, kind });
  }
  async copy(args: object, excludeHistory: boolean) {
    const { invoke } = await this.core;
    try {
      return await invoke<{ rows: number; cells: number }>('copy_to_clipboard', { args, excludeHistory });
    } catch (e) {
      throw toError(e);
    }
  }
  async loadSettings() {
    const { invoke } = await this.core;
    try {
      return (await invoke<Partial<Settings> | null>('load_settings')) ?? {};
    } catch {
      return {};
    }
  }
  async saveSettings(s: Settings) {
    const { invoke } = await this.core;
    await invoke('save_settings', { settings: s }).catch(() => undefined);
  }
  setTitle(title: string) {
    document.title = title;
    this.core.then(({ invoke }) => invoke('set_title', { title }).catch(() => undefined));
  }
  async setZoom(factor: number) {
    const { invoke } = await this.core;
    try {
      await invoke('set_zoom', { factor });
    } catch {
      document.documentElement.style.zoom = String(factor);
    }
    return true;
  }
  async startupArgs() {
    const { invoke } = await this.core;
    try {
      return await invoke<{ files: string[]; keys: string[] }>('startup_args');
    } catch {
      return { files: [], keys: [] };
    }
  }
  onFileDrop(cb: (paths: string[], x: number, y: number) => void) {
    this.webview.then(({ getCurrentWebview }) =>
      getCurrentWebview().onDragDropEvent((e) => {
        if (e.payload.type !== 'drop') return;
        const r = window.devicePixelRatio || 1;
        cb(e.payload.paths, e.payload.position.x / r, e.payload.position.y / r);
      }),
    );
  }
}

class HarnessBackend implements Backend {
  readonly kind = 'harness' as const;
  private token = new URLSearchParams(location.hash.slice(1)).get('t') || '';

  async call<T>(cmd: string, args: object = {}): Promise<T> {
    const r = await fetch(`api/${cmd}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-CDR-Token': this.token },
      body: JSON.stringify(args),
    });
    const j = await r.json();
    if (!r.ok) throw toError(j);
    return j as T;
  }
  async pickFile() {
    return window.prompt('ファイルのパス') || null;
  }
  async pickSavePath(defaultName: string) {
    return window.prompt('書き出し先のパス', defaultName) || null;
  }
  async copy(args: object, _excludeHistory: boolean) {
    const r = await this.call<{ text: string; rows: number; cells: number }>('copy', args);
    (window as any).__lastCopy = r.text;
    await navigator.clipboard?.writeText(r.text).catch(() => undefined);
    return { rows: r.rows, cells: r.cells };
  }
  async loadSettings() {
    try {
      return JSON.parse(localStorage.getItem('cdr-settings') || '{}');
    } catch {
      return {};
    }
  }
  async saveSettings(s: Settings) {
    try {
      localStorage.setItem('cdr-settings', JSON.stringify(s));
    } catch {
      /* private mode */
    }
  }
  setTitle(title: string) {
    document.title = title;
  }
  async setZoom() {
    return false;
  }
  onFileDrop() {}
  /** Tests: `#t=…&open=A.csv&open=B.csv&key=K` stands in for the command line. */
  async startupArgs() {
    const q = new URLSearchParams(location.hash.slice(1));
    return { files: q.getAll('open'), keys: q.getAll('key') };
  }
}

export function createBackend(): Backend {
  return '__TAURI_INTERNALS__' in window ? new DesktopBackend() : new HarnessBackend();
}
