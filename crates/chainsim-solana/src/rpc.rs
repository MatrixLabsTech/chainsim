//! JSON-RPC to the Surfpool engine and to the upstream, through the fork's
//! control sidecar.

use std::time::Duration;

use chainsim_core::control::{Control, Target};
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
        self.control.rpc(&self.url, method, params, CALL_TIMEOUT).await
    }

    /// One call to the engine with its own timeout, for readiness probes.
    pub async fn call_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.control.rpc(&self.url, method, params, timeout).await
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
