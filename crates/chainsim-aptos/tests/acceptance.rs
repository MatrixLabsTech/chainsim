//! Acceptance tests for aptos-mainnet (design addendum 2026-11-01). They
//! start a real simulation session on Aptos Mainnet in Docker, so they are
//! ignored by default:
//!
//!     cargo test -p chainsim-aptos --test acceptance -- --ignored --test-threads=1 --nocapture

use std::process::Command;
use std::time::Instant;

use chainsim_aptos::state::{new_account_bytes, to_hex};
use chainsim_aptos::{data_volume, AptosAdapter, API_KEY_ENV};
use chainsim_core::fork::{FundRequest, SendRequest, SetKind, SetRequest, TimeRequest, TokenAmount, UpRequest};
use chainsim_core::profile;
use chainsim_core::{Adapter, ErrorCode};
use serde_json::{json, Value};

const USDC: &str = "0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b";
const HOLDER: &str = "0xbe13ea18241ed83ada91e8f5ca37b5507b397cb66ae1e52151cf32a96547600b";

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().expect("docker");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn inspect(container: &str, format: &str) -> String {
    docker(&["inspect", container, "--format", format])
}

fn assert_gone(name: &str) {
    let label = format!("label=org.chainsim.fork={name}");
    assert_eq!(docker(&["ps", "-a", "--filter", &label, "--format", "{{.Names}}"]), "", "containers left for {name}");
    assert_eq!(docker(&["volume", "ls", "--filter", &label, "--format", "{{.Name}}"]), "", "volumes left for {name}");
    assert_eq!(docker(&["network", "ls", "--filter", &label, "--format", "{{.Name}}"]), "", "networks left for {name}");
}

fn fresh_address() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    format!("0x{:048x}{:016x}", 0x5e1f_7e57u64, nanos)
}

async fn usdc(a: &AptosAdapter, name: &str, who: &str) -> u64 {
    let v = a.call(name, "view", json!(["0x1::primary_fungible_store::balance<0x1::fungible_asset::Metadata>", [format!("address:{who}"), format!("address:{USDC}")]]), None).await.unwrap();
    v[0].as_str().unwrap().parse().unwrap()
}

async fn apt(a: &AptosAdapter, name: &str, who: &str) -> u64 {
    let v = a.call(name, "view", json!(["0x1::coin::balance<0x1::aptos_coin::AptosCoin>", [format!("address:{who}")]]), None).await.unwrap();
    v[0].as_str().unwrap().parse().unwrap()
}

async fn send_usdc(a: &AptosAdapter, name: &str, to: &str, amount: u64) -> Value {
    let req = SendRequest {
        from: HOLDER.into(),
        to: "0x1::primary_fungible_store::transfer<0x1::fungible_asset::Metadata>".into(),
        args: vec![format!("address:{USDC}"), format!("address:{to}"), format!("u64:{amount}")],
        ..Default::default()
    };
    a.send(name, &req).await.unwrap()
}

#[tokio::test]
#[ignore]
async fn aptos_mainnet() {
    let a = AptosAdapter::connect().await.unwrap();
    let p = profile::get("aptos-mainnet").unwrap();
    let name = "acc-aptos";
    a.down(name).await.unwrap();

    // up: rpcUrl null, experimental, the profile limits, the fork version.
    let t = Instant::now();
    let fork = a.up(&p, &UpRequest { name: name.into(), ..Default::default() }).await.unwrap();
    eprintln!("up: {:.1} s", t.elapsed().as_secs_f64());
    let f = serde_json::to_value(&fork).unwrap();
    assert!(f["rpcUrl"].is_null(), "{f}");
    assert_eq!(f["maturity"], "experimental");
    assert_eq!(f["chainId"], "aptos:1");
    assert_eq!(f["forkPoint"]["kind"], "version");
    assert_eq!(f["limits"], json!(p.limits));
    let version = fork.fork_point.number;
    assert!(version > 7_000_000_000, "{version}");

    // The engine and sidecar run hardened, as non-root, with no URL in their arguments.
    for c in [format!("chainsim-{name}"), format!("chainsim-{name}-control")] {
        assert_eq!(inspect(&c, "{{.HostConfig.ReadonlyRootfs}}"), "true", "{c}");
        assert_eq!(inspect(&c, "{{json .HostConfig.CapDrop}}"), "[\"ALL\"]", "{c}");
        assert!(!inspect(&c, "{{json .Args}}").contains("https://"), "{c}: upstream URL in args");
        let user = inspect(&c, "{{.Config.User}}");
        assert!(!user.is_empty() && user != "root" && !user.starts_with("0:"), "{c} user {user:?}");
    }
    assert!(inspect(&format!("chainsim-{name}"), "{{json .HostConfig.Binds}}").contains(&format!("{}:/data", data_volume(name))));

    // up again returns the running fork as is.
    let again = a.up(&p, &UpRequest { name: name.into(), ..Default::default() }).await.unwrap();
    assert_eq!(again.fork_point.number, version);

    let t = Instant::now();
    let report = a.selftest(name).await.unwrap_or_else(|e| panic!("selftest: {e:?} {}", e.data.clone().unwrap_or_default()));
    eprintln!("selftest: {:.1} s {}", t.elapsed().as_secs_f64(), serde_json::to_string(&report).unwrap());
    assert!(report.ok);

    // fund 10 APT and 1000 USDC to a fresh address, read back.
    let me = fresh_address();
    let t = Instant::now();
    let funded = a
        .fund(name, &FundRequest { address: me.clone(), native: Some("10".into()), tokens: vec![TokenAmount { token: "USDC".into(), amount: "1000".into() }] })
        .await
        .unwrap();
    eprintln!("fund: {:.1} s", t.elapsed().as_secs_f64());
    let b = serde_json::to_value(&funded).unwrap();
    assert_eq!(b["balances"][0]["raw"], "1000000000");
    assert_eq!(b["balances"][1]["raw"], "1000000000");
    assert_eq!(apt(&a, name, &me).await, 1_000_000_000);
    assert_eq!(usdc(&a, name, &me).await, 1_000_000_000);
    // Lowering is refused, and the holder itself is not a fund target.
    let lower = a.fund(name, &FundRequest { address: me.clone(), native: Some("1".into()), ..Default::default() }).await.unwrap_err();
    assert_eq!(lower.code, ErrorCode::Unsupported);
    let holder = a.fund(name, &FundRequest { address: HOLDER.into(), native: Some("1".into()), ..Default::default() }).await.unwrap_err();
    assert_eq!(holder.code, ErrorCode::Usage);
    // An account that has sent mainnet transactions is refused (exit 7): this
    // one had sent 1,574,022 transactions on 2026-11-01.
    let used = a.fund(name, &FundRequest { address: "0x44fe92948e90e63c888726f233e0003bcdc0a34941c6b3a52deed59b8c42d726".into(), native: Some("1".into()), ..Default::default() }).await.unwrap_err();
    assert_eq!(used.code, ErrorCode::MainnetAddress, "{used:?}");

    // send --as the USDC holder: a USDC transfer to the test address.
    let t = Instant::now();
    let sent = send_usdc(&a, name, &me, 5_000_000).await;
    eprintln!("send --as: {:.1} s {sent}", t.elapsed().as_secs_f64());
    assert_eq!(sent["status"], "success");
    assert!(sent["hash"].as_str().unwrap().starts_with("0x"));
    assert_eq!(usdc(&a, name, &me).await, 1_005_000_000);

    // set resource: a new Account resource at a fresh address, read back.
    let other = fresh_address();
    let key = [9u8; 32];
    let bcs = to_hex(&new_account_bytes(&other, &key).unwrap());
    let set = a.set(name, &SetRequest { kind: SetKind::Resource { struct_tag: "0x1::account::Account".into(), bcs }, address: other.clone(), value: String::new() }).await.unwrap();
    assert_eq!(set["value"]["authentication_key"], format!("0x{}", "09".repeat(32)), "{set}");
    assert_eq!(set["value"]["sequence_number"], "0");
    // A member of a resource group does not take a plain write.
    let group = a.set(name, &SetRequest { kind: SetKind::Resource { struct_tag: "0x1::fungible_asset::Metadata".into(), bcs: "00".into() }, address: USDC.into(), value: String::new() }).await.unwrap_err();
    assert_eq!(group.code, ErrorCode::ReadbackMismatch, "{group:?}");

    // snapshot, a transfer, revert: the balances come back.
    let before = (usdc(&a, name, &me).await, usdc(&a, name, HOLDER).await);
    let snap = a.snapshot(name).await.unwrap();
    send_usdc(&a, name, &me, 1_000_000).await;
    assert_eq!(usdc(&a, name, &me).await, before.0 + 1_000_000);
    let t = Instant::now();
    let reverted = a.revert(name, &snap.id).await.unwrap();
    eprintln!("revert: {:.1} s", t.elapsed().as_secs_f64());
    assert_eq!((usdc(&a, name, &me).await, usdc(&a, name, HOLDER).await), before);
    assert_eq!(a.revert(name, &snap.id).await.unwrap_err().code, ErrorCode::SnapshotNotFound);
    assert!(a.revert(name, &reverted.snapshot).await.is_ok());

    // time moves the on-chain timestamp, mine adds blocks.
    let moved = a.time(name, &TimeRequest::Advance(3600)).await.unwrap();
    assert_eq!(moved["timestamp"].as_u64().unwrap(), moved["previous"].as_u64().unwrap() + 3600, "{moved}");
    let back = a.time(name, &TimeRequest::To(1)).await.unwrap_err();
    assert_eq!(back.code, ErrorCode::Usage);
    let height = a.status(name).await.unwrap().head;
    let mined = a.mine(name, Some(2)).await.unwrap();
    assert_eq!(mined["height"].as_u64().unwrap(), height + 2, "{mined}");

    // call: ledger and resource; unsupported kinds exit 3.
    let ledger = a.call(name, "ledger", json!([]), None).await.unwrap();
    assert_eq!((ledger["chainId"].as_u64(), ledger["forkVersion"].as_u64()), (Some(1), Some(version)));
    let chain = a.call(name, "resource", json!(["0x1", "0x1::chain_id::ChainId"]), None).await.unwrap();
    assert_eq!(chain["id"], 1);
    let storage = a.set(name, &SetRequest { kind: SetKind::Storage { slot: "0".into() }, address: me.clone(), value: "0".into() }).await.unwrap_err();
    assert_eq!(storage.code.exit_code(), 3);

    // Every send --as and set is in the modification log.
    let mods = chainsim_core::mods::list(&chainsim_core::docker::Docker::connect().await.unwrap(), name).await.unwrap();
    let kinds: Vec<&str> = mods.iter().filter_map(|m| m["kind"].as_str()).collect();
    assert!(kinds.contains(&"send-as") && kinds.contains(&"set-resource"), "{kinds:?}");

    // A pruned version exits 5 with archive advice and leaves nothing behind.
    let old = a.up(&p, &UpRequest { name: "acc-aptos-old".into(), block: Some(1000), ..Default::default() }).await.unwrap_err();
    assert_eq!(old.code.exit_code(), 5, "{old:?}");
    assert!(old.next.as_deref().unwrap_or("").contains("archive"), "{old:?}");
    assert_gone("acc-aptos-old");

    // down leaves no container, volume or network.
    assert!(a.down(name).await.unwrap().removed);
    assert_gone(name);
}

/// A provider URL and API key never reach the output: an unreachable
/// provider host exits 5, naming the variable, not the URL, host or key.
#[tokio::test]
#[ignore]
async fn provider_url_and_key_stay_secret() {
    let p = profile::get("aptos-mainnet").unwrap();
    let url = "https://key123.chainsim-unreachable.invalid/v1/SECRETPATH";
    let key = "aptos-key-SECRET-456";
    // SAFETY: the acceptance tests run with --test-threads=1.
    std::env::set_var(&p.upstream.env, url);
    std::env::set_var(API_KEY_ENV, key);
    let a = AptosAdapter::connect().await.unwrap();
    let name = "acc-aptos-secret";
    a.down(name).await.unwrap();
    let e = a.up(&p, &UpRequest { name: name.into(), ..Default::default() }).await.unwrap_err();
    std::env::remove_var(&p.upstream.env);
    std::env::remove_var(API_KEY_ENV);
    let text = serde_json::to_string(&chainsim_core::envelope::err(&e)).unwrap();
    eprintln!("{text}");
    assert_eq!(e.code.exit_code(), 5, "{text}");
    for secret in ["key123", "SECRETPATH", "SECRET-456", "chainsim-unreachable"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert!(text.contains(&p.upstream.env), "{text}");
    assert_gone(name);
}
