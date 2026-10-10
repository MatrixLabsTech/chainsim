//! The JSON envelope: stdout carries exactly one of these per command
//! (design section 9).

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, ErrorCode};

/// `{ "ok": true, "data": ... }`
pub fn ok(data: impl Serialize) -> Value {
    json!({ "ok": true, "data": data })
}

/// `{ "ok": false, "error": { "code", "message", "next" }, "data"? }`.
/// `data` is present only when the error carries detail, such as each check
/// of a failed selftest.
pub fn err(error: &Error) -> Value {
    let mut out = json!({
        "ok": false,
        "error": { "code": error.code, "message": error.message, "next": error.next },
    });
    if let Some(data) = &error.data {
        out["data"] = data.clone();
    }
    out
}

/// The envelope of a command result and the process exit code it maps to.
pub fn render<T: Serialize>(result: &crate::Result<T>) -> (Value, i32) {
    match result {
        Ok(data) => (ok(data), 0),
        Err(e) => (err(e), e.code.exit_code()),
    }
}

/// Checks that a value has the envelope shape. Used by tests and by the
/// contract tests.
pub fn is_valid(value: &Value) -> bool {
    let Some(obj) = value.as_object() else { return false };
    match obj.get("ok") {
        Some(Value::Bool(true)) => obj.contains_key("data") && !obj.contains_key("error"),
        Some(Value::Bool(false)) => {
            let Some(e) = obj.get("error").and_then(Value::as_object) else { return false };
            let code_ok = e.get("code").and_then(Value::as_str).is_some_and(|c| ErrorCode::ALL.iter().any(|k| k.as_str() == c));
            code_ok && e.get("message").is_some_and(Value::is_string) && e.contains_key("next")
        }
        _ => false,
    }
}
