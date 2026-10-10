//! Phase 0 acceptance: a fake adapter drives up, selftest and down through the
//! Adapter trait, and every result renders as a valid envelope.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chainsim_core::adapter::selftest_result;
use chainsim_core::envelope;
use chainsim_core::fork::*;
use chainsim_core::profile::{self, Profile};
use chainsim_core::{Adapter, Error, ErrorCode, Result};
use serde_json::{json, Value};

#[derive(Default)]
struct Fake {
    forks: Mutex<BTreeMap<String, Fork>>,
    broken: bool,
}

#[async_trait]
impl Adapter for Fake {
    fn family(&self) -> &'static str {
        "evm"
    }

    fn operations(&self) -> &'static [&'static str] {
        &["up", "down", "status", "selftest"]
    }

    async fn up(&self, profile: &Profile, req: &UpRequest) -> Result<Fork> {
        let mut forks = self.forks.lock().unwrap();
        if let Some(f) = forks.get(&req.name) {
            if f.chain != profile.key || req.block.is_some_and(|b| b != f.fork_point.number) {
                return Err(Error::new(ErrorCode::NameConflict, "taken"));
            }
            return Ok(f.clone());
        }
        let fork = Fork {
            name: req.name.clone(),
            chain: profile.key.clone(),
            chain_id: profile.id.clone(),
            maturity: profile.maturity,
            fork_point: ForkPoint { kind: "block".into(), number: req.block.unwrap_or(100) },
            network: format!("chainsim-{}", req.name),
            alias: req.name.clone(),
            rpc_url: format!("http://{}:{}", req.name, profile.ports.rpc),
            ws_url: None,
            host_rpc_url: None,
            dev_accounts: None,
            limits: profile.limits.clone(),
        };
        forks.insert(req.name.clone(), fork.clone());
        Ok(fork)
    }

    async fn down(&self, name: &str) -> Result<Down> {
        let removed = self.forks.lock().unwrap().remove(name).is_some();
        Ok(Down { name: name.into(), removed })
    }

    async fn status(&self, name: &str) -> Result<Status> {
        let fork = self.forks.lock().unwrap().get(name).cloned().ok_or_else(|| Error::fork_not_found(name))?;
        Ok(Status { fork, head: 100, uptime_secs: 0 })
    }

    async fn fund(&self, _: &str, _: &FundRequest) -> Result<Funded> {
        Err(Error::unsupported("fund", "none"))
    }

    async fn snapshot(&self, _: &str) -> Result<Snapshot> {
        Err(Error::unsupported("snapshot", "none"))
    }

    async fn revert(&self, _: &str, _: &str) -> Result<Reverted> {
        Err(Error::unsupported("revert", "none"))
    }

    async fn call(&self, _: &str, _: &str, _: Value, _: Option<&str>) -> Result<Value> {
        Err(Error::unsupported("call", "none"))
    }

    async fn selftest(&self, name: &str) -> Result<SelftestReport> {
        self.status(name).await?;
        let parts = vec![
            Check { part: "block", ok: true, reason: "the fork answers and mines", ms: 1 },
            Check { part: "native-transfer", ok: !self.broken, reason: "a native transfer confirms", ms: 2 },
        ];
        selftest_result(SelftestReport { ok: parts.iter().all(|c| c.ok), parts })
    }
}

fn arc() -> Profile {
    profile::get("arc-mainnet").unwrap()
}

fn req(name: &str) -> UpRequest {
    UpRequest { name: name.into(), ..Default::default() }
}

#[tokio::test]
async fn fake_adapter_drives_up_selftest_down_through_the_trait() {
    let adapter: Box<dyn Adapter> = Box::new(Fake::default());

    let (up, code) = envelope::render(&adapter.up(&arc(), &req("t")).await);
    assert_eq!(code, 0);
    assert!(envelope::is_valid(&up), "{up}");
    assert_eq!(up["data"]["chain"], "arc-mainnet");
    assert_eq!(up["data"]["forkPoint"], json!({"kind": "block", "number": 100}));
    assert_eq!(up["data"]["rpcUrl"], "http://t:8545");
    assert!(up["data"]["limits"].as_array().is_some_and(|l| !l.is_empty()));
    println!("{up}");

    // Idempotent up: same name and chain, no block, returns the same fork.
    let (again, _) = envelope::render(&adapter.up(&arc(), &req("t")).await);
    assert_eq!(again, up);
    // A different block on a taken name is a conflict: exit 8.
    let conflict = adapter.up(&arc(), &UpRequest { block: Some(5), ..req("t") }).await;
    assert_eq!(envelope::render(&conflict).1, 8);

    let (st, code) = envelope::render(&adapter.selftest("t").await);
    assert_eq!(code, 0);
    assert!(envelope::is_valid(&st));
    assert_eq!(st["data"]["ok"], true);
    for part in st["data"]["parts"].as_array().unwrap() {
        assert!(part["part"].is_string() && part["ok"].is_boolean() && part["reason"].is_string() && part["ms"].is_u64());
    }
    println!("{st}");

    let (down, code) = envelope::render(&adapter.down("t").await);
    assert_eq!(code, 0);
    assert_eq!(down["data"]["removed"], true);
    // down on a missing fork is exit 0.
    let (down, code) = envelope::render(&adapter.down("t").await);
    assert_eq!((code, down["data"]["removed"].clone()), (0, json!(false)));
    println!("{down}");

    // status after down: not found, exit 4.
    let (missing, code) = envelope::render(&adapter.status("t").await);
    assert_eq!(code, 4);
    assert!(envelope::is_valid(&missing));
    assert_eq!(missing["error"]["code"], "fork_not_found");
    assert_eq!(missing["error"]["next"], "chainsim ls");
}

#[tokio::test]
async fn failed_selftest_is_exit_6_with_every_check_in_data() {
    let adapter: Box<dyn Adapter> = Box::new(Fake { broken: true, ..Default::default() });
    adapter.up(&arc(), &req("t")).await.unwrap();
    let (out, code) = envelope::render(&adapter.selftest("t").await);
    assert_eq!(code, 6);
    assert!(envelope::is_valid(&out));
    assert_eq!(out["error"]["code"], "selftest_failed");
    assert_eq!(out["data"]["parts"].as_array().unwrap().len(), 2);
    assert_eq!(out["data"]["parts"][1]["ok"], false);
}

#[tokio::test]
async fn default_operations_are_unsupported_exit_3() {
    let adapter: Box<dyn Adapter> = Box::new(Fake::default());
    let (out, code) = envelope::render(&adapter.mine("t", None).await);
    assert_eq!(code, 3);
    assert_eq!(out["error"]["code"], "unsupported");
    assert!(out["error"]["next"].is_string());
}

#[test]
fn exit_codes_cover_0_to_8() {
    let mut codes: Vec<i32> = ErrorCode::ALL.iter().map(|c| c.exit_code()).collect();
    codes.sort();
    codes.dedup();
    assert_eq!(codes, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    for c in ErrorCode::ALL {
        assert_eq!(serde_json::to_value(c).unwrap(), json!(c.as_str()));
    }
}
