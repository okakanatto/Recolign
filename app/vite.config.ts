import { defineConfig } from 'vite';

// The same build is loaded by the desktop app (Tauri) and by the development
// harness, so paths are relative and nothing is inlined into the HTML.
export default defineConfig({
  base: './',
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { outDir: 'dist', target: 'es2022', modulePreload: { polyfill: false }, assetsInlineLimit: 0 },
});
