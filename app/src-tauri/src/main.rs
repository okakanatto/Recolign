//! Recolign desktop app. A thin Tauri shell around `cdr-service`:
//! the page calls `call` with the same commands the development harness
//! serves over HTTP, plus a few things only the desktop can do (file dialog,
//! clipboard, settings file, zoom).
//!
//! Built as a single portable exe (`tauri build --no-bundle`); it needs the
//! WebView2 runtime that Windows 10/11 normally include.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cdr_service::Service;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, RunEvent, State, WebviewWindow, WindowEvent};
use tauri_plugin_dialog::DialogExt;

fn error(msg: impl Into<String>) -> Value {
    json!({ "error": msg.into() })
}

/// Runs one engine command off the UI thread.
#[tauri::command]
async fn call(service: State<'_, Arc<Service>>, cmd: String, args: Value) -> Result<Value, Value> {
    let service = service.inner().clone();
    tauri::async_runtime::spawn_blocking(move || service.handle(&cmd, &args))
        .await
        .map_err(|e| error(e.to_string()))?
        .map_err(|e| e.body)
}

/// Copies a range as tab-separated text. The text goes to the clipboard
/// directly, so a large copy does not pass through the page. With
/// `exclude_history`, Windows keeps it out of the clipboard history (Win+V)
/// and the cloud clipboard.
#[tauri::command]
async fn copy_to_clipboard(
    service: State<'_, Arc<Service>>,
    args: Value,
    exclude_history: bool,
) -> Result<Value, Value> {
    let service = service.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (text, rows, cells) = service.copy(&args).map_err(|e| e.body)?;
        arboard::Clipboard::new()
            .and_then(|mut c| set_clipboard_text(&mut c, text, exclude_history))
            .map_err(|e| error(format!("クリップボードに書き込めませんでした（{e}）。もう一度コピーしてください。")))?;
        Ok(json!({ "rows": rows, "cells": cells }))
    })
    .await
    .map_err(|e| error(e.to_string()))?
}

#[cfg(windows)]
fn set_clipboard_text(c: &mut arboard::Clipboard, text: String, exclude_history: bool) -> Result<(), arboard::Error> {
    use arboard::SetExtWindows;
    if exclude_history {
        c.set().exclude_from_history().exclude_from_cloud().text(text)
    } else {
        c.set_text(text)
    }
}

#[cfg(not(windows))]
fn set_clipboard_text(c: &mut arboard::Clipboard, text: String, _exclude_history: bool) -> Result<(), arboard::Error> {
    c.set_text(text)
}

/// The system file dialog. `None` when cancelled.
#[tauri::command]
async fn pick_file(window: WebviewWindow) -> Option<String> {
    let w = window.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        w.dialog()
            .file()
            .set_title("比較するファイルを選ぶ")
            .add_filter("CSV・TSV・テキスト", &["csv", "tsv", "txt", "dat", "tab", "psv", "prn"])
            .add_filter("すべてのファイル", &["*"])
            .set_parent(&w)
            .blocking_pick_file()
    })
    .await
    .ok()
    .flatten()?;
    picked.into_path().ok().map(|p| p.to_string_lossy().into_owned())
}

/// The system save dialog for an Excel or CSV file. `None` when cancelled.
#[tauri::command]
async fn save_file(window: WebviewWindow, default_name: String, kind: String) -> Option<String> {
    let w = window.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let dialog = w.dialog().file().set_title("書き出し先を選ぶ").set_file_name(default_name);
        let dialog = if kind == "xlsx" {
            dialog.add_filter("Excel ブック", &["xlsx"])
        } else {
            dialog.add_filter("CSV", &["csv"])
        };
        dialog.set_parent(&w).blocking_save_file()
    })
    .await
    .ok()
    .flatten()?;
    picked.into_path().ok().map(|p| p.to_string_lossy().into_owned())
}

/// The window title: the files being compared.
#[tauri::command]
fn set_title(window: WebviewWindow, title: String) -> Result<(), String> {
    window.set_title(&title).map_err(|e| e.to_string())
}

/// Preferences (layout, zoom, export choices) are kept next to the exe so
/// they travel with a portable copy, or in the user's profile when that
/// folder is not writable.
fn settings_files(app: &AppHandle) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        v.push(dir.join("Recolign.settings.json"));
    }
    if let Ok(dir) = app.path().app_config_dir() {
        v.push(dir.join("settings.json"));
    }
    v
}

/// Settings kept in the user's profile only (as JSON pointers), not in the
/// preferences file next to the exe, which anyone who can write to that
/// folder can change and every user of a shared copy shares:
/// - what names files and columns (recent comparisons, columns left out of
///   the change patterns), so a copy of the exe on a shared folder or a USB
///   drive does not carry or mix one person's file and column names;
/// - the settings that protect the data, so a shared copy cannot turn them
///   off for the next person. The second value is the one that protects:
///   from a preferences file, only that value is taken, except once from a
///   profile of an earlier version (which saved them there).
const PRIVATE_KEYS: [(&str, Option<bool>); 5] = [
    ("/recent", None),
    ("/groupSkip", None),
    ("/keepHistory", Some(false)),
    ("/clipboardExclude", Some(true)),
    ("/exportOptions/protectFormulas", Some(true)),
];

fn history_file(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("history.json"))
}

fn read_json(p: &Path) -> Option<Value> {
    std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str(&t).ok())
}

/// Removes and returns the value at the JSON pointer `key`.
fn take(v: &mut Value, key: &str) -> Option<Value> {
    let (parent, name) = key.rsplit_once('/')?;
    v.pointer_mut(parent)?.as_object_mut()?.remove(name)
}

/// Puts `value` at the JSON pointer `key`, adding the objects on the way.
fn put(v: &mut Value, key: &str, value: Value) {
    let mut names = key.split('/').skip(1).peekable();
    let mut cur = v;
    while let Some(name) = names.next() {
        let Some(obj) = cur.as_object_mut() else { return };
        if names.peek().is_none() {
            obj.insert(name.to_string(), value);
            return;
        }
        cur = obj.entry(name).or_insert_with(|| json!({}));
    }
}

/// The preferences, with the private settings from the user's profile:
/// names in a preferences file are never used, so a planted recent
/// comparison cannot lead to a file on another computer, and a protecting
/// setting turned off there stays on for someone new to this copy.
#[tauri::command]
fn load_settings(app: AppHandle) -> Option<Value> {
    let mut s = settings_files(&app).iter().find_map(|p| read_json(p)).unwrap_or_else(|| json!({}));
    let h = history_file(&app).and_then(|p| read_json(&p));
    // A profile of an earlier version has none of the protecting settings:
    // its user had them in the preferences file, and keeps them as they were.
    let earlier = h.as_ref().is_some_and(|h| PRIVATE_KEYS.iter().all(|(k, p)| p.is_none() || h.pointer(k).is_none()));
    for (k, protects) in PRIVATE_KEYS {
        let shared = take(&mut s, k).filter(|v| protects.is_some_and(|b| earlier || *v == Value::Bool(b)));
        if let Some(v) = h.as_ref().and_then(|h| h.pointer(k)).cloned().or(shared) {
            put(&mut s, k, v);
        }
    }
    Some(s)
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Value) -> Result<(), String> {
    let mut prefs = settings;
    let mut private = json!({});
    for (k, _) in PRIVATE_KEYS {
        if let Some(v) = take(&mut prefs, k) {
            put(&mut private, k, v);
        }
    }
    if let Some(path) = history_file(&app) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let text = serde_json::to_string_pretty(&private).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let text = serde_json::to_string_pretty(&prefs).map_err(|e| e.to_string())?;
    let mut last = String::from("設定の保存場所がありません。");
    for path in settings_files(&app) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        match std::fs::write(&path, &text) {
            Ok(()) => return Ok(()),
            Err(e) => last = format!("{}: {e}", path.display()),
        }
    }
    Err(last)
}

/// Files and keys the app was started with:
/// `Recolign.exe [LEFT.csv [RIGHT.csv]] [--key NAME ...]`. Explorer
/// passes the files when they are dropped on the exe or sent with 送る.
#[tauri::command]
fn startup_args() -> Value {
    let (mut files, mut keys) = (Vec::new(), Vec::new());
    let mut it = std::env::args_os().skip(1);
    while let Some(a) = it.next() {
        if a == "--key" {
            if let Some(k) = it.next() {
                keys.push(k.to_string_lossy().into_owned());
            }
        } else {
            files.push(a.to_string_lossy().into_owned());
        }
    }
    json!({ "files": files, "keys": keys })
}

/// Zooms the whole page, like a browser's zoom.
#[tauri::command]
fn set_zoom(window: WebviewWindow, factor: f64) -> Result<(), String> {
    window.set_zoom(factor.clamp(0.25, 5.0)).map_err(|e| e.to_string())
}

fn main() {
    #[cfg(windows)]
    load_system_libraries();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Bugs are written to a log in the user's profile (no data values).
            if let Ok(dir) = app.path().app_local_data_dir() {
                cdr_service::crash::install(dir.join("logs"));
            }
            // Review states are kept in the user's profile, under the
            // comparison ID (both files' SHA-256 and the conditions).
            let service = match app.path().app_config_dir() {
                Ok(dir) => Service::with_data_dir(dir.join("reviews")),
                Err(_) => Service::new(),
            };
            app.manage(Arc::new(service));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            call,
            copy_to_clipboard,
            pick_file,
            save_file,
            set_title,
            load_settings,
            save_settings,
            set_zoom,
            startup_args
        ])
        .build(tauri::generate_context!());
    match app {
        Ok(app) => app.run(|handle, event| match event {
            RunEvent::WindowEvent { event: WindowEvent::CloseRequested { api, .. }, .. } => {
                // Write review states changed in the last moments. If they
                // cannot be written, closing would lose them: ask first.
                if let Err(msg) = handle.state::<Arc<Service>>().flush() {
                    let q = format!("{msg}\n\nこのまま閉じると、保存していない確認状況は失われます。閉じますか？");
                    if !ask_yes_no(&q) {
                        api.prevent_close();
                    }
                }
            }
            RunEvent::Exit => {
                handle.state::<Arc<Service>>().flush().ok();
            }
            _ => {}
        }),
        Err(e) => {
            // Most often: the WebView2 runtime is missing or blocked.
            let msg = format!(
                "画面を表示できませんでした。\n\n{e}\n\n\
                 Microsoft Edge WebView2 ランタイムが必要です。Windows 10/11 には通常含まれています。\
                 見つからない場合は、システム管理者にWebView2ランタイムの導入を依頼してください。"
            );
            show_startup_error(&msg);
            std::process::exit(1);
        }
    }
}

/// DLLs loaded by name while the app runs, which `/DEPENDENTLOADFLAG`
/// (build.rs) does not cover: the standard library loads dbghelp.dll to
/// write where a bug happened, and the window code loads uxtheme.dll. Loaded
/// here from System32 first, the later loads by name use these, never a DLL
/// of the same name next to the exe (in the Downloads folder, for example).
#[cfg(windows)]
fn load_system_libraries() {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut core::ffi::c_void, flags: u32) -> *mut core::ffi::c_void;
    }
    const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x800;
    for name in ["dbghelp.dll", "uxtheme.dll"] {
        let wide: Vec<u16> = std::ffi::OsStr::new(name).encode_wide().chain(Some(0)).collect();
        // Kept loaded for the life of the process; a failure leaves the
        // later load to fail the same way.
        unsafe {
            LoadLibraryExW(wide.as_ptr(), core::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        }
    }
}

/// A Yes/No question in a system message box. Yes when not on Windows.
fn ask_yes_no(msg: &str) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide = |s: &str| std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect::<Vec<u16>>();
        let (text, title) = (wide(msg), wide("Recolign"));
        #[link(name = "user32")]
        extern "system" {
            fn MessageBoxW(hwnd: *mut core::ffi::c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
        }
        // MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND | MB_TOPMOST; IDYES = 6
        unsafe {
            MessageBoxW(core::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x4 | 0x30 | 0x100 | 0x10000 | 0x40000)
                == 6
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("{msg}");
        true
    }
}

/// A message box without a running app (the webview could not be created).
fn show_startup_error(msg: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide = |s: &str| std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect::<Vec<u16>>();
        let (text, title) = (wide(msg), wide("Recolign"));
        #[link(name = "user32")]
        extern "system" {
            fn MessageBoxW(hwnd: *mut core::ffi::c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
        }
        // MB_OK | MB_ICONERROR
        unsafe {
            MessageBoxW(core::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x10);
        }
    }
    #[cfg(not(windows))]
    eprintln!("{msg}");
}
