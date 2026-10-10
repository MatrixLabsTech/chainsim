//! The `fork` object and the request and result types of the Adapter trait
//! (design section 5).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::profile::Maturity;

/// Where a fork was taken from: a block (EVM), a slot (Solana) or a
/// checkpoint (Sui).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkPoint {
    pub kind: String,
    pub number: u64,
}

/// Where a fork reads mainnet state: the public default and the variable
/// that overrides it. The variable's value is never shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpstreamInfo {
    pub default: String,
    pub env: String,
    /// True when the variable is set, so the fork reads from a provider.
    pub set: bool,
}

/// What `up` returns and every later step reads.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Fork {
    pub name: String,
    /// The profile name, for example `arc-mainnet`.
    pub chain: String,
    /// The CAIP-2 chain id, for example `eip155:5042`.
    pub chain_id: String,
    pub maturity: Maturity,
    pub fork_point: ForkPoint,
    /// The Docker network the fork is on.
    pub network: String,
    /// The host name of the engine on that network.
    pub alias: String,
    /// In-network URL: reachable from containers on `network`, not from the host.
    pub rpc_url: String,
    pub ws_url: Option<String>,
    /// Set only with `--publish`: a `127.0.0.1` URL for tools on the host.
    pub host_rpc_url: Option<String>,
    /// Unlocked accounts on the engine (EVM).
    pub dev_accounts: Option<Vec<String>>,
    pub upstream: UpstreamInfo,
    /// What differs from the real chain, word for word from the profile.
    pub limits: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpRequest {
    pub name: String,
    pub block: Option<u64>,
    /// Join this existing network instead of creating `chainsim-<name>`.
    pub network: Option<String>,
    pub alias: Option<String>,
    pub publish: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    #[serde(flatten)]
    pub fork: Fork,
    pub head: u64,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Down {
    pub name: String,
    /// False when there was no fork of that name (still exit 0).
    pub removed: bool,
}

/// One balance to set: a token symbol or address, and a decimal amount in
/// whole units (`25` or `0.5`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenAmount {
    pub token: String,
    pub amount: String,
}

#[derive(Debug, Clone, Default)]
pub struct FundRequest {
    pub address: String,
    pub native: Option<String>,
    pub tokens: Vec<TokenAmount>,
}

/// A balance read back after `fund`.
#[derive(Debug, Clone, Serialize)]
pub struct Balance {
    /// `native`, or the token symbol.
    pub asset: String,
    /// The token contract, absent for native.
    pub address: Option<String>,
    pub decimals: u8,
    /// Whole units, as a decimal string.
    pub amount: String,
    /// Base units, as a decimal string.
    pub raw: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Funded {
    pub address: String,
    pub balances: Vec<Balance>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reverted {
    /// The snapshot id the fork went back to. It is consumed.
    pub reverted: String,
    /// A fresh snapshot of the restored state, so loops stay the same on every family.
    pub snapshot: String,
}

#[derive(Debug, Clone)]
pub enum SetKind {
    Code,
    Storage,
    Account,
}

#[derive(Debug, Clone)]
pub struct SetRequest {
    pub kind: SetKind,
    pub target: String,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub enum TimeRequest {
    Advance(u64),
    To(u64),
}

/// One selftest check. `reason` is one of the adapter's fixed sentences, so it
/// can never carry an RPC URL.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub part: &'static str,
    pub ok: bool,
    pub reason: &'static str,
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SelftestReport {
    pub ok: bool,
    pub parts: Vec<Check>,
}
