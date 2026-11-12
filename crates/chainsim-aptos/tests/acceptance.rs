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

/// The Node image the SDK script runs in.
const NODE: &str = "node:22.23.5-bookworm-slim";

/// Runs `examples/aptos/sdk.mjs` in a Node container on the fork's network,
/// with the fork's rpcUrl as the SDK's fullnode, and returns its output. The
/// files are copied in, so this also works when the test itself runs in a container.
fn sdk(container: &str, env: &[(&str, &str)], arg: Option<&str>) -> String {
    let mut args: Vec<String> = vec!["exec".into()];
    for (k, v) in env {
        args.extend(["-e".to_string(), format!("{k}={v}")]);
    }
    args.extend([container.to_string(), "node".into(), "/app/sdk.mjs".into()]);
    args.extend(arg.map(str::to_string));
    let out = Command::new("docker").args(&args).output().expect("docker");
    assert!(out.status.success(), "sdk.mjs failed: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn start_node(container: &str, network: &str) {
    docker(&["rm", "-f", container]);
    let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/aptos");
    assert!(!docker(&["run", "-d", "--name", container, "--network", network, "--workdir", "/app", NODE, "sleep", "infinity"]).is_empty(), "the Node container did not start");
    for file in ["package.json", "yarn.lock", "sdk.mjs"] {
        let copied = Command::new("docker").args(["cp", &format!("{example}/{file}"), &format!("{container}:/app/{file}")]).status().expect("docker");
        assert!(copied.success(), "docker cp {file}");
    }
    let installed = Command::new("docker").args(["exec", container, "yarn", "install", "--frozen-lockfile", "--silent"]).output().expect("docker");
    assert!(installed.status.success(), "yarn install: {}", String::from_utf8_lossy(&installed.stderr));
}

/// One request to the upstream from the control sidecar, at a ledger version.
/// The URL and key stay in the sidecar's environment.
fn mainnet(name: &str, path: &str, version: u64, body: Option<&Value>) -> Value {
    let post = body.map(|b| format!("-H 'content-type: application/json' -d '{b}'")).unwrap_or_default();
    let script = format!("curl -sS --max-time 30 ${{CHAINSIM_API_KEY:+-H \"Authorization: Bearer $CHAINSIM_API_KEY\"}} {post} \"${{CHAINSIM_UPSTREAM%/}}{path}?ledger_version={version}\"");
    let out = docker(&["exec", &format!("chainsim-{name}-control"), "sh", "-c", &script]);
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("mainnet {path}: {e}"))
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
    // A Node container left by a failed run holds the fork's network.
    let node = "acc-aptos-node";
    docker(&["rm", "-f", node]);
    a.down(name).await.unwrap();

    // up --publish: the REST endpoint as rpcUrl and, on this first up, as hostRpcUrl,
    // experimental, the profile limits, the fork version.
    let t = Instant::now();
    let fork = a.up(&p, &UpRequest { name: name.into(), publish: true, ..Default::default() }).await.unwrap();
    eprintln!("up: {:.1} s", t.elapsed().as_secs_f64());
    let f = serde_json::to_value(&fork).unwrap();
    assert_eq!(f["rpcUrl"], format!("http://{name}:8080"), "{f}");
    assert!(f["hostRpcUrl"].as_str().is_some_and(|u| u.starts_with("http://127.0.0.1:")), "{f}");
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

    // The Aptos TypeScript SDK, unchanged, with the fork as its fullnode: a mainnet
    // account's APT balance and Account resource and a view equal mainnet's at
    // the version the session reads, the one before the fork version. It runs before
    // anything else touches the copy.
    start_node(node, &fork.network);
    let fullnode = format!("{}/v1", fork.rpc_url.as_deref().unwrap());
    let key = format!("0x{}", to_hex(&chainsim_aptos::state::ed25519_auth_key(fresh_address().as_bytes())));
    let env = [("FULLNODE", fullnode.as_str()), ("READ_ACCOUNT", HOLDER), ("SENDER_KEY", key.as_str())];
    let sender = sdk(node, &env, Some("address"));
    a.fund(name, &FundRequest { address: sender.clone(), native: Some("5".into()), ..Default::default() }).await.unwrap();
    let t = Instant::now();
    let text = sdk(node, &env, None);
    eprintln!("sdk.mjs: {:.1} s {text}", t.elapsed().as_secs_f64());
    let out: Value = serde_json::from_str(&text[text.find('{').expect("sdk.mjs prints JSON")..]).unwrap();
    let read_at = version - 1;
    let balance = mainnet(name, "/view", read_at, Some(&json!({ "function": "0x1::coin::balance", "type_arguments": ["0x1::aptos_coin::AptosCoin"], "arguments": [HOLDER] })));
    assert_eq!(out["reads"]["aptBalance"].as_u64(), balance[0].as_str().and_then(|b| b.parse().ok()), "{balance}");
    assert!(out["reads"]["aptBalance"].as_u64().unwrap() > 0);
    assert_eq!(out["reads"]["resource"], mainnet(name, &format!("/accounts/{HOLDER}/resource/0x1::account::Account"), read_at, None)["data"]);
    assert_eq!(out["reads"]["view"], mainnet(name, "/view", read_at, Some(&json!({ "function": "0x1::coin::supply", "type_arguments": ["0x1::aptos_coin::AptosCoin"], "arguments": [] }))));
    assert_eq!(out["reads"]["chainId"], 1);
    // The same script simulated, signed and submitted a 0.1 APT transfer and waited
    // for it: the balance change reads back through the endpoint and through the CLI.
    let receiver = format!("0x{:0>64}", "c5e2");
    assert_eq!(out["simulate"]["success"], true, "{out}");
    assert_eq!((out["transfer"]["success"].clone(), out["transfer"]["hashMatchesSdk"].clone()), (json!(true), json!(true)), "{out}");
    assert_eq!(out["transfer"]["moved"], 10_000_000, "{out}");
    assert_eq!(out["transfer"]["gasUsed"], out["simulate"]["gasUsed"], "{out}");
    assert!(out["transfer"]["events"].as_array().unwrap().iter().any(|e| e == "0x1::fungible_asset::Deposit"), "{out}");
    assert_eq!(apt(&a, name, &receiver).await, out["transfer"]["receiverAfter"].as_u64().unwrap());
    // A second run is the sender's next sequence number, and the endpoint's log holds no secret.
    let again: Value = { let t = sdk(node, &env, None); serde_json::from_str(&t[t.find('{').unwrap()..]).unwrap() };
    assert_eq!(again["transfer"]["receiverAfter"].as_u64(), out["transfer"]["receiverAfter"].as_u64().map(|b| b + 10_000_000), "{again}");
    if let Ok(secret) = std::env::var(API_KEY_ENV) {
        let log = Command::new("docker").args(["logs", &format!("chainsim-{name}")]).output().expect("docker");
        assert!(!format!("{}{}", String::from_utf8_lossy(&log.stdout), String::from_utf8_lossy(&log.stderr)).contains(&secret), "the API key is in the engine log");
    }
    docker(&["rm", "-f", node]);

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
    assert_eq!(group.code, ErrorCode::Unsupported, "{group:?}");
    // Bytes that do not decode leave the copy as it was.
    let bad = a.set(name, &SetRequest { kind: SetKind::Resource { struct_tag: "0x1::account::Account".into(), bcs: "00".into() }, address: other.clone(), value: String::new() }).await.unwrap_err();
    assert_eq!(bad.code, ErrorCode::Usage, "{bad:?}");
    assert_eq!(a.call(name, "resource", json!([other, "0x1::account::Account"]), None).await.unwrap()["authentication_key"], format!("0x{}", "09".repeat(32)));

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
