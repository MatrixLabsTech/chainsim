//! JSON-RPC to the Surfpool engine and to the upstream, through the fork's
//! control sidecar.

use std::time::Duration;

use chainsim_core::control::{classify_rpc_error, Control, Target};
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

/// The SPL Token program.
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// The Token-2022 program.
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// One fork's control path and engine URL.
pub struct Rpc<'a> {
    pub control: Control<'a>,
    pub url: Target,
}

impl Rpc<'_> {
    /// One call to the engine.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_timeout(method, params, CALL_TIMEOUT).await
    }

    /// One call to the engine with its own timeout, for readiness probes.
    pub async fn call_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.control.rpc(&self.url, method, params, timeout).await.map_err(|e| self.engine_error(method, e))
    }

    /// Removes a provider URL from the error data the engine sent, then
    /// gives an upstream failure its own code.
    fn engine_error(&self, method: &str, mut e: Error) -> Error {
        let Some(data) = e.data.as_ref().and_then(|d| d.get("rpcData")).cloned() else { return e };
        let data = match data {
            Value::String(s) => Value::String(self.control.scrub(&s)),
            other => serde_json::from_str(&self.control.scrub_secret(&other.to_string())).unwrap_or(Value::Null),
        };
        e.data = Some(json!({ "rpcData": data }));
        reclassify(method, e, self.control.env.as_deref())
    }

    /// One call to the upstream RPC.
    pub async fn upstream(&self, method: &str, params: Value) -> Result<Value> {
        self.control.rpc(&Target::Upstream, method, params, CALL_TIMEOUT).await
    }

    /// The engine's slot at processed commitment. Reads at the default
    /// (finalized) commitment trail it by about 31 slots.
    pub async fn slot(&self) -> Result<u64> {
        let v = self.call("getSlot", json!([processed()])).await?;
        v.as_u64().ok_or_else(|| unexpected("getSlot", &v))
    }

    /// An account as the engine has it now, parsed where the engine knows the
    /// layout, or None when it does not exist.
    pub async fn account(&self, address: &str) -> Result<Option<Value>> {
        let v = self.call("getAccountInfo", json!([address, { "encoding": "jsonParsed", "commitment": "processed" }])).await?;
        Ok(v.get("value").filter(|a| !a.is_null()).cloned())
    }

    /// A native balance in lamports.
    pub async fn balance(&self, address: &str) -> Result<u64> {
        let v = self.call("getBalance", json!([address, processed()])).await?;
        v["value"].as_u64().ok_or_else(|| unexpected("getBalance", &v))
    }

    /// The owner's token accounts for one mint, parsed.
    pub async fn token_accounts(&self, owner: &str, mint: &str) -> Result<Vec<Value>> {
        let v = self.call("getTokenAccountsByOwner", json!([owner, { "mint": mint }, { "encoding": "jsonParsed", "commitment": "processed" }])).await?;
        v["value"].as_array().cloned().ok_or_else(|| unexpected("getTokenAccountsByOwner", &v))
    }
}

/// The commitment config for reads that must see a write at once.
pub fn processed() -> Value {
    json!({ "commitment": "processed" })
}

fn unexpected(method: &str, v: &Value) -> Error {
    Error::new(ErrorCode::RpcError, format!("{method} answered {v}"))
}

/// The engine answers a failed upstream read with "Internal error" and the
/// cause as text in the error data, so the cause decides the code: a
/// throttled or unreachable upstream is exit 5, anything else stays an RPC error.
pub fn reclassify(method: &str, e: Error, env: Option<&str>) -> Error {
    if e.code != ErrorCode::RpcError {
        return e;
    }
    match e.data.as_ref().and_then(|d| d["rpcData"].as_str()) {
        Some(cause) => {
            let by_cause = classify_rpc_error(method, cause, None, env);
            if by_cause.code == ErrorCode::RpcError {
                e
            } else {
                by_cause
            }
        }
        None => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn internal(data: Value) -> Error {
        Error::new(ErrorCode::RpcError, "m: Internal error (-32603)").data(json!({ "rpcData": data }))
    }

    #[test]
    fn an_upstream_failure_behind_an_internal_error_is_exit_5() {
        let env = Some("CHAINSIM_SOLANA_MAINNET_RPC");
        let throttled = reclassify("m", internal(json!("Failed to get largest token accounts for mint X: HTTP status client error (429 Too Many Requests)")), env);
        assert_eq!((throttled.code, throttled.code.exit_code()), (ErrorCode::RateLimited, 5));
        assert!(throttled.next.as_deref().is_some_and(|n| n.contains("CHAINSIM_SOLANA_MAINNET_RPC")));
        // The engine's own refusals stay RPC errors with their data.
        let past = reclassify("m", internal(json!("Cannot travel to past slot: target=1, current=2")), env);
        assert_eq!(past.code, ErrorCode::RpcError);
        assert_eq!(past.data, Some(json!({ "rpcData": "Cannot travel to past slot: target=1, current=2" })));
        let structured = reclassify("m", internal(json!({ "err": "AccountNotFound", "logs": [] })), env);
        assert_eq!(structured.code, ErrorCode::RpcError);
        let plain = reclassify("m", Error::new(ErrorCode::RpcError, "m: Method not found (-32601)"), env);
        assert!(plain.code == ErrorCode::RpcError && plain.data.is_none());
    }
}
