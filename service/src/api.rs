//! The shape of a command's reply, and small helpers every command uses.

use serde::Deserialize;
use serde_json::{json, Value};

/// A failed command: the message is shown to the user as is.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub body: Value,
}

pub(crate) fn bad(msg: impl Into<String>) -> ApiError {
    ApiError { status: 400, body: json!({ "error": msg.into() }) }
}

/// A bug stopped the command (see `crash`).
pub(crate) fn internal(msg: impl Into<String>) -> ApiError {
    ApiError { status: 500, body: json!({ "error": msg.into(), "code": "internal" }) }
}

pub(crate) fn conflict(msg: impl Into<String>) -> ApiError {
    ApiError { status: 409, body: json!({ "error": msg.into() }) }
}

pub type Reply = Result<Value, ApiError>;

/// Reads a command's JSON arguments.
pub(crate) fn parse<T: for<'de> Deserialize<'de>>(args: &Value) -> Result<T, ApiError> {
    T::deserialize(args).map_err(|e| bad(format!("要求の形式が正しくありません（{e}）。")))
}

/// Milliseconds with microsecond precision, for timings in replies.
pub(crate) fn ms(d: std::time::Duration) -> f64 {
    (d.as_secs_f64() * 1e6).round() / 1e3
}

pub(crate) fn default_count() -> usize {
    50
}

pub(crate) fn yes() -> bool {
    true
}
