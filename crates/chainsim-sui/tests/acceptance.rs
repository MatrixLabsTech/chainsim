//! Phase 1b acceptance tests (design section 12). They start a real sui-fork
//! of Sui Mainnet in Docker, so they are ignored by default:
//!
//!     cargo test -p chainsim-sui --test acceptance -- --ignored --test-threads=1 --nocapture

use std::process::Command;
use std::time::Instant;

use chainsim_core::fork::{FundRequest, UpRequest};
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

/// The engine and sidecar run hardened; the engine runs as root only
/// because its data volume is root-owned (a profile limit).
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
    assert_ne!(inspect(&format!("chainsim-{name}-control"), "{{.Config.User}}"), "", "the sidecar runs as non-root");
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
