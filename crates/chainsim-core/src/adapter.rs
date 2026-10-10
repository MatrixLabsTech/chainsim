//! The Adapter trait every chain family implements (design section 5). After
//! phase 0 it changes only by agreement of both streams through pm.chainsim.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::fork::*;
use crate::profile::Profile;

/// An operation an engine lacks returns `unsupported` with the substitute in
/// `next`, and never fakes success.
#[async_trait]
pub trait Adapter: Send + Sync {
    /// `evm`, `solana` or `sui`: the profile `family` this adapter serves.
    fn family(&self) -> &'static str;

    /// The operations this adapter supports, for `describe`.
    fn operations(&self) -> &'static [&'static str];

    /// Start the engine and its control sidecar, wait for readiness, install
    /// the profile overrides, run the warm-up, and return the fork.
    async fn up(&self, profile: &Profile, req: &UpRequest) -> Result<Fork>;

    /// Remove the engine, the sidecar, the fork volume, and the network if
    /// chainsim created it. A missing fork is not an error.
    async fn down(&self, name: &str) -> Result<Down>;

    async fn status(&self, name: &str) -> Result<Status>;

    /// Set each balance to the asked amount, then read every balance back.
    async fn fund(&self, name: &str, req: &FundRequest) -> Result<Funded>;

    async fn snapshot(&self, name: &str) -> Result<Snapshot>;

    /// Consume snapshot `id` and return a fresh snapshot of the restored state.
    async fn revert(&self, name: &str, id: &str) -> Result<Reverted>;

    async fn set(&self, _name: &str, _req: &SetRequest) -> Result<Value> {
        Err(Error::unsupported("set", "chainsim call <name> <method> <params-json>"))
    }

    async fn time(&self, _name: &str, _req: &TimeRequest) -> Result<Value> {
        Err(Error::unsupported("time", "chainsim call <name> <method> <params-json>"))
    }

    async fn mine(&self, _name: &str, _blocks: Option<u64>) -> Result<Value> {
        Err(Error::unsupported("mine", "chainsim call <name> <method> <params-json>"))
    }

    /// One raw RPC call through the control path. On EVM, `from` impersonates
    /// that address for an `eth_sendTransaction`.
    async fn call(&self, name: &str, method: &str, params: Value, from: Option<&str>) -> Result<Value>;

    /// Chain-level checks. Returns the report on success; on any failed check
    /// returns `selftest_failed` with the report in `data`.
    async fn selftest(&self, name: &str) -> Result<SelftestReport>;
}

/// Turns a report into the contract's result: exit 0 when every check passed,
/// exit 6 with every check in `data` otherwise.
pub fn selftest_result(report: SelftestReport) -> Result<SelftestReport> {
    if report.ok {
        return Ok(report);
    }
    let failed: Vec<_> = report.parts.iter().filter(|c| !c.ok).map(|c| c.part).collect();
    Err(Error::new(crate::ErrorCode::SelftestFailed, format!("selftest failed: {}", failed.join(", ")))
        .next("check each failed part in data; chainsim down <name> then chainsim up <chain> for a fresh fork")
        .data(serde_json::to_value(&report).expect("report serializes")))
}
