//! Phase 1b acceptance tests (design section 12). They start a real sui-fork
//! of Sui Mainnet in Docker, so they are ignored by default:
//!
//!     cargo test -p chainsim-sui --test acceptance -- --ignored --test-threads=1 --nocapture

use std::process::Command;
use std::time::Instant;

use chainsim_core::fork::{FundRequest, SendRequest, SetKind, SetRequest, UpRequest};
use chainsim_core::profile;
use chainsim_core::{Adapter, ErrorCode};
use chainsim_sui::{data_volume, grpc, SuiAdapter, FORKING_PROTO};
use serde_json::{json, Value};

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

async fn balance(a: &SuiAdapter, name: &str, owner: &str) -> u64 {
    let v = a.call(name, "StateService/GetBalance", json!({ "owner": owner, "coinType": "0x2::sui::SUI" }), None).await.unwrap();
    v["balance"]["balance"].as_str().unwrap().parse().unwrap()
}

/// The engine and sidecar run hardened and as non-root users.
fn check_hardening(name: &str, published: bool) {
    for c in [format!("chainsim-{name}"), format!("chainsim-{name}-control")] {
        assert_eq!(inspect(&c, "{{.HostConfig.ReadonlyRootfs}}"), "true", "{c}");
        assert_eq!(inspect(&c, "{{json .HostConfig.CapDrop}}"), "[\"ALL\"]", "{c}");
        assert_eq!(inspect(&c, "{{json .HostConfig.SecurityOpt}}"), "[\"no-new-privileges\"]", "{c}");
        assert!(inspect(&c, "{{json .HostConfig.Tmpfs}}").contains("/tmp"), "{c}");
        assert!(!inspect(&c, "{{json .Args}}").contains("https://"), "{c}: upstream URL in args");
    }
    let engine = format!("chainsim-{name}");
    assert_eq!(inspect(&engine, "{{.HostConfig.Memory}}"), (1u64 << 30).to_string());
    assert!(inspect(&engine, "{{json .HostConfig.Binds}}").contains(&format!("{}:/data", data_volume(name))));
    for c in [engine.clone(), format!("chainsim-{name}-control")] {
        let user = inspect(&c, "{{.Config.User}}");
        assert!(!user.is_empty() && user != "root" && !user.starts_with("0:") && user != "0", "{c} user {user:?}");
    }
    let ports = inspect(&engine, "{{json .NetworkSettings.Ports}}");
    let bindings = inspect(&engine, "{{json .HostConfig.PortBindings}}");
    if published {
        assert!(ports.contains("\"HostIp\":\"127.0.0.1\""), "{ports}");
        assert!(!ports.contains("0.0.0.0") && !ports.contains("\"::\""), "{ports}");
    } else {
        assert!(bindings == "{}" || bindings == "null", "bindings {bindings}");
        assert!(!ports.contains("HostPort"), "ports {ports}");
    }
    let sidecar = inspect(&format!("chainsim-{name}-control"), "{{json .HostConfig.PortBindings}}");
    assert!(sidecar == "{}" || sidecar == "null", "sidecar bindings {sidecar}");
}

#[tokio::test]
#[ignore]
async fn sui_mainnet() {
    let a = SuiAdapter::connect().await.unwrap();
    let p = profile::get("sui-mainnet").unwrap();
    let name = "acc-sui";
    a.down(name).await.unwrap();

    // up, then the fork object carries experimental and the profile limits.
    let t = Instant::now();
    let req = UpRequest { name: name.into(), ..Default::default() };
    let fork = a.up(&p, &req).await.unwrap();
    println!("sui up in {} ms at checkpoint {}", t.elapsed().as_millis(), fork.fork_point.number);
    let fv = serde_json::to_value(&fork).unwrap();
    assert_eq!(fv["maturity"], "experimental");
    assert_eq!(fv["chainId"], "sui:mainnet");
    assert_eq!(fv["limits"], json!(p.limits));
    assert_eq!(fv["hostRpcUrl"], Value::Null);
    // A second up with no block returns the same fork.
    assert_eq!(a.up(&p, &req).await.unwrap().fork_point, fork.fork_point);
    check_hardening(name, false);

    // Readiness came from gRPC health, which still answers.
    let handle = a.docker().find(name).await.unwrap().unwrap();
    let g = a.grpc(&handle, &p);
    assert!(g.healthy().await);

    // The vendored ForkingService proto matches the live fork's reflection.
    assert_eq!(g.describe_rpcs("sui.forking.v1alpha.ForkingService").await.unwrap(), grpc::rpc_names(FORKING_PROTO));

    // selftest: gRPC health, GetStatus, a funded transfer by impersonation.
    let st = a.selftest(name).await.unwrap();
    println!("selftest: {}", serde_json::to_string(&st).unwrap());
    let parts: Vec<_> = st.parts.iter().map(|c| c.part).collect();
    assert_eq!(parts, ["grpc-health", "status", "clock", "funded-transfer"]);

    // call reads the Clock 0x6 from the host, with no published port.
    let clock = a.call(name, "LedgerService/GetObject", json!({ "objectId": "0x6" }), None).await.unwrap();
    assert!(clock["object"]["objectId"].as_str().unwrap().ends_with("06"), "{clock}");

    // fund returns the read-back SUI balance of a fresh address, and is idempotent.
    let who = fresh_address();
    let fund = FundRequest { address: who.clone(), native: Some("25".into()), tokens: vec![] };
    let t = Instant::now();
    let funded = a.fund(name, &fund).await.unwrap();
    println!("fund in {} ms", t.elapsed().as_millis());
    assert_eq!(funded.balances[0].raw, "25000000000");
    assert_eq!(a.fund(name, &fund).await.unwrap().balances[0].raw, "25000000000");
    // An address that owns objects on mainnet is refused (exit 7).
    let rich = FundRequest { address: p.funder.clone().unwrap(), native: Some("1".into()), tokens: vec![] };
    assert_eq!(a.fund(name, &rich).await.unwrap_err().code, ErrorCode::MainnetAddress);

    // snapshot, a transfer, then revert restores the balance.
    let t = Instant::now();
    let snap = a.snapshot(name).await.unwrap();
    println!("snapshot in {} ms", t.elapsed().as_millis());
    a.fund(name, &FundRequest { native: Some("30".into()), ..fund.clone() }).await.unwrap();
    assert_eq!(balance(&a, name, &who).await, 30_000_000_000);
    let t = Instant::now();
    let rev = a.revert(name, &snap.id).await.unwrap();
    println!("revert in {} ms", t.elapsed().as_millis());
    assert_eq!(balance(&a, name, &who).await, 25_000_000_000);
    // The id is consumed; the fresh one works.
    assert_eq!(a.revert(name, &snap.id).await.unwrap_err().code, ErrorCode::SnapshotNotFound);
    a.revert(name, &rev.snapshot).await.unwrap();

    // down leaves nothing behind.
    assert!(a.down(name).await.unwrap().removed);
    assert_gone(name);
    assert!(!a.down(name).await.unwrap().removed);
}

#[tokio::test]
#[ignore]
async fn sui_mainnet_published() {
    let a = SuiAdapter::connect().await.unwrap();
    let p = profile::get("sui-mainnet").unwrap();
    let name = "acc-sui-pub";
    a.down(name).await.unwrap();
    let fork = a.up(&p, &UpRequest { name: name.into(), publish: true, ..Default::default() }).await.unwrap();
    let url = fork.host_rpc_url.clone().unwrap();
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    check_hardening(name, true);
    a.down(name).await.unwrap();
    assert_gone(name);
}

/// send --as and set on objects that live on Sui Mainnet (design section 9,
/// phase 2 Sui slice). The objects were checked on 2026-10-11: if a cap has
/// moved on mainnet since, update the constants.
mod mainnet_objects {
    pub const VAULT: &str = "0x0000c7fa394e8abac7eb529e06bb2d90d11a775ee23db1b115a170f062b622f3::vault::VAULT";
    pub const VAULT_CAP: &str = "0xef6752ebbb43af90e8abc41f763aeb5f2fdd7981072e403b2c3ffba4e266edfa";
    pub const VAULT_CAP_OWNER: &str = "0xcf5455078123a7f42f98f237301f6def64a077af7f6bdb8ef57a7f301367209c";
    pub const CETUS: &str = "0x260693ec785a6e6c9d81d58c7d2ff72f1288ae0fa6a9725abe05a6478b11f084";
    pub const CETUS_CONFIG: &str = "0xdaa46292632c3c4d8f31f23ea0f9b36a28ff3677e9684980e4438403a67a3d8f";
    pub const CETUS_ADMIN_CAP: &str = "0x89c1a321291d15ddae5a086c9abc533dff697fde3d89e0ca836c41af73e36a75";
    pub const CETUS_ADMIN: &str = "0xdbfd0b17fa804c98f51d552b050fb7f850b85db96fa2a0d79e50119525814a47";
    /// A DeepBook airdrop object: its type has key but not store.
    pub const KEY_ONLY: &str = "0x253d4d235e6a00968c90d7ad79dc5e029dc9630802e09e15836f20bb51f61cca";
}

#[tokio::test]
#[ignore]
async fn sui_send_and_set() {
    use mainnet_objects::*;
    let a = SuiAdapter::connect().await.unwrap();
    let p = profile::get("sui-mainnet").unwrap();
    let name = "acc-sui-send";
    a.down(name).await.unwrap();
    a.up(&p, &UpRequest { name: name.into(), ..Default::default() }).await.unwrap();
    let send = |from: &str, to: String, args: &[&str]| SendRequest { from: from.into(), to, args: args.iter().map(|s| s.to_string()).collect(), ..Default::default() };
    let member = fresh_address();

    // Mint as the TreasuryCap owner, with no key.
    let out = a.send(name, &send(VAULT_CAP_OWNER, format!("0x2::coin::mint_and_transfer<{VAULT}>"), &[VAULT_CAP, "1000", &member])).await.unwrap();
    assert_eq!(out["status"], 1, "{out}");
    assert_eq!(out["created"].as_array().unwrap().len(), 1, "{out}");

    // A role-gated call from an address without the role aborts with its location and code.
    let fee = format!("{CETUS}::config::update_protocol_fee_rate");
    let e = a.send(name, &send(&member, fee.clone(), &[CETUS_CONFIG, "1500"])).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::TxReverted);
    assert_eq!(e.data.as_ref().unwrap()["error"]["abort"]["function"], "check_pool_manager_role");
    // The admin grants every role; then the same call succeeds.
    let roles = u128::MAX.to_string();
    let out = a.send(name, &send(CETUS_ADMIN, format!("{CETUS}::config::set_roles"), &[CETUS_ADMIN_CAP, CETUS_CONFIG, &member, &roles])).await.unwrap();
    assert_eq!(out["status"], 1, "{out}");
    assert_eq!(a.send(name, &send(&member, fee, &[CETUS_CONFIG, "1500"])).await.unwrap()["status"], 1);

    // set balance up and down, set token by mint and back down, set owner.
    let set = |kind, address: &str, value: &str| SetRequest { kind, address: address.into(), value: value.into() };
    assert_eq!(a.set(name, &set(SetKind::Balance, &member, "12.5")).await.unwrap()["raw"], "12500000000");
    assert_eq!(a.set(name, &set(SetKind::Balance, &member, "2")).await.unwrap()["raw"], "2000000000");
    let token = || SetKind::Token { coin_type: VAULT.into(), from: None };
    assert_eq!(a.set(name, &set(token(), &member, "5000")).await.unwrap()["raw"], "5000");
    assert_eq!(a.set(name, &set(token(), &member, "3000")).await.unwrap()["raw"], "3000");
    let receiver = fresh_address();
    assert_eq!(a.set(name, &set(SetKind::Owner, CETUS_ADMIN_CAP, &receiver)).await.unwrap()["owner"], receiver);
    // Objects that cannot move exit 3: key without store, shared, immutable.
    for object in [KEY_ONLY, CETUS_CONFIG, "0x2"] {
        assert_eq!(a.set(name, &set(SetKind::Owner, object, &receiver)).await.unwrap_err().code, ErrorCode::Unsupported, "{object}");
    }

    // Every send and set is in the modification log: 4 sends (one aborted), 2 balances, 2 tokens, 1 owner.
    let mods = chainsim_core::mods::list(a.docker(), name).await.unwrap();
    assert_eq!(mods.iter().filter(|m| m["kind"] == "send-as").count(), 4, "{mods:?}");
    assert_eq!(mods.len(), 9, "{mods:?}");

    assert!(a.down(name).await.unwrap().removed);
    assert_gone(name);
}

/// The u64 at `offset` in base64 BCS bytes.
fn u64_at(b64: &str, offset: usize) -> u64 {
    let bytes = chainsim_sui::units::unbase64(b64).unwrap();
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// set object on the patched engine: a field override that execution sees,
/// the refusals, and the modification log.
#[tokio::test]
#[ignore]
async fn sui_set_object() {
    use mainnet_objects::*;
    let a = SuiAdapter::connect().await.unwrap();
    let p = profile::get("sui-mainnet").unwrap();
    let name = "acc-sui-object";
    a.down(name).await.unwrap();
    a.up(&p, &UpRequest { name: name.into(), ..Default::default() }).await.unwrap();
    let object = |id: &str, fields: &[&str], bcs: Option<String>| SetRequest {
        kind: SetKind::Object { fields: fields.iter().map(|s| s.to_string()).collect(), bcs },
        address: id.into(),
        value: String::new(),
    };

    // The fee reads back from the fork after the set.
    let out = a.set(name, &object(CETUS_CONFIG, &["protocol_fee_rate=1234"], None)).await.unwrap();
    assert_eq!(out["fields"]["protocol_fee_rate"], "1234", "{out}");
    assert_eq!(out["changed"], json!(["protocol_fee_rate"]));
    assert_eq!(out["fields"]["id"], CETUS_CONFIG);

    // A role-gated call sees it: update_protocol_fee_rate emits the old rate.
    let member = fresh_address();
    let roles = u128::MAX.to_string();
    let grant = SendRequest { from: CETUS_ADMIN.into(), to: format!("{CETUS}::config::set_roles"), args: vec![CETUS_ADMIN_CAP.into(), CETUS_CONFIG.into(), member.clone(), roles], ..Default::default() };
    assert_eq!(a.send(name, &grant).await.unwrap()["status"], 1);
    let funder = p.funder.clone().unwrap();
    let coins = a.call(name, "StateService/ListOwnedObjects", json!({ "owner": funder, "objectType": "0x2::coin::Coin<0x2::sui::SUI>", "pageSize": 1, "readMask": { "paths": ["object_id", "version", "digest"] } }), None).await.unwrap();
    let coin = &coins["objects"][0];
    let shared = a.call(name, "LedgerService/GetObject", json!({ "objectId": CETUS_CONFIG, "readMask": { "paths": ["owner"] } }), None).await.unwrap();
    let tx = json!({
        "transaction": {
            "version": 1, "sender": member,
            "kind": { "kind": "PROGRAMMABLE_TRANSACTION", "programmableTransaction": {
                "inputs": [
                    { "kind": "SHARED", "objectId": CETUS_CONFIG, "version": shared["object"]["owner"]["version"], "mutable": true },
                    { "kind": "PURE", "pure": chainsim_sui::units::base64(&1500u64.to_le_bytes()) },
                ],
                "commands": [{ "moveCall": { "package": CETUS, "module": "config", "function": "update_protocol_fee_rate", "arguments": [{ "kind": "INPUT", "input": 0 }, { "kind": "INPUT", "input": 1 }] } }],
            } },
            "gasPayment": { "objects": [{ "objectId": coin["objectId"], "version": coin["version"], "digest": coin["digest"] }], "owner": funder, "price": "1000", "budget": "500000000" },
            "expiration": { "kind": "NONE" },
        },
        "signatures": [],
        "readMask": { "paths": ["effects.status", "events"] },
    });
    let done = a.call(name, "TransactionExecutionService/ExecuteTransaction", tx, None).await.unwrap();
    assert_eq!(done["transaction"]["effects"]["status"]["success"], true, "{done}");
    let event = &done["transaction"]["events"]["events"][0];
    assert!(event["eventType"].as_str().unwrap().ends_with("::config::UpdateFeeRateEvent"), "{event}");
    let bytes = event["contents"]["value"].as_str().unwrap();
    assert_eq!((u64_at(bytes, 0), u64_at(bytes, 8)), (1234, 1500), "old and new fee rate");

    // Nested fields by dotted path, and the package version gate sees an override.
    let out = a.set(name, &object(CETUS_CONFIG, &["fee_tiers.contents.0.value.fee_rate=111", "package_version=999"], None)).await.unwrap();
    assert_eq!(out["fields"]["fee_tiers"]["contents"][0]["value"]["fee_rate"], "111", "{out}");
    let e = a.send(name, &grant).await.unwrap_err();
    assert_eq!(e.data.as_ref().unwrap()["error"]["abort"]["function"], "checked_package_version");

    // --bcs takes the whole contents: the current bytes go back unchanged.
    let current = a.call(name, "LedgerService/GetObject", json!({ "objectId": CETUS_CONFIG, "readMask": { "paths": ["contents"] } }), None).await.unwrap();
    let hex: String = chainsim_sui::units::unbase64(current["object"]["contents"]["value"].as_str().unwrap()).unwrap().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(a.set(name, &object(CETUS_CONFIG, &[], Some(format!("0x{hex}")))).await.unwrap()["changed"], "bcs");

    // Refusals: a coin and a package exit 3, another object's id and a bad path exit 2.
    let coin_id = coin["objectId"].as_str().unwrap();
    let e = a.set(name, &object(coin_id, &["balance.value=1"], None)).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unsupported);
    assert!(e.next.as_deref().unwrap().contains(" token "), "{:?}", e.next);
    assert_eq!(a.set(name, &object("0x2", &["x=1"], None)).await.unwrap_err().code, ErrorCode::Unsupported);
    let other_id = format!("0x{hex}").replacen(&CETUS_CONFIG[2..], &"11".repeat(32), 1);
    assert_eq!(a.set(name, &object(CETUS_CONFIG, &[], Some(other_id))).await.unwrap_err().code, ErrorCode::Usage);
    assert_eq!(a.set(name, &object(CETUS_CONFIG, &["nope=1"], None)).await.unwrap_err().code, ErrorCode::Usage);

    // Three sets and two sends (one aborted) are in the modification log.
    let mods = chainsim_core::mods::list(a.docker(), name).await.unwrap();
    assert_eq!(mods.iter().filter(|m| m["kind"] == "set-object").count(), 3, "{mods:?}");
    assert_eq!(mods.len(), 5, "{mods:?}");

    assert!(a.down(name).await.unwrap().removed);
    assert_gone(name);
}
