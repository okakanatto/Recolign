//! Application commands shared by the desktop app (Tauri) and the
//! development harness (HTTP on 127.0.0.1). Each command takes JSON and
//! returns JSON, so both shells are thin wrappers around `Service::handle`.
//!
//! The commands, by file:
//!
//! - `comparison`: preview, memory_check, compare, status, cancel, reset
//! - `views`: view, rows, next, edge, locate, detail, find, find_text, patterns, copy
//! - `export`: export
//! - `groups`: groups
//! - `review`: review_summary, review_target, review_set, review_undo,
//!   review_redo, next_review, review_save, review_store
//!
//! `state` holds what the service keeps between commands, `api` the reply
//! type and helpers, and `crash` what happens when the program has a bug.

mod api;
mod comparison;
pub mod crash;
mod export;
mod groups;
mod review;
mod state;
mod views;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use cdr_engine::pattern::Patterns;
use cdr_engine::{CancelToken, Progress};
use serde_json::{json, Value};

pub use api::{ApiError, Reply};
use state::{Current, Phase, Views};

pub struct Service {
    current: Mutex<Current>,
    generation: AtomicU64,
    phase: RwLock<Phase>,
    patterns: RwLock<Option<Arc<Patterns>>>,
    views: Mutex<Views>,
    /// Where review states are saved; `None` keeps them in memory only.
    data_dir: Option<PathBuf>,
    saver: std::sync::OnceLock<()>,
    /// One write of the review states at a time; holds when the last one failed.
    save: Mutex<Option<Instant>>,
    next_target: AtomicU64,
    /// When the memory left falls below `memory_floor` while a comparison
    /// has grown by at least `memory_growth`, the comparison is stopped.
    memory_floor: AtomicU64,
    memory_growth: AtomicU64,
    /// Set when the change patterns could not be aggregated (a bug).
    patterns_error: RwLock<Option<String>>,
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}

impl Service {
    pub fn new() -> Self {
        Service {
            current: Mutex::new(Current {
                generation: 0,
                progress: Arc::new(Progress::default()),
                cancel: CancelToken::new(),
                started: Instant::now(),
            }),
            generation: AtomicU64::new(0),
            phase: RwLock::new(Phase::Idle),
            patterns: RwLock::new(None),
            views: Mutex::new(Views::default()),
            data_dir: None,
            saver: std::sync::OnceLock::new(),
            save: Mutex::new(None),
            next_target: AtomicU64::new(1),
            memory_floor: AtomicU64::new(256 << 20),
            memory_growth: AtomicU64::new(64 << 20),
            patterns_error: RwLock::new(None),
        }
    }

    /// A service that saves review states under `dir`.
    pub fn with_data_dir(dir: PathBuf) -> Self {
        Service { data_dir: Some(dir), ..Self::new() }
    }

    /// Stops a comparison when the memory left falls below `floor` bytes
    /// after it has grown by `growth` bytes (defaults: 256 MB and 64 MB, so
    /// a machine that is merely busy does not stop small comparisons).
    pub fn set_memory_guard(&self, floor: u64, growth: u64) {
        self.memory_floor.store(floor, Ordering::SeqCst);
        self.memory_growth.store(growth, Ordering::SeqCst);
    }

    /// Runs one command. `args` is the JSON object of the request. A bug
    /// in a command (a panic) becomes an error reply naming the crash log.
    pub fn handle(self: &Arc<Self>, cmd: &str, args: &Value) -> Reply {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dispatch(cmd, args)))
            .unwrap_or_else(|_| Err(api::internal(crash::message("操作"))))
    }

    fn dispatch(self: &Arc<Self>, cmd: &str, args: &Value) -> Reply {
        match cmd {
            "app_info" => Ok(json!({ "version": crash::version(), "crash_log": crash::log_file() })),
            "log_error" => {
                crash::log_screen_error(args["text"].as_str().unwrap_or(""));
                Ok(json!({ "logged": crash::log_file().is_some(), "crash_log": crash::log_file() }))
            }
            // Comparing
            "preview" => self.preview_file(args),
            "memory_check" => self.memory_check_cmd(args),
            "compare" => {
                self.start(api::parse(args)?);
                Ok(json!({ "started": true }))
            }
            "status" => Ok(self.status()),
            "cancel" => {
                self.cancel();
                Ok(json!({ "cancelled": true }))
            }
            "reset" => {
                self.reset();
                Ok(json!({ "reset": true }))
            }
            // Reading the result
            "view" => self.view(args),
            "rows" => self.rows(args),
            "next" => self.next(args),
            "edge" => self.edge(args),
            "locate" => self.locate(args),
            "detail" => self.detail(args),
            "find" => self.find(args),
            "find_text" => self.find_text(args),
            "patterns" => self.patterns(args),
            "groups" => self.groups_page(args),
            "copy" => {
                let (text, rows, cells) = self.copy(args)?;
                Ok(json!({ "text": text, "rows": rows, "cells": cells }))
            }
            "export" => self.export(args),
            // Review states
            "review_summary" => self.review_summary_cmd(),
            "review_target" => self.review_target(args),
            "review_set" => self.review_set(args),
            "review_undo" => self.review_undo_redo(false),
            "review_redo" => self.review_undo_redo(true),
            "next_review" => self.next_review(args),
            "review_save" => self.review_save(),
            "review_store" => self.review_store_cmd(args),
            #[cfg(test)]
            "panic_for_test" => panic!("a bug in a command"),
            _ => Err(ApiError { status: 404, body: json!({ "error": format!("不明な操作 {cmd}") }) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bug_in_a_command_is_an_error_reply() {
        let s = Arc::new(Service::new());
        let e = s.handle("panic_for_test", &json!({})).unwrap_err();
        assert_eq!((e.status, e.body["code"].as_str()), (500, Some("internal")));
        assert!(e.body["error"].as_str().unwrap().contains("内部エラー"));
        // The service still answers.
        assert!(s.handle("status", &json!({})).is_ok());
    }

    #[test]
    fn a_bug_in_the_comparison_thread_ends_the_wait() {
        let s = Service::new();
        *s.phase.write().unwrap() = Phase::Running;
        s.after_crash(s.generation.load(Ordering::SeqCst));
        let st = s.status();
        assert_eq!(st["phase"]["state"], "failed");
        assert_eq!(st["phase"]["error"]["code"], "internal");
    }

    #[test]
    fn the_version_names_the_build() {
        let v = crash::version();
        assert!(v.starts_with(env!("CARGO_PKG_VERSION")) && v.contains('('), "{v}");
    }
}
